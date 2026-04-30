//! Command line client. Connects to a running backend over the websocket
//! endpoint and either sends a single chat message or just prints health
//! info.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::config::Config;
use crate::messages::ExtensionEvent;

/// Print the backend health endpoint.
pub async fn run_health(config: &Config) -> Result<()> {
    let url = format!("http://{}:{}/health", config.host, config.port);
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    println!("{status}");
    println!("{body}");
    Ok(())
}

/// Connect to the backend, send a single user message, and stream the
/// resulting agent events to stdout. Exits when the orchestrator emits
/// `JobComplete` or after `idle_timeout` of silence.
pub async fn run_chat(
    config: &Config,
    message: &str,
    workspace_root: Option<String>,
    auto_accept: bool,
    idle_timeout: Duration,
) -> Result<()> {
    let url = format!("ws://{}:{}/ws", config.host, config.port);
    let (mut stream, _) = connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;

    let payload = json!({
        "type": "user_message",
        "text": message,
        "workspace_root": workspace_root,
    });
    stream
        .send(WsMessage::Text(payload.to_string()))
        .await
        .context("sending user_message")?;

    println!("(connected to {url})");
    println!("> {message}\n");

    let mut current_job: Option<String> = None;
    loop {
        let next = tokio::time::timeout(idle_timeout, stream.next()).await;
        let event = match next {
            Ok(Some(Ok(WsMessage::Text(text)))) => text,
            Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) => {
                eprintln!("connection closed by server");
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(err))) => {
                eprintln!("websocket error: {err}");
                break;
            }
            Err(_) => {
                eprintln!("idle timeout reached after {idle_timeout:?}; exiting");
                break;
            }
        };

        let parsed: ExtensionEvent = match serde_json::from_str(&event) {
            Ok(e) => e,
            Err(err) => {
                eprintln!("could not parse event: {err}: {event}");
                continue;
            }
        };

        match &parsed {
            ExtensionEvent::AgentStatus { agent, status, job_id } => {
                current_job.get_or_insert(job_id.clone());
                println!("[{:?}] {}", agent, status);
            }
            ExtensionEvent::Log { agent, message, .. } => {
                println!("[{:?}] {}", agent, message);
            }
            ExtensionEvent::AssistantMessage { text, .. } => {
                println!("\nassistant:\n{text}\n");
            }
            ExtensionEvent::FileProposal {
                proposal_id,
                file,
                content,
                review_notes,
                ..
            } => {
                println!("--- proposed file: {file} ---");
                println!("{content}");
                if let Some(notes) = review_notes {
                    println!("--- review notes: {notes}");
                }
                if auto_accept {
                    let decision = json!({
                        "type": "proposal_decision",
                        "proposal_id": proposal_id,
                        "accepted": true,
                    });
                    stream.send(WsMessage::Text(decision.to_string())).await.ok();
                    println!("(auto-accepted)\n");
                } else {
                    let decision = prompt_decision(file)?;
                    let decision = json!({
                        "type": "proposal_decision",
                        "proposal_id": proposal_id,
                        "accepted": decision,
                    });
                    stream.send(WsMessage::Text(decision.to_string())).await.ok();
                    sleep(Duration::from_millis(50)).await;
                }
            }
            ExtensionEvent::Error { message, .. } => {
                eprintln!("error: {message}");
            }
            ExtensionEvent::JobComplete { job_id } => {
                if current_job.as_deref() == Some(job_id) || current_job.is_none() {
                    break;
                }
            }
        }
    }

    let _ = stream.send(WsMessage::Close(None)).await;
    Ok(())
}

fn prompt_decision(path: &str) -> Result<bool> {
    use std::io::Write;
    print!("Accept write to {path}? [y/N] ");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(buf.trim().to_lowercase().as_str(), "y" | "yes"))
}
