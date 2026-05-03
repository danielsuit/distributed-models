use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A logical agent on the bus. The string representations match the
/// `agent:<name>` queue suffixes used by the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Orchestrator,
    FileStructure,
    CodeWriter,
    ErrorAgent,
    Review,
    /// Ensures new files are wired in (CSS imports, router entries, nav links).
    Integration,
    /// Outside-the-bus client (the editor, the CLI, anything driving us).
    Client,
}

impl Agent {
    pub fn queue(self) -> &'static str {
        use crate::config::queues;
        match self {
            Agent::Orchestrator => queues::ORCHESTRATOR,
            Agent::FileStructure => queues::FILE_STRUCTURE,
            Agent::CodeWriter => queues::CODE_WRITER,
            Agent::ErrorAgent => queues::ERROR_AGENT,
            Agent::Review => queues::REVIEW,
            Agent::Integration => queues::INTEGRATION,
            Agent::Client => queues::EVENTS_CHANNEL,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Agent::Orchestrator => "Orchestrator",
            Agent::FileStructure => "File Structure",
            Agent::CodeWriter => "Code Writer",
            Agent::ErrorAgent => "Error Agent",
            Agent::Review => "Review",
            Agent::Integration => "Integration",
            Agent::Client => "Client",
        }
    }
}

/// Wire format used between agents. Mirrors the JSON contract in the spec
/// (`{ id, from, to, task, context, result }`) plus a few correlation fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    /// All sub-messages spawned for the same user request share a job id so
    /// the orchestrator can correlate replies back to the originating chat.
    #[serde(default)]
    pub job_id: String,
    pub from: Agent,
    pub to: Agent,
    pub task: String,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub result: serde_json::Value,
    #[serde(default = "Utc::now")]
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Message {
    pub fn new(from: Agent, to: Agent, task: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            job_id: Uuid::new_v4().to_string(),
            from,
            to,
            task: task.into(),
            context: serde_json::Value::Null,
            result: serde_json::Value::Null,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        }
    }

    pub fn with_job(mut self, job_id: impl Into<String>) -> Self {
        self.job_id = job_id.into();
        self
    }

    pub fn with_context(mut self, context: serde_json::Value) -> Self {
        self.context = context;
        self
    }

    pub fn with_result(mut self, result: serde_json::Value) -> Self {
        self.result = result;
        self
    }

    /// Build a reply to this message addressed to `to`. The job id, context
    /// and metadata flow through; the caller fills in `result` afterwards.
    pub fn reply(&self, to: Agent, task: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            job_id: self.job_id.clone(),
            from: self.to,
            to,
            task: task.into(),
            context: self.context.clone(),
            result: serde_json::Value::Null,
            timestamp: Utc::now(),
            metadata: self.metadata.clone(),
        }
    }
}

/// File operation produced by the code-writer agent. The shape is the exact
/// `{action, file, content}` contract the spec requires; `content` is omitted
/// for `delete` actions but kept for create/edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileAction {
    Create,
    Edit,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOperation {
    pub action: FileAction,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl FileOperation {
    pub fn create(file: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            action: FileAction::Create,
            file: file.into(),
            content: Some(content.into()),
        }
    }

    pub fn edit(file: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            action: FileAction::Edit,
            file: file.into(),
            content: Some(content.into()),
        }
    }

    pub fn delete(file: impl Into<String>) -> Self {
        Self {
            action: FileAction::Delete,
            file: file.into(),
            content: None,
        }
    }
}

/// Bundle of operations the code writer returns. The spec shows the per-file
/// shape; we wrap them in this envelope so a single response can carry
/// multiple operations, an explanation, and (for the new tool-use loop)
/// the list of paths the user rejected mid-run so the orchestrator can
/// surface them in the final assistant message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeWriterResult {
    #[serde(default)]
    pub operations: Vec<FileOperation>,
    #[serde(default)]
    pub summary: String,
    /// Paths the user rejected during the loop (informational; never
    /// re-proposed by the orchestrator). Defaults to empty so older
    /// fixtures and the legacy envelope keep deserialising cleanly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_paths: Vec<String>,
    /// True when the writer already proposed every op it returns and the
    /// user has decided each one. The orchestrator uses this to skip the
    /// post-loop "iterate operations through proposals" phase.
    #[serde(default, skip_serializing_if = "is_false")]
    pub already_decided: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Verdict returned by the review agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub problems: Vec<String>,
}

/// Sidebar-style heuristic (~4 UTF-8 bytes ≈ one token) for telemetry only.
#[inline]
pub fn approximate_llm_turn_tokens_utf8(system: Option<&str>, prompt: &str) -> u32 {
    let chars = prompt
        .len()
        .saturating_add(system.map(str::len).unwrap_or(0));
    chars.div_ceil(4).min(u32::MAX as usize) as u32
}

/// Events streamed back to clients (editor or CLI) over Server-Sent Events.
/// They are also published on the Redis `events:client` channel so any agent
/// can fan-out updates without going through the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    /// A new agent started working on the active job.
    AgentStatus {
        job_id: String,
        agent: Agent,
        status: String,
    },
    /// Free-form log line surfaced in the sidebar's activity feed.
    Log {
        job_id: String,
        agent: Agent,
        message: String,
    },
    /// The orchestrator (or any agent) finished and produced an answer.
    AssistantMessage { job_id: String, text: String },
    /// A file operation proposal that needs Accept/Reject by the client.
    FileProposal {
        job_id: String,
        proposal_id: String,
        operation: FileOperation,
        review_notes: Option<String>,
    },
    /// A shell command the agent wants to run. Same accept/reject contract
    /// as `FileProposal` — the client posts to `/proposal/:id` to decide.
    CommandProposal {
        job_id: String,
        proposal_id: String,
        command: String,
        cwd: Option<String>,
    },
    /// Streamed output (stdout/stderr/exit) from an accepted bash command.
    /// Surfaced after `CommandProposal` is accepted so the UI can render
    /// the result inline; the agent loop also receives this verbatim.
    CommandResult {
        job_id: String,
        proposal_id: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
    /// Something went wrong; surfaced as an error bubble in the sidebar.
    Error { job_id: String, message: String },
    /// Approximate size of text sent as one Ollama `generate` round (system + prompt),
    /// for UI context meters (`chars÷4`; not a tokenizer count).
    PromptEstimate {
        job_id: String,
        agent: Agent,
        approximate_tokens: u32,
    },
    /// The orchestrator finished the job and the chat can accept new input.
    JobComplete { job_id: String },
}

/// File entry reported by the editor's file watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChange {
    Created { path: String },
    Changed { path: String },
    Deleted { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEntry {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// One turn of conversational memory sent alongside a chat request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatTurn {
    /// "user" or "assistant".
    pub role: String,
    pub text: String,
}

/// Body for `POST /chat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub text: String,
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// Recent turns of conversation to give the orchestrator memory.
    /// Most recent turn last. Empty for fresh sessions.
    #[serde(default)]
    pub history: Vec<ChatTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotRequest {
    pub workspace_root: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChangeRequest {
    pub workspace_root: String,
    pub change: FileChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsRequest {
    pub workspace_root: String,
    pub diagnostics: Vec<DiagnosticEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalDecisionRequest {
    pub accepted: bool,
}

/// Config payload surfaced to UI/CLI so users can edit model assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfigResponse {
    pub host: String,
    pub port: u16,
    pub redis_url: String,
    pub ollama_endpoint: String,
    pub models: ModelAssignmentsPayload,
    /// `num_ctx` we attach to each Ollama `generate` call (from config).
    pub ollama_num_ctx: u32,
    /// GGUF-derived context cap via Ollama `POST /api/show` (`*.context_length`), when reachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_native: Option<u32>,
    /// What the sidebar meter divides against—`native.min(ollama_num_ctx)` when `native`
    /// is known, otherwise `ollama_num_ctx`. Chat is orchestrator-driven; this matches that model.
    pub context_window_effective: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAssignmentsPayload {
    pub orchestrator: String,
    pub file_structure: String,
    pub code_writer: String,
    pub error_agent: String,
    pub review: String,
    pub integration: String,
    #[serde(default)]
    pub embeddings: String,
    #[serde(default)]
    pub completions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateModelsRequest {
    pub models: ModelAssignmentsPayload,
}
