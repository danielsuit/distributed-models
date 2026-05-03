//! Review Agent.
//!
//! Validates the code-writer's `CodeWriterResult` before the orchestrator
//! shows it to the user. Returns `{approved, reason, problems}` so the
//! orchestrator can either continue, annotate, or warn.

use anyhow::Result;
use serde_json::json;

use crate::agents::AgentRuntime;
use crate::messages::{Agent, ClientEvent, CodeWriterResult, ReviewVerdict};

const POLL_TIMEOUT_SECS: f64 = 5.0;

/// Cap per-file body in the review prompt so local models stay within context.
const MAX_REVIEW_CHARS_PER_FILE: usize = 48_000;

const MAX_RELATED_PATHS_IN_REVIEW_PROMPT: usize = 80;

const SYSTEM_PROMPT: &str = "You are a code review agent. You read proposed file operations (create/edit/delete) — often several files produced by independent steps — and output JSON: \
{\"approved\":true|false,\"reason\":\"<short explanation>\",\"problems\":[\"<list of concrete issues>\"]}. \
You are given the user's request, planner notes, and optional index paths so you can judge whether the bundle matches intent and whether imports/links reference plausible paths (proposal + related path list; you do not have file contents except inside the proposed operations). \
Approve when each file looks syntactically valid and imports/reference paths look plausible together. \
**Reject** when the later operation for the **same path** replaces a substantive file with something much shorter, empty-ish, placeholder-only (`...`, stubs, `{}` only), or appears to discard prior content—a sign of accidental wipe. \
Reject when the user's request demanded visual polish LOOK BETTER yet the bundle MAKES ZERO substantive stylesheet/markup deltas and excuses are only ALREADY LINKED / NOTHING TO CHANGE — stylesheet presence is unrelated to aesthetics; require real CSS tweaks or enumerate why impossible. \
Reject when the user wanted better styling/visuals but CSS deltas are hollow: ONLY comments/TODO stubs (ADD MORE STYLES placeholders) without substantive new selectors+declarations.
Reject when a styling request results ONLY in resizing the `body` or `html` elements without targeting specific child elements (like buttons, navbars, layout grids, etc.). A request for \"styling\" or \"better visuals\" MUST include rules for specific UI elements. \
Reject when the user asks for an app, script, or website, and the proposed code is an overly basic \"Minimum Viable Product\" (e.g., just an HTML file that says \"Website\" or a python script with a single print statement). The code must be comprehensive, production-ready, and complex enough to satisfy the request. \
Reject for clearly broken syntax, missing imports, contradictions between files in the bundle, wildly off-task content, or destructive-looking replacements. Do not rewrite the code. \
For HTML drafts: `<button href=\"\">` is invalid (use `<a href>` for navigation); if `<body>` implies layout/styling yet `<head>` has no `<link rel=\"stylesheet\">`/`@import`/`<style>` and the task implied a styled UI, approve=false or enumerate the gap so it can be fixed. \
**HTML↔CSS coupling:** Reject bundles that mix HTML + CSS where CSS defines `.class` or `#id` rules that NEVER appear as `class=` / `id=` in any proposed HTML in the bundle (orphan stylesheet rules)—unless edits clearly add those attributes in the same proposal set. Prefer approve=false when site-building intent is obvious but markup lacks matching hooks for declared selectors. \
If a section is marked TRUNCATED, only review the visible portion and note uncertainty in problems if needed.";

pub async fn run(rt: AgentRuntime) {
    tracing::info!("review agent online");
    loop {
        match step(&rt).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("review agent error: {err:?}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn step(rt: &AgentRuntime) -> Result<()> {
    let Some(message) = rt
        .bus
        .next_message(Agent::Review.queue(), POLL_TIMEOUT_SECS)
        .await?
    else {
        return Ok(());
    };

    rt.bus
        .publish_event(&ClientEvent::AgentStatus {
            job_id: message.job_id.clone(),
            agent: Agent::Review,
            status: "reviewing".into(),
        })
        .await?;

    let payload: CodeWriterResult = match serde_json::from_value(
        message
            .context
            .get("code_writer_result")
            .cloned()
            .unwrap_or_default(),
    ) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!("review agent could not parse payload: {err}");
            let response = message
                .reply(message.from, "review_result")
                .with_result(json!(ReviewVerdict {
                    approved: false,
                    reason: format!("review payload parse error: {err}"),
                    problems: vec![],
                }));
            rt.bus.dispatch(&response).await?;
            return Ok(());
        }
    };

    let user_message = message
        .context
        .get("user_message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let workspace_root = message
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let planner_plan = message
        .context
        .get("planner_plan")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let code_instruction = message
        .context
        .get("code_instruction")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let related_files: Vec<String> = message
        .context
        .get("related_files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .take(MAX_RELATED_PATHS_IN_REVIEW_PROMPT)
                .collect()
        })
        .unwrap_or_default();

    let verdict = review_payload(
        rt,
        message.job_id.as_str(),
        &payload,
        &user_message,
        &workspace_root,
        &planner_plan,
        &code_instruction,
        &related_files,
    )
    .await
    .unwrap_or(ReviewVerdict {
        approved: true,
        reason: "Review skipped due to model error.".into(),
        problems: vec![],
    });

    let response = message
        .reply(message.from, "review_result")
        .with_result(json!(verdict));
    rt.bus.dispatch(&response).await?;
    Ok(())
}

async fn review_payload(
    rt: &AgentRuntime,
    job_id: &str,
    payload: &CodeWriterResult,
    user_message: &str,
    workspace_root: &str,
    planner_plan: &str,
    code_instruction: &str,
    related_files: &[String],
) -> Result<ReviewVerdict> {
    let mut prompt = String::from("## Task context (for intent and cross-path checks)\n");
    push_optional_block(&mut prompt, "User request", user_message);
    push_optional_block(&mut prompt, "Orchestrator plan summary", planner_plan);
    push_optional_block(&mut prompt, "Code-writer instruction", code_instruction);
    push_optional_block(&mut prompt, "Workspace root", workspace_root);
    if !related_files.is_empty() {
        prompt.push_str(&format!(
            "Indexer path hints (not full contents; proposals may omit some):\n{:?}\n\n",
            related_files
        ));
    }
    if !payload.summary.trim().is_empty() {
        prompt.push_str(&format!(
            "Writer/integration summary:\n{}\n\n",
            payload.summary.trim()
        ));
    }

    prompt.push_str("## Proposed file operations\n\n");
    for op in &payload.operations {
        let action_str = match op.action {
            crate::messages::FileAction::Create => "CREATE",
            crate::messages::FileAction::Edit => "EDIT",
            crate::messages::FileAction::Delete => "DELETE",
        };
        let body = match &op.content {
            Some(text) => truncate_for_review(text),
            None => "(no content — delete?)".to_string(),
        };
        prompt.push_str(&format!("=== {action_str} {} ===\n{}\n\n", op.file, body));
    }
    prompt.push_str("Now respond with the JSON verdict only.");

    rt.emit_prompt_estimate(job_id, Agent::Review, Some(SYSTEM_PROMPT), &prompt)
        .await;
    let raw = rt
        .ollama
        .generate(
            &rt.model_for(Agent::Review).await,
            Some(SYSTEM_PROMPT),
            &prompt,
            rt.config.ollama_num_ctx,
        )
        .await?;
    Ok(parse_verdict(&raw))
}

fn push_optional_block(out: &mut String, label: &str, value: &str) {
    let t = value.trim();
    if t.is_empty() {
        return;
    }
    out.push_str(&format!("{label}:\n{t}\n\n"));
}

fn truncate_for_review(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= MAX_REVIEW_CHARS_PER_FILE {
        return content.to_string();
    }
    let head: String = chars[..MAX_REVIEW_CHARS_PER_FILE].iter().collect();
    format!(
        "{}\n\n… [TRUNCATED for review prompt: {} chars total, only first {} visible]\n",
        head,
        chars.len(),
        MAX_REVIEW_CHARS_PER_FILE
    )
}

/// Public for tests: parse a verdict from raw model output, falling back to
/// "approved" when the model returns prose we cannot interpret.
pub fn parse_verdict(raw: &str) -> ReviewVerdict {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(verdict) = serde_json::from_str::<ReviewVerdict>(trimmed) {
        return verdict;
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if let Ok(verdict) = serde_json::from_str::<ReviewVerdict>(&trimmed[start..=end]) {
            return verdict;
        }
    }
    ReviewVerdict {
        approved: true,
        reason: "Reviewer returned non-JSON output; defaulting to approved.".into(),
        problems: vec![],
    }
}
