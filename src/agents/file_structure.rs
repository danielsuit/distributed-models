//! File Structure Agent.
//!
//! Maintains a live JSON map of the workspace and answers requests for
//! relevant paths. The editor's file watcher feeds us snapshots and
//! incremental changes; the orchestrator can ask us for a ranked list of
//! candidate paths.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

use crate::agents::AgentRuntime;
use crate::messages::{Agent, ClientEvent, FileChange, FileEntry, Message};
use std::path::Path;
use tokio::fs;

const STATE_KEY: &str = "state:filestructure";
const POLL_TIMEOUT_SECS: f64 = 5.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceState {
    pub root: Option<String>,
    /// Path -> entry. We keep this as a BTreeMap so serialised state is
    /// stable across runs (helps debugging and consumer caching).
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
    let Some(message) = rt
        .bus
        .next_message(Agent::FileStructure.queue(), POLL_TIMEOUT_SECS)
        .await?
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
    for mut file in files {
        if !file.is_dir && file.path.ends_with(".rs") {
            if let Some(ref root) = workspace_root {
                let full_path = Path::new(root).join(&file.path);
                if let Ok(code) = fs::read_to_string(&full_path).await {
                    file.symbols = Some(parse_rust_symbols(&code));
                }
            }
        }
        state.files.insert(file.path.clone(), file);
    }
    save_state(rt, &state).await?;

    rt.bus
        .publish_event(&ClientEvent::Log {
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
                let mut symbols = None;
                if path.ends_with(".rs") {
                    if let Some(ref root) = state.root {
                        let full_path = Path::new(root).join(&path);
                        if let Ok(code) = fs::read_to_string(&full_path).await {
                            symbols = Some(parse_rust_symbols(&code));
                        }
                    }
                }
                state.files.insert(
                    path.clone(),
                    FileEntry {
                        path,
                        size: 0,
                        is_dir: false,
                        symbols,
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
        .trim()
        .to_lowercase();

    let indexed_files = state.files.len();
    let has_non_dir_files = state.files.values().any(|e| !e.is_dir);

    let keyword_empty = query.is_empty();

    let mut candidates: Vec<&FileEntry> = state
        .files
        .values()
        .filter(|entry| {
            !entry.is_dir && (keyword_empty || entry.path.to_lowercase().contains(&query))
        })
        .collect();

    // Planner wording ("review entire website professionally") rarely appears in
    // relative paths (`index.html`, `src/App.tsx`). Falling back avoids "0 paths"
    // when the workspace is actually indexed.
    if candidates.is_empty() && has_non_dir_files && !keyword_empty {
        tracing::debug!(
            "file_query `{query}` matched no paths ({indexed_files} indexed); using broad file list"
        );
        candidates = state.files.values().filter(|entry| !entry.is_dir).collect();
    }

    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates.truncate(120);

    let mut ranked: Vec<String> = candidates.iter().map(|c| c.path.clone()).collect();
    if !query.is_empty() && ranked.len() > 8 {
        if let Ok(suggestion) =
            ask_model_to_rank(rt, &message.job_id, &query, &ranked).await {
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

    let match_count = response.result["matches"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    let log_msg = if !has_non_dir_files {
        format!(
            "Returned {match_count} candidate paths — workspace index is empty. Open a folder in the editor (or reopen the workspace) so the Distributed Models extension can snapshot files."
        )
    } else if match_count == 0 && keyword_empty {
        format!("Returned {match_count} candidate paths (indexed {indexed_files} entries, none are files)")
    } else {
        format!("Returned {match_count} candidate paths")
    };

    rt.bus
        .publish_event(&ClientEvent::Log {
            job_id: message.job_id.clone(),
            agent: Agent::FileStructure,
            message: log_msg,
        })
        .await?;
    Ok(())
}

async fn ask_model_to_rank(
    rt: &AgentRuntime,
    job_id: &str,
    query: &str,
    candidates: &[String],
) -> Result<Vec<String>> {
    let prompt = format!(
        "You rank file paths by relevance to a coding task. Return at most 12 paths, one per line, no commentary.\n\nTask: {query}\n\nCandidate paths:\n{}\n",
        candidates.join("\n")
    );
    let system = "You are a precise file-relevance ranker.";
    rt.emit_prompt_estimate(job_id, Agent::FileStructure, Some(system), &prompt)
        .await;
    let raw = rt
        .ollama
        .generate(
            &rt.model_for(Agent::FileStructure).await,
            Some(system),
            &prompt,
            rt.config.ollama_num_ctx,
        )
        .await?;
    Ok(parse_ranked_paths(&raw, candidates))
}

/// Public for tests: split a model's response into ranked paths and keep
/// only those that match the candidate list.
pub fn parse_ranked_paths(raw: &str, candidates: &[String]) -> Vec<String> {
    let strippable: &[char] = &[
        '-', '*', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', ' ', '`',
    ];
    let ranked: Vec<String> = raw
        .lines()
        .map(|l| l.trim().trim_start_matches(strippable).to_string())
        .filter(|l| !l.is_empty())
        .filter(|l| candidates.iter().any(|c| c == l))
        .collect();
    if ranked.is_empty() {
        candidates.to_vec()
    } else {
        ranked
    }
}

fn parse_rust_symbols(code: &str) -> Vec<String> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&tree_sitter_rust::language()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(code, None) {
        Some(t) => t,
        None => return vec![],
    };
    let query_str = r#"
        (function_item name: (identifier) @name)
        (struct_item name: (type_identifier) @name)
        (impl_item type: (type_identifier) @name)
        (enum_item name: (type_identifier) @name)
        (trait_item name: (type_identifier) @name)
    "#;
    let query = match tree_sitter::Query::new(&tree_sitter_rust::language(), query_str) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut symbols = Vec::new();
    for m in cursor.matches(&query, tree.root_node(), code.as_bytes()) {
        for cap in m.captures {
            if let Ok(text) = cap.node.utf8_text(code.as_bytes()) {
                let t = text.to_string();
                if !symbols.contains(&t) {
                    symbols.push(t);
                }
            }
        }
    }
    symbols
}
