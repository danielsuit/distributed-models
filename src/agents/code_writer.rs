//! Code Writer Agent.
//!
//! Receives specific coding tasks and produces complete files. We never emit
//! diffs or patches; every result has the exact relative path and full file
//! contents.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agents::AgentRuntime;
use crate::messages::{Agent, ExtensionEvent};

const POLL_TIMEOUT_SECS: f64 = 5.0;

const SYSTEM_PROMPT: &str = "You are a senior software engineer. You write complete, runnable files in response to a task. \
ALWAYS respond with a single JSON object of the form: {\"files\":[{\"path\":\"<relative path>\",\"content\":\"<entire file>\"}],\"summary\":\"<1-2 sentence summary>\"}. \
Never use diffs, patches, or partial code. Include all imports. If multiple files are required, list each one. The path must be relative to the workspace root and must use forward slashes. \
Do not wrap the JSON in markdown fences.";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProposedFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodeWriterResult {
    #[serde(default)]
    pub files: Vec<ProposedFile>,
    #[serde(default)]
    pub summary: String,
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
    let Some(message) = rt.bus.next_message(Agent::CodeWriter.queue(), POLL_TIMEOUT_SECS).await?
    else {
        return Ok(());
    };

    rt.bus
        .publish_event(&ExtensionEvent::AgentStatus {
            job_id: message.job_id.clone(),
            agent: Agent::CodeWriter,
            status: "writing code".into(),
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
        .unwrap_or("");
    let related_files = message
        .context
        .get("related_files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect::<Vec<_>>();

    let prompt = format!(
        "Workspace root: {workspace_root}\nRelevant files (paths): {related_files:?}\n\nTask: {task}\n\nReturn the JSON object now.",
    );

    let raw = match rt
        .ollama
        .generate(&rt.config.models.code_writer, Some(SYSTEM_PROMPT), &prompt)
        .await
    {
        Ok(r) => r,
        Err(err) => {
            rt.bus
                .publish_event(&ExtensionEvent::Error {
                    job_id: message.job_id.clone(),
                    message: format!("Code writer Ollama call failed: {err}"),
                })
                .await?;
            return Ok(());
        }
    };

    let result: CodeWriterResult = match parse_result(&raw) {
        Some(parsed) => parsed,
        None => {
            tracing::warn!("code writer returned unparseable output, falling back to single file");
            CodeWriterResult {
                files: vec![ProposedFile {
                    path: message
                        .context
                        .get("target_file")
                        .and_then(|v| v.as_str())
                        .unwrap_or("UNKNOWN.txt")
                        .to_string(),
                    content: raw.clone(),
                }],
                summary: "Could not parse JSON, returned raw model output.".into(),
            }
        }
    };

    let response = message
        .reply(message.from, "code_writer_result")
        .with_result(json!(result));
    rt.bus.dispatch(&response).await?;
    Ok(())
}

fn parse_result(raw: &str) -> Option<CodeWriterResult> {
    let trimmed = strip_code_fence(raw);
    if let Ok(parsed) = serde_json::from_str::<CodeWriterResult>(&trimmed) {
        return Some(parsed);
    }
    // The model sometimes returns prose around the JSON; take the first {...} block.
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    let candidate = &trimmed[start..=end];
    serde_json::from_str(candidate).ok()
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
