//! Command line client. Uses the public REST + Server-Sent Events API so it
//! exercises the same surface a third-party editor would use.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::messages::{ChatRequest, ChatResponse, ClientEvent, FileAction};

/// Base URL helper.
fn base_url(config: &Config) -> String {
    format!("http://{}:{}", config.host, config.port)
}

/// Print the backend health endpoint.
pub async fn run_health(config: &Config) -> Result<()> {
    let url = format!("{}/health", base_url(config));
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    println!("{status}");
    println!("{body}");
    Ok(())
}

/// Send a single chat message via REST and follow the SSE event stream until
/// `JobComplete` or `idle_timeout` of silence.
pub async fn run_chat(
    config: &Config,
    message: &str,
    workspace_root: Option<String>,
    auto_accept: bool,
    idle_timeout: Duration,
) -> Result<()> {
    let http = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("building reqwest client")?;
    let base = base_url(config);

    let chat_url = format!("{}/chat", base);
    let chat_response: ChatResponse = http
        .post(&chat_url)
        .json(&ChatRequest {
            text: message.to_string(),
            workspace_root,
            history: Vec::new(),
        })
        .send()
        .await
        .with_context(|| format!("posting to {chat_url}"))?
        .error_for_status()?
        .json()
        .await
        .context("decoding chat response")?;

    println!("(connected to {})", base);
    println!("> {message}");
    println!("(job_id={})\n", chat_response.job_id);

    let events_url = format!("{}/events?job_id={}", base, chat_response.job_id);
    let response = http
        .get(&events_url)
        .send()
        .await
        .with_context(|| format!("opening event stream {events_url}"))?
        .error_for_status()?;

    // Convert the chunked HTTP body into discrete SSE event payloads.
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let stream_task = tokio::spawn(async move {
        let mut bytes = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            let Ok(text) = std::str::from_utf8(&chunk) else {
                continue;
            };
            buffer.push_str(text);
            // SSE separates events with a blank line (\n\n).
            while let Some(idx) = buffer.find("\n\n") {
                let event_block = buffer[..idx].to_string();
                buffer.drain(..idx + 2);
                if let Some(payload) = parse_sse_data(&event_block) {
                    if tx.send(payload).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    loop {
        let next = tokio::time::timeout(idle_timeout, rx.recv()).await;
        let payload = match next {
            Ok(Some(payload)) => payload,
            Ok(None) => {
                eprintln!("event stream closed by server");
                break;
            }
            Err(_) => {
                eprintln!("idle timeout reached after {idle_timeout:?}; exiting");
                break;
            }
        };

        let parsed: ClientEvent = match serde_json::from_str(&payload) {
            Ok(e) => e,
            Err(err) => {
                eprintln!("could not parse event: {err}: {payload}");
                continue;
            }
        };

        match &parsed {
            ClientEvent::AgentStatus { agent, status, .. } => {
                println!("[{}] {}", agent.label(), status);
            }
            ClientEvent::Log { agent, message, .. } => {
                println!("[{}] {}", agent.label(), message);
            }
            ClientEvent::AssistantMessage { text, .. } => {
                println!("\nassistant:\n{text}\n");
            }
            ClientEvent::FileProposal {
                proposal_id,
                operation,
                review_notes,
                ..
            } => {
                let action_label = match operation.action {
                    FileAction::Create => "create",
                    FileAction::Edit => "edit",
                    FileAction::Delete => "delete",
                };
                println!("--- proposed {action_label}: {} ---", operation.file);
                if let Some(content) = &operation.content {
                    println!("{content}");
                }
                if let Some(notes) = review_notes {
                    println!("--- review notes: {notes}");
                }
                let accepted = if auto_accept {
                    println!("(auto-accepted)\n");
                    true
                } else {
                    prompt_decision(&operation.file, action_label)?
                };
                let url = format!("{}/proposal/{}", base, proposal_id);
                http.post(&url)
                    .json(&json!({ "accepted": accepted }))
                    .send()
                    .await
                    .ok();
            }
            ClientEvent::CommandProposal {
                proposal_id,
                command,
                cwd,
                ..
            } => {
                println!("--- proposed bash command ---");
                if let Some(cwd) = cwd {
                    println!("cwd: {cwd}");
                }
                println!("$ {command}");
                let accepted = if auto_accept {
                    println!("(auto-accepted)\n");
                    true
                } else {
                    prompt_decision(command, "run")?
                };
                let url = format!("{}/proposal/{}", base, proposal_id);
                http.post(&url)
                    .json(&json!({ "accepted": accepted }))
                    .send()
                    .await
                    .ok();
            }
            ClientEvent::CommandResult {
                exit_code,
                stdout,
                stderr,
                truncated,
                ..
            } => {
                if !stdout.is_empty() {
                    println!("--- stdout ---\n{stdout}");
                }
                if !stderr.is_empty() {
                    println!("--- stderr ---\n{stderr}");
                }
                println!(
                    "--- exit: {} ---{}\n",
                    exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                    if *truncated { " [truncated]" } else { "" }
                );
            }
            ClientEvent::Error { message, .. } => {
                eprintln!("error: {message}");
            }
            ClientEvent::PromptEstimate {
                agent,
                approximate_tokens,
                ..
            } => {
                println!(
                    "[{}] ~{} tok (backend prompt heuristic)",
                    agent.label(),
                    approximate_tokens
                );
            }
            ClientEvent::JobComplete { .. } => {
                break;
            }
        }
    }

    stream_task.abort();
    Ok(())
}

/// Public for tests: pull a single SSE `data:` payload out of an event block.
/// Multiple `data:` lines in the same block are concatenated with newlines
/// per the SSE spec; comment lines (starting with `:`) are ignored.
pub fn parse_sse_data(event_block: &str) -> Option<String> {
    let mut data_lines: Vec<&str> = Vec::new();
    for raw in event_block.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

fn prompt_decision(path: &str, action: &str) -> Result<bool> {
    use std::io::Write;
    print!("Accept {action} of {path}? [y/N] ");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(buf.trim().to_lowercase().as_str(), "y" | "yes"))
}
