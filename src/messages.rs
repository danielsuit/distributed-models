use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A logical agent on the bus. The string representations match the
/// `agent:<name>` queue suffixes used by the README.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Orchestrator,
    FileStructure,
    CodeWriter,
    ErrorAgent,
    Review,
    Extension,
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
            Agent::Extension => queues::EVENTS_CHANNEL,
        }
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Agent::Orchestrator => "Orchestrator",
            Agent::FileStructure => "File Structure",
            Agent::CodeWriter => "Code Writer",
            Agent::ErrorAgent => "Error Agent",
            Agent::Review => "Review",
            Agent::Extension => "Extension",
        }
    }
}

/// Wire format used between agents. Mirrors the JSON contract in the README:
/// `{ id, from, to, task, context, result }` with a few extra fields used for
/// correlation and observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    /// All sub-messages spawned for the same user request share a job id so
    /// the orchestrator can correlate results back to the originating chat.
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

    #[allow(dead_code)]
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

/// Events streamed back to the VS Code extension over the websocket. These
/// are also published on `events:extension` so the orchestrator can fan-out
/// updates from any agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionEvent {
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
    AssistantMessage {
        job_id: String,
        text: String,
    },
    /// A file write proposal that needs Accept/Reject in the sidebar.
    FileProposal {
        job_id: String,
        proposal_id: String,
        file: String,
        content: String,
        review_notes: Option<String>,
    },
    /// Something went wrong; surfaced as an error bubble in the sidebar.
    Error {
        job_id: String,
        message: String,
    },
    /// The orchestrator finished the job and the chat can accept new input.
    JobComplete {
        job_id: String,
    },
}

/// Messages the extension sends to the backend over the websocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionRequest {
    /// User typed a chat message.
    UserMessage {
        text: String,
        #[serde(default)]
        workspace_root: Option<String>,
    },
    /// File watcher snapshot of the workspace.
    FileSnapshot {
        workspace_root: String,
        files: Vec<FileEntry>,
    },
    /// Incremental file change from the watcher.
    FileChange {
        workspace_root: String,
        change: FileChange,
    },
    /// Diagnostic update from VS Code (errors and warnings).
    Diagnostics {
        workspace_root: String,
        diagnostics: Vec<DiagnosticEntry>,
    },
    /// User decided what to do with a file proposal.
    ProposalDecision {
        proposal_id: String,
        accepted: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChange {
    Created { path: String },
    Changed { path: String },
    Deleted { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEntry {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}
