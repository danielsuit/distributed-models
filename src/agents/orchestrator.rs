//! Orchestrator Agent.
//!
//! Owns the lifecycle of every chat job. The main loop polls the orchestrator
//! inbox; when it receives a brand new request (`user_message` or `auto_fix`)
//! it spawns a per-job task. All other messages on the inbox are routed to
//! the corresponding job task via an in-process registry so each job can
//! orchestrate its sub-agents without stepping on the others.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::agents::code_writer::{CodeWriterResult, ProposedFile};
use crate::agents::review::ReviewVerdict;
use crate::agents::AgentRuntime;
use crate::messages::{Agent, ExtensionEvent, Message};

const POLL_TIMEOUT_SECS: f64 = 5.0;
const SUBTASK_TIMEOUT_SECS: u64 = 600;

const PLANNER_SYSTEM: &str = "You are an orchestrator. Decide whether to call helper agents and respond with strict JSON: \
{\"plan\":\"<short summary>\",\"need_files\":true|false,\"file_query\":\"<keywords>\",\"need_code\":true|false,\"target_file\":\"<relative path or empty>\",\"code_instruction\":\"<single instruction for the code writer>\",\"final_answer\":\"<answer to user when no code is required>\"}. \
Set need_code to true only when the user wants new or modified files. When need_code is false, fill final_answer.";

const ANSWER_SYSTEM: &str = "You are the orchestrator's final speaker. Summarise what happened for the user in friendly markdown. \
Mention each file that was written or rejected, and any open follow-ups. Keep it under 200 words.";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlannerOutput {
    #[serde(default)]
    plan: String,
    #[serde(default)]
    need_files: bool,
    #[serde(default)]
    file_query: String,
    #[serde(default)]
    need_code: bool,
    #[serde(default)]
    target_file: String,
    #[serde(default)]
    code_instruction: String,
    #[serde(default)]
    final_answer: String,
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
    let Some(message) = rt.bus.next_message(Agent::Orchestrator.queue(), POLL_TIMEOUT_SECS).await?
    else {
        return Ok(());
    };

    match message.task.as_str() {
        "user_message" | "auto_fix" => {
            spawn_job(rt.clone(), registry.clone(), message);
        }
        _ => {
            // Route reply to the appropriate job.
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
        if let Err(err) = outcome {
            tracing::error!("job {} failed: {err:?}", job_id);
            let _ = rt
                .bus
                .publish_event(&ExtensionEvent::Error {
                    job_id: job_id.clone(),
                    message: format!("Job failed: {err}"),
                })
                .await;
        }
        let _ = rt
            .bus
            .publish_event(&ExtensionEvent::JobComplete {
                job_id: job_id.clone(),
            })
            .await;
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

    rt.bus
        .publish_event(&ExtensionEvent::AgentStatus {
            job_id: job_id.clone(),
            agent: Agent::Orchestrator,
            status: "planning".into(),
        })
        .await?;

    let plan = plan_job(&rt, &user_message).await?;
    rt.bus
        .publish_event(&ExtensionEvent::Log {
            job_id: job_id.clone(),
            agent: Agent::Orchestrator,
            message: format!("Plan: {}", plan.plan),
        })
        .await?;

    let mut related_files: Vec<String> = Vec::new();

    if plan.need_files {
        rt.bus
            .publish_event(&ExtensionEvent::AgentStatus {
                job_id: job_id.clone(),
                agent: Agent::FileStructure,
                status: "scanning workspace".into(),
            })
            .await?;
        let mut request = Message::new(Agent::Orchestrator, Agent::FileStructure, "query")
            .with_context(json!({
                "query": plan.file_query,
                "workspace_root": workspace_root,
            }));
        request.job_id = job_id.clone();
        rt.bus.dispatch(&request).await?;

        let reply = wait_for(&mut replies, "query_result").await?;
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

    if !plan.need_code {
        let summary = if plan.final_answer.is_empty() {
            plan.plan.clone()
        } else {
            plan.final_answer.clone()
        };
        rt.bus
            .publish_event(&ExtensionEvent::AssistantMessage {
                job_id: job_id.clone(),
                text: summary,
            })
            .await?;
        return Ok(());
    }

    rt.bus
        .publish_event(&ExtensionEvent::AgentStatus {
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
        }));
    writer_request.job_id = job_id.clone();
    rt.bus.dispatch(&writer_request).await?;

    let writer_reply = wait_for(&mut replies, "code_writer_result").await?;
    let writer_result: CodeWriterResult = serde_json::from_value(writer_reply.result.clone())
        .unwrap_or(CodeWriterResult {
            files: Vec::new(),
            summary: String::from("Code writer returned no usable files."),
        });

    if writer_result.files.is_empty() {
        rt.bus
            .publish_event(&ExtensionEvent::AssistantMessage {
                job_id: job_id.clone(),
                text: writer_result.summary,
            })
            .await?;
        return Ok(());
    }

    rt.bus
        .publish_event(&ExtensionEvent::AgentStatus {
            job_id: job_id.clone(),
            agent: Agent::Review,
            status: "validating".into(),
        })
        .await?;

    let mut review_request = Message::new(Agent::Orchestrator, Agent::Review, "review")
        .with_context(json!({
            "code_writer_result": writer_result,
        }));
    review_request.job_id = job_id.clone();
    rt.bus.dispatch(&review_request).await?;
    let review_reply = wait_for(&mut replies, "review_result").await?;
    let verdict: ReviewVerdict = serde_json::from_value(review_reply.result.clone()).unwrap_or_default();

    if !verdict.approved {
        rt.bus
            .publish_event(&ExtensionEvent::Log {
                job_id: job_id.clone(),
                agent: Agent::Review,
                message: format!(
                    "Review rejected the proposal: {} (problems: {})",
                    verdict.notes,
                    verdict.problems.join(", ")
                ),
            })
            .await?;
    }

    let mut accepted_paths = Vec::new();
    let mut rejected_paths = Vec::new();
    for file in &writer_result.files {
        let proposal_id = Uuid::new_v4().to_string();
        let receiver = rt.proposals.register(proposal_id.clone());
        rt.bus
            .publish_event(&ExtensionEvent::FileProposal {
                job_id: job_id.clone(),
                proposal_id: proposal_id.clone(),
                file: file.path.clone(),
                content: file.content.clone(),
                review_notes: if verdict.notes.is_empty() {
                    None
                } else {
                    Some(verdict.notes.clone())
                },
            })
            .await?;
        match timeout(Duration::from_secs(60 * 30), receiver).await {
            Ok(Ok(true)) => accepted_paths.push(file.path.clone()),
            Ok(_) => rejected_paths.push(file.path.clone()),
            Err(_) => {
                rt.proposals.resolve(&proposal_id, false);
                rejected_paths.push(file.path.clone());
            }
        }
    }

    let summary = compose_summary(
        &rt,
        &user_message,
        &plan,
        &writer_result.files,
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
        .publish_event(&ExtensionEvent::AssistantMessage {
            job_id: job_id.clone(),
            text: summary,
        })
        .await?;
    Ok(())
}

async fn plan_job(rt: &AgentRuntime, user_message: &str) -> Result<PlannerOutput> {
    let raw = rt
        .ollama
        .generate(
            &rt.config.models.orchestrator,
            Some(PLANNER_SYSTEM),
            user_message,
        )
        .await?;
    Ok(parse_plan(&raw).unwrap_or_else(|| PlannerOutput {
        plan: "Direct response to user".to_string(),
        need_files: false,
        file_query: String::new(),
        need_code: false,
        target_file: String::new(),
        code_instruction: String::new(),
        final_answer: raw,
    }))
}

fn parse_plan(raw: &str) -> Option<PlannerOutput> {
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

async fn wait_for(rx: &mut mpsc::Receiver<Message>, expected_task: &str) -> Result<Message> {
    let fut = async {
        loop {
            match rx.recv().await {
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
    };
    timeout(Duration::from_secs(SUBTASK_TIMEOUT_SECS), fut)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for `{expected_task}`"))?
}

#[allow(clippy::too_many_arguments)]
async fn compose_summary(
    rt: &AgentRuntime,
    user_message: &str,
    plan: &PlannerOutput,
    files: &[ProposedFile],
    accepted: &[String],
    rejected: &[String],
    verdict: &ReviewVerdict,
) -> Result<String> {
    let mut prompt = String::new();
    prompt.push_str("User asked:\n");
    prompt.push_str(user_message);
    prompt.push_str("\n\nPlan executed:\n");
    prompt.push_str(&plan.plan);
    prompt.push_str("\n\nProposed files:\n");
    for f in files {
        prompt.push_str(&format!("- {}\n", f.path));
    }
    prompt.push_str(&format!("\nAccepted: {}\nRejected: {}\n", accepted.join(", "), rejected.join(", ")));
    if !verdict.notes.is_empty() {
        prompt.push_str(&format!("Review notes: {}\n", verdict.notes));
    }
    rt.ollama
        .generate(&rt.config.models.orchestrator, Some(ANSWER_SYSTEM), &prompt)
        .await
}
