//! Error Agent.
//!
//! Watches diagnostics streamed in from the VS Code extension and triggers a
//! fix request whenever the workspace transitions from "no errors" to
//! "errors". It then sends a high-priority follow-up task into the
//! orchestrator inbox so a fix can be proposed automatically.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::json;

use crate::agents::AgentRuntime;
use crate::messages::{Agent, DiagnosticEntry, ExtensionEvent, Message};

const STATE_KEY: &str = "state:diagnostics";
const POLL_TIMEOUT_SECS: f64 = 5.0;

const SYSTEM_PROMPT: &str = "You are a senior debugging assistant. Given a list of compiler/lint errors, \
produce a SHORT plain-English plan describing how the orchestrator should ask the code writer to fix them. \
Keep it under 5 sentences and focus on the most-impactful fix first.";

pub async fn run(rt: AgentRuntime) {
    tracing::info!("error agent online");
    loop {
        match step(&rt).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("error agent crashed: {err:?}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn step(rt: &AgentRuntime) -> Result<()> {
    let Some(message) = rt.bus.next_message(Agent::ErrorAgent.queue(), POLL_TIMEOUT_SECS).await?
    else {
        return Ok(());
    };

    if message.task != "diagnostics" {
        tracing::warn!("error agent received unknown task: {}", message.task);
        return Ok(());
    }

    let workspace_root = message
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let diagnostics: Vec<DiagnosticEntry> = serde_json::from_value(
        message
            .context
            .get("diagnostics")
            .cloned()
            .unwrap_or(json!([])),
    )
    .unwrap_or_default();

    let serious = diagnostics
        .iter()
        .filter(|d| d.severity.eq_ignore_ascii_case("error"))
        .cloned()
        .collect::<Vec<_>>();

    // Persist the latest diagnostics map so other agents can read it.
    let mut grouped: BTreeMap<String, Vec<DiagnosticEntry>> = BTreeMap::new();
    for diag in &diagnostics {
        grouped.entry(diag.file.clone()).or_default().push(diag.clone());
    }
    rt.bus
        .set_string(STATE_KEY, &serde_json::to_string(&grouped)?)
        .await?;

    if serious.is_empty() {
        return Ok(());
    }

    rt.bus
        .publish_event(&ExtensionEvent::Log {
            job_id: message.job_id.clone(),
            agent: Agent::ErrorAgent,
            message: format!("Detected {} error(s); asking orchestrator for a fix.", serious.len()),
        })
        .await?;

    let plan = build_plan(rt, &serious).await.unwrap_or_else(|_| {
        format!(
            "Fix the following errors in priority order: {}.",
            serious
                .iter()
                .take(3)
                .map(|d| format!("{} ({}:{} - {})", d.message, d.file, d.line, d.severity))
                .collect::<Vec<_>>()
                .join("; ")
        )
    });

    // Forward an actionable task to the orchestrator.
    let mut follow_up = Message::new(
        Agent::ErrorAgent,
        Agent::Orchestrator,
        "auto_fix",
    )
    .with_context(json!({
        "user_message": format!(
            "Diagnostics agent detected new errors. Plan: {plan}\n\nErrors: {serious:?}"
        ),
        "workspace_root": workspace_root,
        "diagnostics": serious,
        "auto": true,
    }));
    follow_up.job_id = message.job_id.clone();
    rt.bus.dispatch(&follow_up).await?;
    Ok(())
}

async fn build_plan(rt: &AgentRuntime, diagnostics: &[DiagnosticEntry]) -> Result<String> {
    let mut prompt = String::from("Errors:\n");
    for d in diagnostics {
        prompt.push_str(&format!(
            "- {} at {}:{}:{} ({})\n",
            d.message, d.file, d.line, d.column, d.severity
        ));
    }
    prompt.push_str("\nWrite the plan now.");
    rt.ollama
        .generate(&rt.config.models.error_agent, Some(SYSTEM_PROMPT), &prompt)
        .await
}
