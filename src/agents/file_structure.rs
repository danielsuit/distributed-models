//! File Structure Agent.
//!
//! Maintains a live JSON map of the workspace and answers requests for
//! relevant paths. The watcher in the VS Code extension feeds us snapshots
//! and incremental changes; the orchestrator (or any agent) can ask us for a
//! summary of the workspace.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

use crate::agents::AgentRuntime;
use crate::messages::{Agent, ExtensionEvent, FileChange, FileEntry, Message};

const STATE_KEY: &str = "state:filestructure";
const POLL_TIMEOUT_SECS: f64 = 5.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub root: Option<String>,
    /// Path -> entry. We keep this as a BTreeMap so serialised state is stable
    /// across runs (helps debugging and consumer caching).
    pub files: BTreeMap<String, FileEntry>,
}

pub async fn run(rt: AgentRuntime) {
    tracing::info!("file structure agent online");
    loop {
        match step(&rt).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("file structure agent error: {err:?}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn step(rt: &AgentRuntime) -> Result<()> {
    let Some(message) = rt.bus.next_message(Agent::FileStructure.queue(), POLL_TIMEOUT_SECS).await?
    else {
        return Ok(());
    };

    match message.task.as_str() {
        "snapshot" => handle_snapshot(rt, message).await,
        "created" | "changed" | "deleted" => handle_change(rt, message).await,
        "query" => handle_query(rt, message).await,
        other => {
            tracing::warn!("file structure agent received unknown task: {other}");
            Ok(())
        }
    }
}

async fn load_state(rt: &AgentRuntime) -> Result<WorkspaceState> {
    Ok(match rt.bus.get_string(STATE_KEY).await? {
        Some(payload) => serde_json::from_str(&payload).unwrap_or_default(),
        None => WorkspaceState::default(),
    })
}

async fn save_state(rt: &AgentRuntime, state: &WorkspaceState) -> Result<()> {
    rt.bus
        .set_string(STATE_KEY, &serde_json::to_string(state)?)
        .await
}

async fn handle_snapshot(rt: &AgentRuntime, message: Message) -> Result<()> {
    let workspace_root = message
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let files: Vec<FileEntry> = message
        .context
        .get("files")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();

    let mut state = WorkspaceState {
        root: workspace_root.clone(),
        files: BTreeMap::new(),
    };
    for file in files {
        state.files.insert(file.path.clone(), file);
    }
    save_state(rt, &state).await?;

    rt.bus
        .publish_event(&ExtensionEvent::Log {
            job_id: message.job_id.clone(),
            agent: Agent::FileStructure,
            message: format!(
                "Indexed {} entries from {}",
                state.files.len(),
                workspace_root.as_deref().unwrap_or("<unknown>")
            ),
        })
        .await?;
    Ok(())
}

async fn handle_change(rt: &AgentRuntime, message: Message) -> Result<()> {
    let mut state = load_state(rt).await?;
    if let Some(root) = message
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
    {
        state.root = Some(root.to_string());
    }

    let change: Option<FileChange> = message
        .context
        .get("change")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;

    if let Some(change) = change {
        match change {
            FileChange::Created { path } | FileChange::Changed { path } => {
                state.files.insert(
                    path.clone(),
                    FileEntry {
                        path,
                        size: 0,
                        is_dir: false,
                    },
                );
            }
            FileChange::Deleted { path } => {
                state.files.remove(&path);
            }
        }
    }

    save_state(rt, &state).await?;
    Ok(())
}

async fn handle_query(rt: &AgentRuntime, message: Message) -> Result<()> {
    let state = load_state(rt).await?;
    let query = message
        .context
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    // Simple substring match over the path list. The 3b model is invoked only
    // when the orchestrator wants ranking on top of this fast pre-filter.
    let mut candidates: Vec<&FileEntry> = state
        .files
        .values()
        .filter(|entry| {
            !entry.is_dir && (query.is_empty() || entry.path.to_lowercase().contains(&query))
        })
        .collect();
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates.truncate(80);

    // Ask the small model to rank when there is something to choose between.
    let mut ranked: Vec<String> = candidates.iter().map(|c| c.path.clone()).collect();
    if !query.is_empty() && ranked.len() > 8 {
        if let Ok(suggestion) = ask_model_to_rank(rt, &query, &ranked).await {
            ranked = suggestion;
        }
    }

    let response = message
        .reply(message.from, "query_result")
        .with_result(json!({
            "root": state.root,
            "matches": ranked,
            "total_indexed": state.files.len(),
        }));
    rt.bus.dispatch(&response).await?;

    rt.bus
        .publish_event(&ExtensionEvent::Log {
            job_id: message.job_id.clone(),
            agent: Agent::FileStructure,
            message: format!("Returned {} candidate paths", response.result["matches"].as_array().map(|a| a.len()).unwrap_or(0)),
        })
        .await?;
    Ok(())
}

async fn ask_model_to_rank(
    rt: &AgentRuntime,
    query: &str,
    candidates: &[String],
) -> Result<Vec<String>> {
    let prompt = format!(
        "You rank file paths by relevance to a coding task. Return at most 12 paths, one per line, no commentary.\n\nTask: {query}\n\nCandidate paths:\n{}\n",
        candidates.join("\n")
    );
    let raw = rt
        .ollama
        .generate(
            &rt.config.models.file_structure,
            Some("You are a precise file-relevance ranker."),
            &prompt,
        )
        .await?;
    let ranked: Vec<String> = raw
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', ' ']).to_string())
        .filter(|l| !l.is_empty())
        .filter(|l| candidates.iter().any(|c| c == l))
        .collect();
    if ranked.is_empty() {
        Ok(candidates.to_vec())
    } else {
        Ok(ranked)
    }
}
