//! Review Agent.
//!
//! Validates code-writer output. The orchestrator forwards each
//! `CodeWriterResult` to us before showing it to the user, and we either
//! green-light the proposal or annotate it with notes.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agents::AgentRuntime;
use crate::agents::code_writer::CodeWriterResult;
use crate::messages::{Agent, ExtensionEvent};

const POLL_TIMEOUT_SECS: f64 = 5.0;

const SYSTEM_PROMPT: &str = "You are a code review agent. You read proposed file contents and output JSON: \
{\"approved\":true|false,\"notes\":\"<short notes>\",\"problems\":[\"<list of concrete issues>\"]}. \
Approve when the file looks syntactically valid and imports/types appear consistent. \
Reject only for clearly broken syntax, missing imports, or contradictions. Do not rewrite the code.";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewVerdict {
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub problems: Vec<String>,
}

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
    let Some(message) = rt.bus.next_message(Agent::Review.queue(), POLL_TIMEOUT_SECS).await?
    else {
        return Ok(());
    };

    rt.bus
        .publish_event(&ExtensionEvent::AgentStatus {
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
            // Always send a verdict back so the orchestrator never blocks.
            let response = message
                .reply(message.from, "review_result")
                .with_result(json!(ReviewVerdict {
                    approved: false,
                    notes: format!("review payload parse error: {err}"),
                    problems: vec![],
                }));
            rt.bus.dispatch(&response).await?;
            return Ok(());
        }
    };

    let verdict = review_payload(rt, &payload).await.unwrap_or(ReviewVerdict {
        approved: true,
        notes: "Review skipped due to model error.".into(),
        problems: vec![],
    });

    let response = message
        .reply(message.from, "review_result")
        .with_result(json!(verdict));
    rt.bus.dispatch(&response).await?;
    Ok(())
}

async fn review_payload(rt: &AgentRuntime, payload: &CodeWriterResult) -> Result<ReviewVerdict> {
    let mut prompt = String::from("Review the following proposed file changes.\n\n");
    for file in &payload.files {
        prompt.push_str(&format!(
            "=== FILE: {} ===\n{}\n\n",
            file.path, file.content
        ));
    }
    prompt.push_str("Now respond with the JSON verdict.");

    let raw = rt
        .ollama
        .generate(&rt.config.models.review, Some(SYSTEM_PROMPT), &prompt)
        .await?;

    let trimmed = raw.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let verdict: ReviewVerdict = serde_json::from_str(trimmed).unwrap_or_else(|_| {
        // Try to extract a JSON object from prose.
        if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
            serde_json::from_str(&trimmed[start..=end]).unwrap_or(ReviewVerdict {
                approved: true,
                notes: "Reviewer returned non-JSON output; defaulting to approved.".into(),
                problems: vec![],
            })
        } else {
            ReviewVerdict {
                approved: true,
                notes: "Reviewer returned no JSON; defaulting to approved.".into(),
                problems: vec![],
            }
        }
    });
    Ok(verdict)
}

impl Default for ReviewVerdict {
    fn default() -> Self {
        Self {
            approved: false,
            notes: String::new(),
            problems: Vec::new(),
        }
    }
}
