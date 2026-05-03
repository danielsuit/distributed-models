//! Code Writer Agent.
//!
//! Drives a tool-use loop against the workspace: the model emits one JSON
//! tool call per turn (`read_file`, `list_dir`, `grep`, `glob`, `edit`,
//! `create`, `delete`, `finish`). Read-only tools execute against the actual
//! filesystem and stream their output back into the next prompt; mutating
//! tools update an in-memory virtual filesystem. When the model emits
//! `finish`, the staged state is converted into whole-file `FileOperation`s
//! so the orchestrator's existing accept/reject UI keeps working unchanged.
//!
//! Edits use Claude-Code-style search/replace pairs internally — local
//! models handle small targeted patches far more reliably than they handle
//! "rewrite this 800-line file from memory."
//!
//! Legacy fallback: if a model returns the old `{operations:[…],summary}`
//! envelope (or any other shape `parse_code_writer_output` recognises), we
//! consume it directly. That keeps older fixtures and the integration test
//! green.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use crate::agents::AgentRuntime;
use crate::bash;
use crate::job_cancel::JobCancelled;
use crate::messages::{Agent, ClientEvent, CodeWriterResult, FileAction, FileOperation};
use crate::tools::{self, ToolAction, ToolCall, ToolSession};
use crate::workspace_path;

const POLL_TIMEOUT_SECS: f64 = 5.0;
const MAX_LOOP_ITERATIONS: usize = 25;
const MAX_FEEDBACK_CHARS: usize = 6_000;
const MAX_TRANSCRIPT_CHARS: usize = 24_000;
const MAX_PARSE_ERRORS_BEFORE_GIVE_UP: usize = 3;
/// Stop when the agent keeps invoking read-only tools without ever issuing
/// `edit`/`create`/`delete` — e.g. alternating `list_dir(".")` and `list_dir("src")`
/// would never trip the consecutive-duplicate detector.
const READONLY_EXPLORE_STREAK_CAP: usize = 24;
/// Bail out after this many consecutive identical read-only exploration signatures.
const EXPLORE_DUP_STOP_AFTER: usize = 6;
/// How long to wait for the user (or auto-acceptor) to decide on a single
/// mid-loop proposal before treating silence as rejection.
const PROPOSAL_DECISION_TIMEOUT: Duration = Duration::from_secs(60 * 30);
const PROPOSAL_CANCEL_POLL: Duration = Duration::from_millis(250);
/// Align with `file_structure` query cap — surface orchestrator indexer paths in prompts.
const MAX_INDEXER_HINTS_IN_PROMPT: usize = 120;

const SYSTEM_PROMPT: &str = r#"# GOALS
You are a senior software engineer working on a real code repository.
You operate by calling tools — one tool per turn — until the task is complete, then you call `finish`.
There is no external plugin protocol — only the tools listed below exist.

# FORMAT
On every turn, respond with EXACTLY ONE JSON object describing the next tool call.
NO markdown fences, NO commentary outside the JSON, NO multiple objects.
Format: {"thought":"<one short line>","tool":"<name>","args":{ ... }}

Do NOT imitate function-call prose (`grep(\"x\") in \"file\")` — that breaks JSON.
`pattern` carries only the substring/needle; narrowing to one file ALWAYS uses `"path":"relative/file.ext"`.

# JSON EXAMPLES (valid shapes — adapt paths to your workspace)
Whole-workspace substring search:
{"thought":"find stylesheet refs","tool":"grep","args":{"pattern":"stylesheet"}}
One HTML file only:
{"thought":"locate buttons","tool":"grep","args":{"pattern":"<button","path":"index.html"}}
Targeted edit (search must match file bytes exactly):
{"thought":"nicer hero section","tool":"edit","args":{"path":"styles.css","search":"header {","replace":"header {\n  padding: 2rem 1.5rem;\n  letter-spacing: 0.02em;" }}

# TOOLS
The only valid way to call a tool is to return a JSON object. The available tools and their arguments are:
- read_file (args: path, start_line?, end_line?): read a workspace file. ALWAYS read before editing.
- list_dir (args: path): list directory. Empty path = workspace root.
- grep (args: pattern, path?): **case-insensitive substring search**, NOT regex. Prefer short literal needles (`class=`, `.card`, `<nav`, `stylesheet`). `.*`/`[]`/`|` in `pattern` search for those characters literally, not wildcard behavior. Scope with `"path":"index.html"` when you know the file.
- glob (args: pattern): find files matching glob (e.g. `**/*.rs`).
- edit (args: path, search, replace, replace_all?): replace `search` with `replace`. `search` MUST be verbatim (including whitespace). Prefer SHORT, targeted edits.
- create (args: path, content): create a new file.
- delete (args: path): remove a file.
- bash (args: command, timeout?): run shell command.
- semantic_search (args: query, top_k?): vector search across codebase.
- finish (args: summary): end when the TASK is done.


# HTML ↔ CSS COUPLING (STATIC SITES — NON-NEGOTIABLE)
When you create or change a **website** (HTML + external `.css` or `<style>`):
1. **No orphan CSS**: If you add a rule for `.foo`, `#bar`, or `[data-x]`, that selector MUST appear on an element in **some HTML you edit in this job** (add `class="foo"` / `id="bar"` / the attribute). Do not leave styles that nothing in markup can ever match.
2. **Default workflow**: Introduce a **shared class naming scheme** (e.g. `site-nav__link`, `hero`, `btn-primary`) — **edit HTML first** to add those classes on the right tags, then **edit CSS** to style them. Same turn / same session; the user should never need a follow-up to "add class names."
3. **Alternative**: You MAY use **only** selectors that already exist in the file (`header nav a`, `.container h2`) — then do **not** invent unused `.class` blocks elsewhere.
4. **Multi-page**: Shared components (nav, footer, buttons) MUST use the **same class names** on each page that should look identical, with rules defined once in the shared stylesheet.


# GUIDELINES
1. **Paths**: ALL `path` arguments MUST be relative to the Workspace Root. Do NOT use absolute paths like `/Users/...`.
2. **Orient**: Use indexer hints, glob, list_dir, grep. Never invent paths. If a path fails, pivot.
3. **Editing**: ALWAYS `read_file` before `edit`. Prefer `edit` over wholesale rewrites. Add localized chunks.
4. **STYLING & VISUALS (CRITICAL)**:
   - When asked to add styling or make UI look better, DO NOT just resize the `body` or make superficial changes.
   - You MUST generate detailed, working CSS for SPECIFIC elements (buttons, navbars, cards, grid layouts, etc.).
   - Find the exact class names and IDs in the HTML using `grep` and `read_file`, and tie your CSS specifically to them — **or add those classes/ids to HTML** when you introduce new CSS rules (see **HTML ↔ CSS COUPLING** above).
   - DO NOT insert hollow comments like "TODO stylize later" or empty placeholder blocks. You MUST write REAL properties (color, flex, padding, etc.).
   - Checking that `<link rel="stylesheet">` exists does NOT satisfy "look better / more styling". You MUST still `edit` / `append` substantive rules to CSS (or markup) before `finish`.
   - `finish` early with "link already present" on a VISUAL/STYLING request is FAILURE — keep iterating with real selectors from `read_file` until edits land.
   - A styling task is NEVER complete until you have shipped concrete, detailed CSS rules for the components requested.
5. **PREMIUM QUALITY & COMPLEXITY**:
   - DO NOT write "Minimum Viable Product" (MVP) code.
   - When asked to create an app, script, or website, build a comprehensive, production-ready solution.
   - Include robust error handling, modern aesthetics, structured logic, and deep implementation details. Never leave features half-finished.
6. **Recovery**: If a tool errors, do not repeat the failing call verbatim. Pivot and try another approach.
"#;

fn instruction_signals_html_css_or_site_work(ins: &str) -> bool {
    let s = ins.to_ascii_lowercase();
    if s.starts_with("what ")
        || s.starts_with("why ")
        || s.starts_with("how does ")
        || s.starts_with("explain ")
        || s.starts_with("describe ")
        || s.contains("difference between ")
    {
        // Pure Q&A; do not trap the writer for “finish without edits”.
        return false;
    }
    const NEEDLES: &[&str] = &[
        "styling",
        " stylesheet",
        "stylesheet",
        "styles.css",
        ".css",
        "tailwind",
        "navbar",
        "landing",
        "typography",
        "responsive",
        "visual",
        "formatting",
        "formatted",
        "make it look",
        "look good",
        "looks good",
        " prettier",
        "prettier ",
        "nicer",
        "polish",
        "frontend",
        "corresponding styl",
        "elements and ",
    ];
    NEEDLES.iter().any(|needle| s.contains(*needle))
        || (s.contains("website") && !s.starts_with("what "))
        || ((s.contains("html") || s.contains(".css")) && s.contains("page"))
}

fn instruction_demands_site_or_style_edits(ins: &str) -> bool {
    let s = ins.to_ascii_lowercase();
    if !instruction_signals_html_css_or_site_work(&s) {
        return false;
    }
    const ACTION: &[&str] = &[
        "add ",
        "fix ",
        "fixing ",
        "more ",
        "improve ",
        "update ",
        "implement",
        "create ",
        "build ",
        "change ",
        "enhance ",
        "polish",
        "make ",
        "continue ",
        "extend ",
        "expand ",
        "better ",
        "working ",
        "ensure ",
        "need ",
        "want ",
        "more elements",
        "more styling",
        "looks and ",
        "look and ",
    ];
    ACTION.iter().any(|a| s.contains(*a))
}

/// Canonical path key for detecting repeated exploration (`.`, `./`, absolute under workspace → same bucket).
fn explore_path_signature(raw: &str, workspace_root: Option<&Path>) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return "<root>".to_string();
    }
    if let Some(root) = workspace_root {
        if let Ok((key, _)) = tools::safe_relative_path(Some(root), t) {
            return if key.is_empty() {
                "<root>".into()
            } else {
                key
            };
        }
    }
    let normalized = t.replace('\\', "/");
    let stripped = normalized.trim_start_matches("./");
    let trimmed = stripped.trim_end_matches('/');
    let core = if trimmed.is_empty() { "" } else { trimmed };
    if core.is_empty() || core == "." {
        "<root>".into()
    } else {
        core.into()
    }
}

/// Signature for duplicate-read detection; aligns `list_dir` / `read_file` paths that differ cosmetically.
fn readonly_loop_signature(call: &ToolCall, workspace_root: Option<&Path>) -> String {
    match call {
        ToolCall::ListDir { path } => format!("list_dir:{}", explore_path_signature(path, workspace_root)),
        ToolCall::ReadFile {
            path,
            start_line,
            end_line,
        } => format!(
            "read_file:{}|{:?}|{:?}",
            explore_path_signature(path, workspace_root),
            start_line,
            end_line
        ),
        _ => call.label(),
    }
}

pub async fn run(rt: AgentRuntime) {
    tracing::info!("code writer agent online");
    loop {
        match step(&rt).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("code writer agent error: {err:?}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn step(rt: &AgentRuntime) -> Result<()> {
    let Some(message) = rt
        .bus
        .next_message(Agent::CodeWriter.queue(), POLL_TIMEOUT_SECS)
        .await?
    else {
        return Ok(());
    };

    rt.bus
        .publish_event(&ClientEvent::AgentStatus {
            job_id: message.job_id.clone(),
            agent: Agent::CodeWriter,
            status: "exploring repository".into(),
        })
        .await?;

    let task = message
        .context
        .get("instruction")
        .and_then(|v| v.as_str())
        .unwrap_or(&message.task)
        .to_string();
    let workspace_root = message
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let related_files: Vec<String> = message
        .context
        .get("related_files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    let target_file = message
        .context
        .get("target_file")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let semantic_snippets = message
        .context
        .get("semantic_snippets")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let result = match run_tool_loop(
        rt,
        &message.job_id,
        &task,
        &workspace_root,
        &related_files,
        &target_file,
        &semantic_snippets,
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            rt.bus
                .publish_event(&ClientEvent::Error {
                    job_id: message.job_id.clone(),
                    message: format!("Code writer loop failed: {err}"),
                })
                .await?;
            CodeWriterResult::default()
        }
    };

    let response = message
        .reply(message.from, "code_writer_result")
        .with_result(json!(result));
    rt.bus.dispatch(&response).await?;
    Ok(())
}

async fn run_tool_loop(
    rt: &AgentRuntime,
    job_id: &str,
    instruction: &str,
    workspace_root: &str,
    related_files: &[String],
    target_file: &str,
    semantic_snippets: &str,
) -> Result<CodeWriterResult> {
    let root_path = workspace_path::parse_workspace_root(workspace_root);
    let mut session = ToolSession::new(root_path.clone());
    let mut transcript: Vec<String> = Vec::new();
    let mut consecutive_errors = 0usize;
    let mut rejected_paths: Vec<String> = Vec::new();
    let mut accepted_paths: Vec<String> = Vec::new();
    let model = rt.model_for(Agent::CodeWriter).await;
    // Track the last few read-only tool labels so we can detect when the
    // model gets stuck repeating the same call. Read-only because mutations
    // change session state, so a repeat is rarely a "loop" in the bad sense.
    // Track signatures (path-normalised) so `list_dir(.)`, `list_dir("")`, identical
    // probes under the workspace collapse to the same key.
    let mut recent_readonly_signatures: Vec<String> = Vec::new();
    let mut readonly_explore_streak: usize = 0;

    for iteration in 0..MAX_LOOP_ITERATIONS {
        if rt.job_cancel.is_cancelled(job_id) {
            return Ok(session.final_operations("Job cancelled by user.".into()));
        }

        let prompt = build_prompt(
            instruction,
            related_files,
            target_file,
            semantic_snippets,
            &transcript,
            root_path.as_deref(),
            iteration,
        );
        rt.emit_prompt_estimate(job_id, Agent::CodeWriter, Some(SYSTEM_PROMPT), &prompt)
            .await;
        let raw = match rt
            .ollama
            .generate(
                &model,
                Some(SYSTEM_PROMPT),
                &prompt,
                rt.config.ollama_num_ctx,
            )
            .await
        {
            Ok(r) => r,
            Err(err) => {
                rt.bus
                    .publish_event(&ClientEvent::Log {
                        job_id: job_id.to_string(),
                        agent: Agent::CodeWriter,
                        message: format!("Ollama error on iteration {iteration}: {err}"),
                    })
                    .await?;
                break;
            }
        };

        match tools::parse_tool_action(&raw) {
            ToolAction::ParseError(msg) => {
                consecutive_errors += 1;
                rt.bus
                    .publish_event(&ClientEvent::Log {
                        job_id: job_id.to_string(),
                        agent: Agent::CodeWriter,
                        message: format!("could not parse tool call: {msg}"),
                    })
                    .await?;
                push_transcript(&mut transcript, format!("PARSE_ERROR: {msg}"));
                if consecutive_errors >= MAX_PARSE_ERRORS_BEFORE_GIVE_UP {
                    break;
                }
                continue;
            }
            ToolAction::LegacyOperations { result } => {
                rt.bus
                    .publish_event(&ClientEvent::Log {
                        job_id: job_id.to_string(),
                        agent: Agent::CodeWriter,
                        message: format!(
                            "model returned legacy {{operations}} envelope ({} op(s)); using directly",
                            result.operations.len()
                        ),
                    })
                    .await?;
                return Ok(result);
            }
            ToolAction::Call { thought, call } => {
                consecutive_errors = 0;
                let label = call.label();
                let intents_file_mutation = matches!(
                    &call,
                    ToolCall::Edit { .. }
                        | ToolCall::Create { .. }
                        | ToolCall::Delete { .. }
                );
                let counts_as_explore_only = matches!(
                    &call,
                    ToolCall::ReadFile { .. }
                        | ToolCall::ListDir { .. }
                        | ToolCall::Grep { .. }
                        | ToolCall::Glob { .. }
                        | ToolCall::Bash { .. }
                        | ToolCall::SemanticSearch { .. }
                );
                let log_line = if thought.trim().is_empty() {
                    label.clone()
                } else {
                    format!("{label} — {}", thought.trim())
                };
                rt.bus
                    .publish_event(&ClientEvent::Log {
                        job_id: job_id.to_string(),
                        agent: Agent::CodeWriter,
                        message: log_line,
                    })
                    .await?;
                if intents_file_mutation {
                    readonly_explore_streak = 0;
                } else if counts_as_explore_only {
                    readonly_explore_streak += 1;
                }
                if readonly_explore_streak >= READONLY_EXPLORE_STREAK_CAP {
                    let summary = format!(
                        "Stopped: {readonly_explore_streak} read-only tool turns in a row with no file edits. \
The agent was likely exploring in circles (e.g. alternating list_dir targets). \
If list_dir keeps showing empty or errors, pivot to create{{path, content}} or finish with guidance for the user. \
If tooling says there is no workspace root, ask the user to open a folder or pass workspace_root."
                    );
                    rt.bus
                        .publish_event(&ClientEvent::Log {
                            job_id: job_id.to_string(),
                            agent: Agent::CodeWriter,
                            message: summary.clone(),
                        })
                        .await?;
                    return Ok(finalize_result(
                        &session,
                        summary,
                        accepted_paths,
                        rejected_paths,
                    ));
                }

                // Detect read-only loops. If the model fires the same
                // exploration signature many times in a row, workspace
                // output won't change — nudge early, bail after dup cap.
                let sig = readonly_loop_signature(&call, root_path.as_deref());
                let is_readonly = matches!(
                    call,
                    ToolCall::ReadFile { .. }
                        | ToolCall::ListDir { .. }
                        | ToolCall::Grep { .. }
                        | ToolCall::Glob { .. }
                );
                if is_readonly {
                    recent_readonly_signatures.push(sig.clone());
                    if recent_readonly_signatures.len() > 6 {
                        recent_readonly_signatures.remove(0);
                    }
                    let consecutive_repeats = recent_readonly_signatures
                        .iter()
                        .rev()
                        .take_while(|s| **s == sig)
                        .count();
                    if consecutive_repeats >= EXPLORE_DUP_STOP_AFTER {
                        let summary = format!(
                            "Stopped: agent looped on exploration pattern `{sig}` ({label}) {consecutive_repeats} times. \
The workspace state isn't changing — likely it is empty or the path is wrong. \
Tell the user to either supply files in the workspace, or rephrase the request \
so the agent creates files instead of looking for them."
                        );
                        rt.bus
                            .publish_event(&ClientEvent::Log {
                                job_id: job_id.to_string(),
                                agent: Agent::CodeWriter,
                                message: summary.clone(),
                            })
                            .await?;
                        return Ok(finalize_result(
                            &session,
                            summary,
                            accepted_paths,
                            rejected_paths,
                        ));
                    } else if consecutive_repeats >= 2 {
                        let styling_grep_hint = if instruction_demands_site_or_style_edits(instruction)
                            && sig.contains("grep(pattern=")
                        {
                            " STYLING/WEB TASK: grep is substring-only — repeating identical grep does not sharpen CSS/HTML. NEXT turn MUST be `edit` on `styles.css` and/or `.html`; use a verbatim `search` substring from READ_FILE."
                        } else {
                            ""
                        };
                        push_transcript(
                            &mut transcript,
                            format!(
                                "ANTI-LOOP NUDGE (same exploration pattern `{sig}` as `{label}`, {consecutive_repeats} times in a row): \
the workspace view is unchanged.{styling_grep_hint} \
Otherwise pivot — `create{{path, content}}` if files are missing, or a different exploration tool."
                            ),
                        );
                    }
                } else {
                    recent_readonly_signatures.clear();
                }

                if let ToolCall::Finish { summary } = &call {
                    if instruction_demands_site_or_style_edits(instruction) && !session.has_staged() {
                        rt.bus
                            .publish_event(&ClientEvent::Log {
                                job_id: job_id.to_string(),
                                agent: Agent::CodeWriter,
                                message:
                                    "Finish blocked: HTML/CSS/visual request requires at least one substantive \
file change (edit/create/delete) — link checks alone are not completion."
                                        .into(),
                            })
                            .await?;
                        push_transcript(
                            &mut transcript,
                            format!(
                                "CALL: finish (summary omitted)\nRESULT:\nYou cannot finish yet: THIS TASK requires edits to `.html`/`.css` (or deletes). Produce at least one successful `edit` or `create` with real markup/styles before finishing. Draft summary began: `{}`…",
                                summary.chars().take(120).collect::<String>()
                            ),
                        );
                        continue;
                    }
                    if session.has_staged() {
                        let final_ops = session.final_operations(summary.clone());
                        let mut review_req = crate::messages::Message::new(Agent::CodeWriter, Agent::Review, "review")
                            .with_context(serde_json::json!({
                                "code_writer_result": final_ops,
                                "user_message": instruction,
                                "workspace_root": root_path,
                                "related_files": related_files,
                                "planner_plan": "Self-review inside CodeWriter",
                                "code_instruction": instruction,
                            }));
                        review_req.job_id = job_id.to_string();
                        rt.bus.dispatch(&review_req).await.ok();
                        
                        let review_result = loop {
                            match tokio::time::timeout(std::time::Duration::from_secs(120), rt.bus.next_message(Agent::CodeWriter.queue(), 1.0)).await {
                                Ok(Ok(Some(msg))) if msg.task == "review_result" => break msg,
                                Ok(Ok(Some(_))) => continue, // Ignore other messages while waiting for review
                                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break crate::messages::Message::new(Agent::Review, Agent::CodeWriter, "review_result"), // timeout or error
                            }
                        };
                        
                        let verdict: crate::messages::ReviewVerdict = serde_json::from_value(review_result.result).unwrap_or_default();
                        if !verdict.approved && !verdict.reason.is_empty() {
                            push_transcript(
                                &mut transcript,
                                format!(
                                    "CALL: finish\nRESULT:\nReview rejected your changes: {}\nProblems:\n{}\n\nYou must fix these issues before calling finish again.",
                                    verdict.reason,
                                    verdict.problems.join("\n")
                                )
                            );
                            continue;
                        }
                    }

                    let summary = if summary.trim().is_empty() && session.has_staged() {
                        format!("Finished after {} step(s).", iteration + 1)
                    } else {
                        summary.clone()
                    };
                    return Ok(finalize_result(
                        &session,
                        summary,
                        accepted_paths,
                        rejected_paths,
                    ));
                }

                // Bash: propose, wait for user, then run.
                if let ToolCall::Bash {
                    command,
                    timeout_secs,
                } = &call
                {
                    let bash_feedback = match run_bash_with_proposal(
                        rt,
                        job_id,
                        root_path.as_deref(),
                        command,
                        *timeout_secs,
                    )
                    .await
                    {
                        BashFlow::Done(text) => text,
                        BashFlow::Cancelled => return Err(JobCancelled.into()),
                    };
                    let clipped = clip(&bash_feedback, MAX_FEEDBACK_CHARS);
                    push_transcript(
                        &mut transcript,
                        format!("CALL: {label}\nRESULT:\n{clipped}"),
                    );
                    continue;
                }

                // Semantic search: hit the embedding index.
                if let ToolCall::SemanticSearch { query, top_k } = &call {
                    let feedback =
                        run_semantic_search(rt, root_path.as_deref(), query, *top_k).await;
                    let clipped = clip(&feedback, MAX_FEEDBACK_CHARS);
                    push_transcript(
                        &mut transcript,
                        format!("CALL: {label}\nRESULT:\n{clipped}"),
                    );
                    continue;
                }

                let outcome = tools::execute(&mut session, &call).await;
                let mut feedback = outcome.feedback.clone();

                // Mutated session - just log it internally, proposals happen at the end
                if outcome.mutated {
                    if let Some(path) = call.mutating_path() {
                        let key = match tools::safe_relative_file_path(
                            session.workspace_root(),
                            path,
                        ) {
                            Ok((k, _)) => k,
                            Err(_) => path.to_string(),
                        };
                        feedback.push_str("\n(internal: change staged successfully)");
                    }
                }

                let clipped = clip(&feedback, MAX_FEEDBACK_CHARS);
                push_transcript(
                    &mut transcript,
                    format!("CALL: {label}\nRESULT:\n{clipped}"),
                );
            }
        }
    }

    let summary = if session.has_staged() || !accepted_paths.is_empty() {
        format!(
            "Stopped at iteration limit. Accepted: {}. Rejected: {}.",
            accepted_paths.len(),
            rejected_paths.len()
        )
    } else {
        "Stopped without producing edits — please rephrase or supply more context.".into()
    };
    Ok(finalize_result(
        &session,
        summary,
        accepted_paths,
        rejected_paths,
    ))
}

/// Build the final result handed back to the orchestrator. The operations
/// list is exactly what the user accepted in-loop; `already_decided=true`
/// signals that the orchestrator should NOT re-propose them.
fn finalize_result(
    session: &ToolSession,
    summary: String,
    _accepted_paths: Vec<String>,
    rejected_paths: Vec<String>,
) -> CodeWriterResult {
    let mut result = session.final_operations(summary);
    result.rejected_paths = rejected_paths;
    result.already_decided = false;
    result
}

#[derive(Debug, Clone, Copy)]
enum ProposalOutcome {
    Accepted,
    Rejected,
    Cancelled,
}

async fn propose_and_await(
    rt: &AgentRuntime,
    job_id: &str,
    operation: FileOperation,
) -> ProposalOutcome {
    let proposal_id = Uuid::new_v4().to_string();
    let receiver = rt.proposals.register(proposal_id.clone());
    let event = ClientEvent::FileProposal {
        job_id: job_id.to_string(),
        proposal_id: proposal_id.clone(),
        operation,
        review_notes: None,
    };
    if let Err(err) = rt.bus.publish_event(&event).await {
        tracing::warn!("failed to publish file proposal: {err}");
        return ProposalOutcome::Rejected;
    }

    tokio::select! {
        wait = tokio::time::timeout(PROPOSAL_DECISION_TIMEOUT, receiver) => {
            match wait {
                Ok(Ok(true)) => ProposalOutcome::Accepted,
                Ok(Ok(false)) => ProposalOutcome::Rejected,
                Ok(Err(_)) => ProposalOutcome::Rejected,
                Err(_) => {
                    rt.proposals.resolve(&proposal_id, false);
                    ProposalOutcome::Rejected
                }
            }
        }
        () = poll_until_cancelled(rt, job_id) => {
            let _ = rt.proposals.resolve(&proposal_id, false);
            ProposalOutcome::Cancelled
        }
    }
}

async fn poll_until_cancelled(rt: &AgentRuntime, job_id: &str) {
    loop {
        tokio::time::sleep(PROPOSAL_CANCEL_POLL).await;
        if rt.job_cancel.is_cancelled(job_id) {
            return;
        }
    }
}

enum BashFlow {
    Done(String),
    Cancelled,
}

async fn run_semantic_search(
    rt: &AgentRuntime,
    workspace_root: Option<&Path>,
    query: &str,
    top_k: Option<usize>,
) -> String {
    let root = match workspace_root {
        Some(r) => r,
        None => {
            return "error: semantic_search needs a workspace root (open a folder in the editor or pass --workspace)".to_string();
        }
    };
    let model = rt.models.read().await.embeddings.clone();
    if let Err(err) = rt.semantic_index.ensure_built(&rt.ollama, &model, root).await {
        return format!("error building semantic index: {err}");
    }
    let k = top_k.unwrap_or(8).clamp(1, 20);
    match rt.semantic_index.search(&rt.ollama, &model, query, k).await {
        Ok(hits) if hits.is_empty() => format!(
            "(no semantic hits for `{query}` — index has {} chunk(s))",
            rt.semantic_index.entry_count()
        ),
        Ok(hits) => {
            let mut out = format!(
                "--- semantic_search `{query}` (top {} of {} chunk(s)) ---\n",
                hits.len(),
                rt.semantic_index.entry_count()
            );
            for hit in hits {
                out.push_str(&format!(
                    "{}:{}-{}  score={:.3}\n{}\n\n",
                    hit.path, hit.start_line, hit.end_line, hit.score, hit.snippet
                ));
            }
            out
        }
        Err(err) => format!("error during semantic search: {err}"),
    }
}

async fn run_bash_with_proposal(
    rt: &AgentRuntime,
    job_id: &str,
    workspace_root: Option<&Path>,
    command: &str,
    timeout_secs: Option<u64>,
) -> BashFlow {
    let cmd_trimmed = command.trim();
    let is_safe = cmd_trimmed.starts_with("ls")
        || cmd_trimmed.starts_with("find ")
        || cmd_trimmed.starts_with("cat ")
        || cmd_trimmed.starts_with("grep ")
        || cmd_trimmed.starts_with("rg ")
        || cmd_trimmed.starts_with("cargo check")
        || cmd_trimmed.starts_with("cargo clippy")
        || cmd_trimmed.starts_with("cargo test")
        || cmd_trimmed.starts_with("tsc ")
        || cmd_trimmed.starts_with("npm run lint")
        || cmd_trimmed.starts_with("npm test")
        || cmd_trimmed.starts_with("python -m py_compile");

    if is_safe {
        rt.bus.publish_event(&ClientEvent::Log {
            job_id: job_id.to_string(),
            agent: Agent::CodeWriter,
            message: format!("auto-running safe bash command: {}", command),
        }).await.ok();
        
        let timeout = crate::bash::resolve_timeout(timeout_secs);
        let outcome = crate::bash::run(workspace_root, command, timeout).await;
        
        let result_event = ClientEvent::CommandResult {
            job_id: job_id.to_string(),
            proposal_id: "auto-approved".to_string(),
            exit_code: outcome.exit_code,
            stdout: outcome.stdout.clone(),
            stderr: outcome.stderr.clone(),
            truncated: outcome.truncated,
        };
        rt.bus.publish_event(&result_event).await.ok();

        let exit_label = outcome
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let mut feedback = format!("$ {command}\n--- exit {exit_label}");
        if outcome.timed_out {
            feedback.push_str(" (timed out)");
        }
        if outcome.truncated {
            feedback.push_str(" (truncated)");
        }
        feedback.push_str(" ---\n");
        if !outcome.stdout.is_empty() {
            feedback.push_str("--- stdout ---\n");
            feedback.push_str(&outcome.stdout);
            feedback.push('\n');
        }
        if !outcome.stderr.is_empty() {
            feedback.push_str("--- stderr ---\n");
            feedback.push_str(&outcome.stderr);
            feedback.push('\n');
        }
        return BashFlow::Done(feedback);
    }

    let proposal_id = Uuid::new_v4().to_string();
    let receiver = rt.proposals.register(proposal_id.clone());
    let proposal_event = ClientEvent::CommandProposal {
        job_id: job_id.to_string(),
        proposal_id: proposal_id.clone(),
        command: command.to_string(),
        cwd: workspace_root.map(|p| p.display().to_string()),
    };
    if let Err(err) = rt.bus.publish_event(&proposal_event).await {
        return BashFlow::Done(format!("error publishing command proposal: {err}"));
    }

    let decision = tokio::select! {
        wait = tokio::time::timeout(PROPOSAL_DECISION_TIMEOUT, receiver) => {
            match wait {
                Ok(Ok(true)) => Some(true),
                Ok(Ok(false)) => Some(false),
                Ok(Err(_)) => Some(false),
                Err(_) => {
                    rt.proposals.resolve(&proposal_id, false);
                    Some(false)
                }
            }
        }
        () = poll_until_cancelled(rt, job_id) => None,
    };

    match decision {
        None => {
            let _ = rt.proposals.resolve(&proposal_id, false);
            BashFlow::Cancelled
        }
        Some(false) => BashFlow::Done(
            "✗ user rejected the bash command. Do not retry the same command — try a different approach or skip the step."
                .to_string(),
        ),
        Some(true) => {
            let timeout = bash::resolve_timeout(timeout_secs);
            let outcome = bash::run(workspace_root, command, timeout).await;

            let result_event = ClientEvent::CommandResult {
                job_id: job_id.to_string(),
                proposal_id: proposal_id.clone(),
                exit_code: outcome.exit_code,
                stdout: outcome.stdout.clone(),
                stderr: outcome.stderr.clone(),
                truncated: outcome.truncated,
            };
            if let Err(err) = rt.bus.publish_event(&result_event).await {
                tracing::warn!("failed to publish command result: {err}");
            }

            let exit_label = outcome
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into());
            let mut feedback = format!("$ {command}\n--- exit {exit_label}");
            if outcome.timed_out {
                feedback.push_str(" (timed out)");
            }
            if outcome.truncated {
                feedback.push_str(" (truncated)");
            }
            feedback.push_str(" ---\n");
            if !outcome.stdout.is_empty() {
                feedback.push_str("--- stdout ---\n");
                feedback.push_str(&outcome.stdout);
                feedback.push('\n');
            }
            if !outcome.stderr.is_empty() {
                feedback.push_str("--- stderr ---\n");
                feedback.push_str(&outcome.stderr);
                feedback.push('\n');
            }
            BashFlow::Done(feedback)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_prompt(
    instruction: &str,
    related_files: &[String],
    target_file: &str,
    semantic_snippets: &str,
    transcript: &[String],
    workspace_root: Option<&Path>,
    iteration: usize,
) -> String {
    let mut out = String::new();
    if let Some(root) = workspace_root {
        out.push_str(&format!(
            "Workspace root (all paths must be RELATIVE to this): {}\n",
            root.display()
        ));
    } else {
        out.push_str(
            "Workspace root: <none — file/grep/list tools will fail. Tell the user to open a folder in the editor or pass --workspace>\n",
        );
    }
    if !related_files.is_empty() {
        out.push_str("Indexer hints (indexed workspace paths — READ these before editing; correlate HTML↔CSS; use grep/glob for any path not listed):\n");
        for path in related_files.iter().take(MAX_INDEXER_HINTS_IN_PROMPT) {
            out.push_str(&format!("  - {path}\n"));
        }
    } else if workspace_root.is_some() {
        out.push_str(
            "Indexer hints: none yet — the editor snapshot may still be syncing. Discover files with glob (e.g. `**/*.html`, `**/*.{css,scss}`) and list_dir, then read_file before edits.\n",
        );
    }
    if !target_file.is_empty() {
        out.push_str(&format!("Suggested primary file: {target_file}\n"));
    }
    if !semantic_snippets.is_empty() {
        out.push_str("\nSemantic Search Context (automatically retrieved snippets based on your task):\n");
        out.push_str(semantic_snippets);
        out.push_str("\n");
    }

    out.push_str("\nTask:\n");
    out.push_str(instruction.trim());
    out.push_str("\n\n");

    if transcript.is_empty() {
        out.push_str(
            "No tool calls yet. Begin by exploring the workspace (list_dir / glob / grep / read_file).\n",
        );
    } else {
        out.push_str("Tool transcript so far:\n");
        let trimmed = trim_transcript(transcript);
        for (i, entry) in trimmed.iter().enumerate() {
            out.push_str(&format!("--- step {} ---\n{entry}\n", i + 1));
        }
    }

    out.push_str(&format!(
        "\n(iteration {} of {MAX_LOOP_ITERATIONS}.) Reply with one JSON tool call.\n",
        iteration + 1
    ));
    out
}

fn push_transcript(transcript: &mut Vec<String>, entry: String) {
    transcript.push(entry);
}

/// Keep the transcript bounded so prompts don't grow without limit. We drop
/// the oldest entries first; the model can always re-read a file if it
/// needs to.
fn trim_transcript(transcript: &[String]) -> Vec<String> {
    let mut total: usize = transcript.iter().map(|s| s.len()).sum();
    if total <= MAX_TRANSCRIPT_CHARS {
        return transcript.to_vec();
    }
    let mut out: Vec<String> = transcript.to_vec();
    while total > MAX_TRANSCRIPT_CHARS && out.len() > 1 {
        let dropped = out.remove(0);
        total = total.saturating_sub(dropped.len());
    }
    if out.len() < transcript.len() {
        out.insert(
            0,
            format!(
                "(dropped {} earlier step(s) to stay under {MAX_TRANSCRIPT_CHARS} chars)",
                transcript.len() - out.len()
            ),
        );
    }
    out
}

fn clip(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let head: String = text.chars().take(cap).collect();
    format!("{head}\n… [truncated; tool output exceeded {cap} chars]")
}

// =============================================================================
// Legacy parsers — kept public so the integration agent and existing tests
// continue to work. The tool loop now consumes these via `LegacyOperations`,
// but they remain available as standalone fallbacks.
// =============================================================================

/// Parse a model response and return a `CodeWriterResult`. Tries the
/// `{operations, summary}` envelope, then a bare operations array, then a
/// single operation object. Falls back to wrapping the raw text as a single
/// `create` op against `fallback_path` so something always gets through.
pub fn parse_code_writer_output(raw: &str, fallback_path: &str) -> CodeWriterResult {
    if let Some(parsed) = parse_envelope(raw) {
        return parsed;
    }
    if let Some(operations) = parse_operations_array(raw) {
        return CodeWriterResult {
            operations,
            ..Default::default()
        };
    }
    if let Some(operation) = parse_single_operation(raw) {
        return CodeWriterResult {
            operations: vec![operation],
            ..Default::default()
        };
    }
    tracing::warn!("code writer returned unparseable output, falling back to single create");
    CodeWriterResult {
        operations: vec![FileOperation {
            action: FileAction::Create,
            file: fallback_path.to_string(),
            content: Some(raw.to_string()),
        }],
        summary: "Could not parse JSON, returned raw model output.".into(),
        ..Default::default()
    }
}

/// Like `parse_code_writer_output` but never invents a fallback file. Used
/// by the integration agent — when wiring edits cannot be decoded we'd
/// rather emit nothing than pollute the workspace.
pub fn parse_operations_envelope_or_empty(raw: &str) -> CodeWriterResult {
    if let Some(parsed) = parse_envelope(raw) {
        return parsed;
    }
    if let Some(operations) = parse_operations_array(raw) {
        return CodeWriterResult {
            operations,
            ..Default::default()
        };
    }
    if let Some(operation) = parse_single_operation(raw) {
        return CodeWriterResult {
            operations: vec![operation],
            ..Default::default()
        };
    }
    CodeWriterResult::default()
}

fn parse_envelope(raw: &str) -> Option<CodeWriterResult> {
    let trimmed = strip_code_fence(raw);
    if let Ok(parsed) = serde_json::from_str::<CodeWriterResult>(&trimmed) {
        if !parsed.operations.is_empty() || !parsed.summary.is_empty() {
            return Some(parsed);
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    let candidate = &trimmed[start..=end];
    let parsed: CodeWriterResult = serde_json::from_str(candidate).ok()?;
    if parsed.operations.is_empty() && parsed.summary.is_empty() {
        return None;
    }
    Some(parsed)
}

fn parse_operations_array(raw: &str) -> Option<Vec<FileOperation>> {
    let trimmed = strip_code_fence(raw);
    if let Ok(ops) = serde_json::from_str::<Vec<FileOperation>>(&trimmed) {
        return Some(ops);
    }
    let start = trimmed.find('[')?;
    let end = trimmed.rfind(']')?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

fn parse_single_operation(raw: &str) -> Option<FileOperation> {
    let trimmed = strip_code_fence(raw);
    if let Ok(op) = serde_json::from_str::<FileOperation>(&trimmed) {
        return Some(op);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

fn strip_code_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod instruction_heuristic_tests {
    use super::{instruction_demands_site_or_style_edits, instruction_signals_html_css_or_site_work};

    #[test]
    fn detects_styling_request() {
        assert!(instruction_signals_html_css_or_site_work("Add more styling to the site"));
        assert!(instruction_demands_site_or_style_edits(
            "can you add more styling to my website?"
        ));
    }

    #[test]
    fn question_intro_is_not_trap() {
        assert!(!instruction_signals_html_css_or_site_work(
            "What is the difference between margin and padding in CSS?"
        ));
    }

    #[test]
    fn make_it_pretty_requires_edits() {
        assert!(instruction_demands_site_or_style_edits(
            "just make it look and formatted good with my index.html"
        ));
    }

    #[test]
    fn continue_elements_request() {
        assert!(instruction_demands_site_or_style_edits(
            "continue adding more elements with corresponding styling"
        ));
    }
}
