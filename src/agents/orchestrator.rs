//! Orchestrator Agent.
//!
//! Owns the lifecycle of every chat job. The main loop polls the
//! orchestrator inbox; when it receives a brand new request (`user_message`
//! or `auto_fix`) it spawns a per-job task. All other messages on the inbox
//! are routed to the corresponding job task via an in-process registry so
//! each job can drive its sub-agents without stepping on the others.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agents::AgentRuntime;
use crate::job_cancel::JobCancelled;
use crate::messages::{
    Agent, ChatTurn, ClientEvent, CodeWriterResult, FileOperation, Message, ReviewVerdict,
};

/// How many recent conversation turns to include verbatim in the planner /
/// summary prompts. Older turns are dropped client-side; this is a defensive
/// cap on the backend so a misbehaving client can't blow up the prompt.
const MAX_HISTORY_TURNS: usize = 16;

const POLL_TIMEOUT_SECS: f64 = 5.0;
/// Caps how long quick sub-steps (index query, planner adjacency, review) block the job.
const SUBTASK_TIMEOUT_QUICK_SECS: u64 = 600;
/// Code writer runs many Ollama turns and may propose edits; user accept/reject can take up to
/// `code_writer::PROPOSAL_DECISION_TIMEOUT` **per proposal** — orchestrator MUST outlive that tail.
/// 90 minutes = headroom beyond one ~30‑minute unattended proposal window plus model latency.
const SUBTASK_TIMEOUT_CODE_WRITER_SECS: u64 = 60 * 90;
/// How long we'll wait for the user (or auto-acceptor) to decide on a single
/// proposal. 30 minutes is generous; we don't want to block jobs forever.
const PROPOSAL_DECISION_TIMEOUT: Duration = Duration::from_secs(60 * 30);
const PROPOSAL_CANCEL_POLL: Duration = Duration::from_millis(250);

const PLANNER_SYSTEM: &str = "You are an orchestrator. Decide whether to call helper agents and respond with strict JSON: \
{\"plan\":\"<short summary>\",\"need_files\":true|false,\"file_query\":\"<keywords>\",\"need_code\":true|false,\"target_file\":\"<relative path or empty>\",\"code_instruction\":\"<single instruction for the code writer>\",\"final_answer\":\"<answer to user when no code is required>\"}. \
When need_files is true, file_query must be **path fragments** likely to occur in workspace paths (e.g. \"\", \"src\", \"html\", \"css\", \"components\") — not prose like \"review\" or \"professional website\" (those match zero files). \
Use \"\" for file_query when unsure; indexing returns all files anyway. \
If need_code is true, the orchestrator ALWAYS runs an index sweep — set need_files:true when edits should focus (use file_query \"\" or fragments like css/html). \
Set need_code to true only when the user wants new or modified files. When need_code is false, fill final_answer. \
IMPORTANT: If the user asks whether a website/app \"works\", is \"fully working\", wants styling/CSS/visual polish, or to \"make sure\" pages work, you MUST NOT guess from memory — set need_files true AND need_code true and give code_instruction demanding the writer read existing HTML/CSS, match selectors to real elements, and patch with targeted `edit` (not blanket rewrites). \
For static sites and \"create a website\" tasks: require **HTML↔CSS coupling** — the writer must add `class`/`id` on markup for every new component style (or use only selectors already in HTML); orphan `.class` rules with plain unstyled tags are unacceptable. \
PREMIUM QUALITY & COMPLEXITY (CRITICAL): If the user asks to build a website, app, script, or feature, DO NOT output a basic Minimum Viable Product (MVP). You MUST instruct the Code Writer to build a comprehensive, complex, and production-ready solution with robust error handling, modern design, modular structure, and deep detail. Never settle for just outputting 'website'.";

const ANSWER_SYSTEM: &str = "You are the orchestrator's final speaker. Summarise what happened for the user in friendly markdown. \
Mention each file that was created/edited/deleted or rejected, and any open follow-ups. Keep it under 200 words.";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PlannerOutput {
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub need_files: bool,
    #[serde(default)]
    pub file_query: String,
    #[serde(default)]
    pub need_code: bool,
    #[serde(default)]
    pub target_file: String,
    #[serde(default)]
    pub code_instruction: String,
    #[serde(default)]
    pub final_answer: String,
}

type JobRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<Message>>>>;

pub async fn run(rt: AgentRuntime) {
    tracing::info!("orchestrator agent online");
    let registry: JobRegistry = Arc::new(Mutex::new(HashMap::new()));
    loop {
        match step(&rt, &registry).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("orchestrator step error: {err:?}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn step(rt: &AgentRuntime, registry: &JobRegistry) -> Result<()> {
    let Some(message) = rt
        .bus
        .next_message(Agent::Orchestrator.queue(), POLL_TIMEOUT_SECS)
        .await?
    else {
        return Ok(());
    };

    match message.task.as_str() {
        "user_message" | "auto_fix" => {
            spawn_job(rt.clone(), registry.clone(), message);
        }
        _ => {
            let sender = {
                let map = registry.lock();
                map.get(&message.job_id).cloned()
            };
            match sender {
                Some(tx) => {
                    if tx.send(message).await.is_err() {
                        tracing::warn!("dropping reply: job task already finished");
                    }
                }
                None => {
                    tracing::warn!(
                        "orchestrator received reply for unknown job {} task {}",
                        message.job_id,
                        message.task
                    );
                }
            }
        }
    }
    Ok(())
}

fn spawn_job(rt: AgentRuntime, registry: JobRegistry, initial: Message) {
    let job_id = initial.job_id.clone();
    let (tx, rx) = mpsc::channel::<Message>(32);
    registry.lock().insert(job_id.clone(), tx);

    tokio::spawn(async move {
        let outcome = run_job(rt.clone(), initial.clone(), rx).await;
        match outcome {
            Ok(()) => {}
            Err(ref err) if err.downcast_ref::<JobCancelled>().is_some() => {
                tracing::info!("job {} stopped by user", job_id);
                let _ = rt
                    .bus
                    .publish_event(&ClientEvent::AssistantMessage {
                        job_id: job_id.clone(),
                        text: "Stopped.".into(),
                    })
                    .await;
            }
            Err(err) => {
                tracing::error!("job {} failed: {err:?}", job_id);
                let _ = rt
                    .bus
                    .publish_event(&ClientEvent::Error {
                        job_id: job_id.clone(),
                        message: format!("Job failed: {err}"),
                    })
                    .await;
            }
        }
        let _ = rt
            .bus
            .publish_event(&ClientEvent::JobComplete {
                job_id: job_id.clone(),
            })
            .await;
        rt.job_cancel.clear(&job_id);
        registry.lock().remove(&job_id);
    });
}

async fn run_job(
    rt: AgentRuntime,
    initial: Message,
    mut replies: mpsc::Receiver<Message>,
) -> Result<()> {
    let job_id = initial.job_id.clone();
    let user_message = initial
        .context
        .get("user_message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let workspace_root = initial
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let history: Vec<ChatTurn> = initial
        .context
        .get("history")
        .and_then(|v| serde_json::from_value::<Vec<ChatTurn>>(v.clone()).ok())
        .unwrap_or_default();

    rt.bus
        .publish_event(&ClientEvent::AgentStatus {
            job_id: job_id.clone(),
            agent: Agent::Orchestrator,
            status: "planning".into(),
        })
        .await?;

    let plan =
        enforce_frontend_audit_plan(plan_job(&rt, &job_id, &user_message, &history).await?, &user_message);
    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }
    rt.bus
        .publish_event(&ClientEvent::Log {
            job_id: job_id.clone(),
            agent: Agent::Orchestrator,
            message: format!("Plan: {}", plan.plan),
        })
        .await?;

    let mut related_files: Vec<String> = Vec::new();

    let should_index_files = plan.need_files || plan.need_code;
    let effective_file_query = if plan.need_files && !plan.file_query.trim().is_empty() {
        plan.file_query.clone()
    } else {
        String::new()
    };

    if should_index_files {
        rt.bus
            .publish_event(&ClientEvent::AgentStatus {
                job_id: job_id.clone(),
                agent: Agent::FileStructure,
                status: "scanning workspace".into(),
            })
            .await?;
        let mut request = Message::new(Agent::Orchestrator, Agent::FileStructure, "query")
            .with_context(json!({
                "query": effective_file_query,
                "workspace_root": workspace_root,
            }));
        request.job_id = job_id.clone();
        rt.bus.dispatch(&request).await?;

        let reply = wait_for(
            &rt,
            &job_id,
            &mut replies,
            "query_result",
            Duration::from_secs(SUBTASK_TIMEOUT_QUICK_SECS),
        )
        .await?;
        related_files = reply
            .result
            .get("matches")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }

    let mut semantic_snippets = String::new();
    if should_index_files && !workspace_root.is_empty() {
        rt.bus
            .publish_event(&ClientEvent::AgentStatus {
                job_id: job_id.clone(),
                agent: Agent::Orchestrator,
                status: "semantic search".into(),
            })
            .await?;
        let embeddings_model = rt.config.models.embeddings.clone();
        if let Ok(()) = rt.semantic_index.ensure_built(&rt.ollama, &embeddings_model, std::path::Path::new(&workspace_root)).await {
            if let Ok(hits) = rt.semantic_index.search(&rt.ollama, &embeddings_model, &user_message, 5).await {
                for (i, hit) in hits.into_iter().enumerate() {
                    semantic_snippets.push_str(&format!("Snippet {} ({}:L{}-{}): \n{}\n", i + 1, hit.path, hit.start_line, hit.end_line, hit.snippet));
                }
            }
        }
    }

    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }

    if !plan.need_code {
        let summary = if plan.final_answer.is_empty() {
            plan.plan.clone()
        } else {
            plan.final_answer.clone()
        };
        rt.bus
            .publish_event(&ClientEvent::AssistantMessage {
                job_id: job_id.clone(),
                text: summary,
            })
            .await?;
        return Ok(());
    }

    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }

    rt.bus
        .publish_event(&ClientEvent::AgentStatus {
            job_id: job_id.clone(),
            agent: Agent::CodeWriter,
            status: "drafting files".into(),
        })
        .await?;

    let mut writer_request = Message::new(Agent::Orchestrator, Agent::CodeWriter, "write_code")
        .with_context(json!({
            "instruction": if plan.code_instruction.is_empty() {
                user_message.clone()
            } else {
                plan.code_instruction.clone()
            },
            "target_file": plan.target_file,
            "workspace_root": workspace_root,
            "related_files": related_files,
            "semantic_snippets": semantic_snippets,
        }));
    writer_request.job_id = job_id.clone();
    rt.bus.dispatch(&writer_request).await?;

    let writer_reply = wait_for(
        &rt,
        &job_id,
        &mut replies,
        "code_writer_result",
        Duration::from_secs(SUBTASK_TIMEOUT_CODE_WRITER_SECS),
    )
    .await?;
    let writer_result: CodeWriterResult =
        serde_json::from_value(writer_reply.result.clone()).unwrap_or_default();

    // The new tool-loop CodeWriter emits FileProposal events inline and waits
    // for the user, so its returned operations are already accepted. We must
    // not re-propose them, but we still want them visible in the final
    // summary.
    let writer_already_decided = writer_result.already_decided;
    let writer_rejected: Vec<String> = writer_result.rejected_paths.clone();
    let writer_accepted: Vec<String> = if writer_already_decided {
        writer_result
            .operations
            .iter()
            .map(|op| op.file.clone())
            .collect()
    } else {
        Vec::new()
    };

    let merged_result = writer_result;

    if merged_result.operations.is_empty() && writer_rejected.is_empty() {
        let text = if merged_result.summary.is_empty() {
            "Code writer returned no usable operations.".to_string()
        } else {
            merged_result.summary.clone()
        };
        rt.bus
            .publish_event(&ClientEvent::AssistantMessage {
                job_id: job_id.clone(),
                text,
            })
            .await?;
        return Ok(());
    }

    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }

    // The standalone Integration agent used to run a one-shot whole-file
    // pass here to add wiring edits (imports, route entries, nav links).
    // The CodeWriter's tool-use loop now reads the workspace, edits in
    // place, and gets immediate user feedback, so wiring concerns are
    // handled in the same loop. We keep the agent code around for tests
    // and as a future opt-in second pass, but skip it from the default
    // pipeline. Writer's ops are already user-decided.
    let extras_count = 0usize;
    let writer_op_count = merged_result.operations.len();

    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }

    rt.bus
        .publish_event(&ClientEvent::AgentStatus {
            job_id: job_id.clone(),
            agent: Agent::Review,
            status: "validating".into(),
        })
        .await?;

    let mut review_request = Message::new(Agent::Orchestrator, Agent::Review, "review")
        .with_context(json!({
            "code_writer_result": merged_result,
            "user_message": user_message,
            "workspace_root": workspace_root,
            "related_files": related_files,
            "planner_plan": plan.plan,
            "code_instruction": if plan.code_instruction.is_empty() {
                user_message.clone()
            } else {
                plan.code_instruction.clone()
            },
        }));
    review_request.job_id = job_id.clone();
    rt.bus.dispatch(&review_request).await?;
    let review_reply = wait_for(
        &rt,
        &job_id,
        &mut replies,
        "review_result",
        Duration::from_secs(SUBTASK_TIMEOUT_QUICK_SECS),
    )
    .await?;
    let verdict: ReviewVerdict =
        serde_json::from_value(review_reply.result.clone()).unwrap_or_default();

    if !verdict.approved {
        rt.bus
            .publish_event(&ClientEvent::Log {
                job_id: job_id.clone(),
                agent: Agent::Review,
                message: format!(
                    "Review flagged the proposal: {} (problems: {})",
                    verdict.reason,
                    verdict.problems.join(", ")
                ),
            })
            .await?;
    }

    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }

    // Writer's operations were proposed mid-loop and are already decided.
    // Only iterate the integration extras through proposals here.
    let mut accepted_paths: Vec<String> = if writer_already_decided {
        writer_accepted.clone()
    } else {
        Vec::new()
    };
    let mut rejected_paths: Vec<String> = writer_rejected.clone();
    let extras_start = if writer_already_decided {
        writer_op_count
    } else {
        0
    };
    if !writer_already_decided {
        // Legacy path: writer returned the old envelope without going
        // through the loop. Propose every op (including writer's) here.
    }
    let total_ops = merged_result.operations.len();
    let to_propose: Vec<(usize, FileOperation)> = merged_result
        .operations
        .iter()
        .enumerate()
        .skip(extras_start)
        .map(|(i, op)| (i, op.clone()))
        .collect();

    let _ = (extras_count, total_ops); // silence unused-warnings on legacy path

    for (idx, operation) in to_propose {
        let proposal_id = Uuid::new_v4().to_string();
        let receiver = rt.proposals.register(proposal_id.clone());
        rt.bus
            .publish_event(&ClientEvent::FileProposal {
                job_id: job_id.clone(),
                proposal_id: proposal_id.clone(),
                operation: operation.clone(),
                review_notes: if verdict.reason.is_empty() {
                    None
                } else {
                    Some(verdict.reason.clone())
                },
            })
            .await?;

        let outcome = tokio::select! {
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
            () = poll_until_cancelled(&rt, &job_id) => None,
        };

        match outcome {
            None => {
                let _ = rt.proposals.resolve(&proposal_id, false);
                rejected_paths.push(operation.file.clone());
                for rest in merged_result.operations.iter().skip(idx + 1) {
                    rejected_paths.push(rest.file.clone());
                }
                return Err(JobCancelled.into());
            }
            Some(true) => accepted_paths.push(operation.file.clone()),
            Some(false) => rejected_paths.push(operation.file.clone()),
        }
    }

    if rt.job_cancel.is_cancelled(&job_id) {
        return Err(JobCancelled.into());
    }

    let summary = compose_summary(
        &rt,
        &job_id,
        &user_message,
        &history,
        &plan,
        &merged_result.operations,
        &accepted_paths,
        &rejected_paths,
        &verdict,
    )
    .await
    .unwrap_or_else(|_| {
        format!(
            "Done. Accepted: {}. Rejected: {}.",
            accepted_paths.join(", "),
            rejected_paths.join(", ")
        )
    });

    rt.bus
        .publish_event(&ClientEvent::AssistantMessage {
            job_id: job_id.clone(),
            text: summary,
        })
        .await?;
    Ok(())
}

async fn plan_job(
    rt: &AgentRuntime,
    job_id: &str,
    user_message: &str,
    history: &[ChatTurn],
) -> Result<PlannerOutput> {
    let prompt = build_prompt_with_history(history, user_message);
    rt.emit_prompt_estimate(job_id, Agent::Orchestrator, Some(PLANNER_SYSTEM), &prompt)
        .await;
    let raw = rt
        .ollama
        .generate(
            &rt.model_for(Agent::Orchestrator).await,
            Some(PLANNER_SYSTEM),
            &prompt,
            rt.config.ollama_num_ctx,
        )
        .await?;
    Ok(parse_plan(&raw).unwrap_or_else(|| PlannerOutput {
        plan: "Direct response to user".to_string(),
        final_answer: raw,
        ..Default::default()
    }))
}

/// Render `history` (up to MAX_HISTORY_TURNS most recent turns) followed by
/// the new user message in a stable, lightly-tagged format the planner can
/// parse without ambiguity.
fn build_prompt_with_history(history: &[ChatTurn], user_message: &str) -> String {
    if history.is_empty() {
        return user_message.to_string();
    }
    let start = history.len().saturating_sub(MAX_HISTORY_TURNS);
    let mut out = String::from("Conversation so far:\n");
    for turn in &history[start..] {
        let role = if turn.role.eq_ignore_ascii_case("assistant") {
            "Assistant"
        } else {
            "User"
        };
        out.push_str(&format!("{}: {}\n", role, turn.text.trim()));
    }
    out.push_str("\nNew user message:\n");
    out.push_str(user_message);
    out
}

/// User questions like "does the website work" must go through indexed files +
/// code writer—not a hallucinated final_answer—with concrete HTML/CSS hygiene.
fn user_expects_frontend_file_pass(message: &str) -> bool {
    let l = message.to_lowercase();
    let site_like = l.contains("website")
        || l.contains("webpage")
        || l.contains("web page")
        || l.contains("landing page")
        || l.contains("homepage");
    let css_like = l.contains("css")
        || l.contains("stylesheet")
        || l.contains("style sheet")
        || l.contains("styled");
    let completeness = l.contains("fully functional")
        || l.contains("fully working")
        || l.contains("works fully")
        || l.contains("work fully")
        || (l.contains("make sure")
            && (l.contains("work")
                || l.contains("full")
                || l.contains("website")
                || l.contains("site")))
        || l.contains("import") && (l.contains(" css") || l.contains(" stylesheet"));
    let broken_html =
        l.contains("html") && (l.contains("fix") || l.contains("broken") || l.contains("wrong"));

    let polish = l.contains(" polish")
        || l.contains("make it look")
        || l.contains("make it prettier")
        || l.contains("look better")
        || l.contains("visual")
        || l.contains("layout");
    let styling_terms =
        l.contains("styling") || l.contains("add style") || l.contains("decorate");

    site_like || css_like || completeness || broken_html || polish || styling_terms
}

fn enforce_frontend_audit_plan(mut plan: PlannerOutput, message: &str) -> PlannerOutput {
    if !user_expects_frontend_file_pass(message) {
        return plan;
    }
    plan.need_files = true;
    plan.need_code = true;
    const EXTRA: &str =
        "Present stylesheet `<link>` in HTML proves wiring ONLY — LOOK BETTER / polish still requires READING markup + stylesheet and APPLYING richer CSS selectors+declarations. Never treat link-check as task completion unless user asked LINK FIX ONLY.\n\
`read_file`/grep markup FIRST; tie CSS selectors to REAL class/id/data-* from that HTML — no stylesheet swap that ignores structure. PATCH with edits carrying ACTUAL typography, spacing, colors, layout — NEVER hollow COMMENT-ONLY deltas. Links use `<a href=\"...\">`, not href on `<button>`; add `<link>` in head ONLY when stylesheet was missing.\n\
**HTML↔CSS COUPLING:** For websites, every `.class` / `#id` in CSS MUST match attributes you add to HTML in the same pass (or use only selectors already in the markup). Never deliver CSS full of unused class rules while anchors/tags stay bare — update HTML with `class=\"...\"` as you style components.";
    let trimmed = plan.code_instruction.trim().to_string();
    plan.code_instruction = if trimmed.is_empty() {
        format!("Use indexer paths; open EVERY relevant HTML/CSS for this polish request BEFORE finishing. {}", EXTRA)
    } else {
        format!("{trimmed}\n\n{EXTRA}")
    };
    plan
}

/// Public for tests: pull a `PlannerOutput` out of raw model text. Returns
/// `None` when no JSON object can be located.
pub fn parse_plan(raw: &str) -> Option<PlannerOutput> {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(parsed) = serde_json::from_str::<PlannerOutput>(trimmed) {
        return Some(parsed);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

async fn poll_until_cancelled(rt: &AgentRuntime, job_id: &str) {
    loop {
        tokio::time::sleep(PROPOSAL_CANCEL_POLL).await;
        if rt.job_cancel.is_cancelled(job_id) {
            return;
        }
    }
}

async fn wait_for(
    rt: &AgentRuntime,
    job_id: &str,
    rx: &mut mpsc::Receiver<Message>,
    expected_task: &str,
    overall_timeout: Duration,
) -> Result<Message> {
    let deadline = tokio::time::Instant::now() + overall_timeout;
    loop {
        if rt.job_cancel.is_cancelled(job_id) {
            return Err(JobCancelled.into());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(anyhow::anyhow!("timed out waiting for `{expected_task}`"));
        }
        let tick = Duration::from_millis(250);
        let sleep_for = tick.min(deadline - now);

        tokio::select! {
            maybe_msg = rx.recv() => {
                match maybe_msg {
                    Some(msg) if msg.task == expected_task => return Ok(msg),
                    Some(msg) => {
                        tracing::warn!(
                            "orchestrator ignoring unexpected reply task `{}`",
                            msg.task
                        );
                    }
                    None => return Err(anyhow::anyhow!("reply channel closed")),
                }
            }
            () = tokio::time::sleep(sleep_for) => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn compose_summary(
    rt: &AgentRuntime,
    job_id: &str,
    user_message: &str,
    history: &[ChatTurn],
    plan: &PlannerOutput,
    operations: &[FileOperation],
    accepted: &[String],
    rejected: &[String],
    verdict: &ReviewVerdict,
) -> Result<String> {
    let mut prompt = String::new();
    if !history.is_empty() {
        let start = history.len().saturating_sub(MAX_HISTORY_TURNS);
        prompt.push_str("Conversation so far:\n");
        for turn in &history[start..] {
            let role = if turn.role.eq_ignore_ascii_case("assistant") {
                "Assistant"
            } else {
                "User"
            };
            prompt.push_str(&format!("{}: {}\n", role, turn.text.trim()));
        }
        prompt.push('\n');
    }
    prompt.push_str("User asked:\n");
    prompt.push_str(user_message);
    prompt.push_str("\n\nPlan executed:\n");
    prompt.push_str(&plan.plan);
    prompt.push_str("\n\nProposed operations:\n");
    for op in operations {
        prompt.push_str(&format!("- {:?} {}\n", op.action, op.file));
    }
    prompt.push_str(&format!(
        "\nAccepted: {}\nRejected: {}\n",
        accepted.join(", "),
        rejected.join(", ")
    ));
    if !verdict.reason.is_empty() {
        prompt.push_str(&format!("Review notes: {}\n", verdict.reason));
    }
    rt.emit_prompt_estimate(job_id, Agent::Orchestrator, Some(ANSWER_SYSTEM), &prompt)
        .await;
    rt.ollama
        .generate(
            &rt.model_for(Agent::Orchestrator).await,
            Some(ANSWER_SYSTEM),
            &prompt,
            rt.config.ollama_num_ctx,
        )
        .await
}
