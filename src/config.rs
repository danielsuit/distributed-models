use std::env;

use serde::{Deserialize, Serialize};

/// Names of all queues used by the platform. The names match the README's
/// `agent:<name>` convention.
pub mod queues {
    pub const ORCHESTRATOR: &str = "agent:orchestrator";
    pub const FILE_STRUCTURE: &str = "agent:filestructure";
    pub const CODE_WRITER: &str = "agent:codewriter";
    pub const ERROR_AGENT: &str = "agent:error";
    pub const REVIEW: &str = "agent:review";

    /// Pub/Sub channel used to stream events back to the VS Code extension.
    pub const EVENTS_CHANNEL: &str = "events:extension";
}

/// Per-agent model assignments. Each agent reads its model name from this
/// struct, which in turn is populated from environment variables (see
/// `Config::from_env`). Defaults follow the README recommendations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAssignments {
    pub orchestrator: String,
    pub file_structure: String,
    pub code_writer: String,
    pub error_agent: String,
    pub review: String,
}

impl ModelAssignments {
    pub fn from_env() -> Self {
        Self {
            orchestrator: env::var("DM_MODEL_ORCHESTRATOR")
                .unwrap_or_else(|_| "qwen2.5-coder:7b".to_string()),
            file_structure: env::var("DM_MODEL_FILE_STRUCTURE")
                .unwrap_or_else(|_| "llama3.2:3b".to_string()),
            code_writer: env::var("DM_MODEL_CODE_WRITER")
                .unwrap_or_else(|_| "qwen2.5-coder:14b".to_string()),
            error_agent: env::var("DM_MODEL_ERROR")
                .unwrap_or_else(|_| "qwen2.5-coder:7b".to_string()),
            review: env::var("DM_MODEL_REVIEW")
                .unwrap_or_else(|_| "qwen2.5-coder:7b".to_string()),
        }
    }
}

/// Full backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub redis_url: String,
    pub ollama_endpoint: String,
    pub models: ModelAssignments,
}

impl Config {
    pub fn from_env() -> Self {
        // dotenvy is a no-op if no .env exists; ignore errors.
        let _ = dotenvy::dotenv();

        Self {
            host: env::var("DM_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: env::var("DM_PORT")
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(7878),
            redis_url: env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string()),
            ollama_endpoint: env::var("OLLAMA_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            models: ModelAssignments::from_env(),
        }
    }
}
