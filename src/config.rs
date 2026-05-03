use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Names of all queues used by the platform. Match the spec's
/// `agent:<name>` convention.
pub mod queues {
    pub const ORCHESTRATOR: &str = "agent:orchestrator";
    pub const FILE_STRUCTURE: &str = "agent:filestructure";
    pub const CODE_WRITER: &str = "agent:codewriter";
    pub const ERROR_AGENT: &str = "agent:error";
    pub const REVIEW: &str = "agent:review";
    pub const INTEGRATION: &str = "agent:integration";

    /// Pub/Sub channel used to stream events back to clients (editor / CLI).
    pub const EVENTS_CHANNEL: &str = "events:client";
}

/// Per-agent model assignments. Each agent reads its model name from this
/// struct, which in turn is populated from environment variables. Defaults
/// follow the spec recommendations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAssignments {
    pub orchestrator: String,
    pub file_structure: String,
    pub code_writer: String,
    pub error_agent: String,
    pub review: String,
    pub integration: String,
    /// Embedding model used by the semantic codebase index. Defaults to
    /// `nomic-embed-text:latest`. Pull it once with
    /// `ollama pull nomic-embed-text`.
    #[serde(default = "default_embeddings_model")]
    pub embeddings: String,
    /// Code-completion (FIM) model for ghost-text inline completions in
    /// the editor overlay. Should be a coder model that supports FIM.
    #[serde(default = "default_completions_model")]
    pub completions: String,
}

fn default_embeddings_model() -> String {
    "nomic-embed-text:latest".to_string()
}

fn default_completions_model() -> String {
    "qwen2.5-coder:7b".to_string()
}

impl ModelAssignments {
    pub fn defaults() -> Self {
        Self {
            orchestrator: "qwen2.5-coder:7b".to_string(),
            file_structure: "llama3.2:3b".to_string(),
            code_writer: "qwen2.5-coder:14b".to_string(),
            error_agent: "qwen2.5-coder:7b".to_string(),
            review: "qwen2.5-coder:7b".to_string(),
            integration: "qwen2.5-coder:7b".to_string(),
            embeddings: default_embeddings_model(),
            completions: default_completions_model(),
        }
    }

    pub fn from_env() -> Self {
        let defaults = Self::defaults();
        Self {
            orchestrator: env::var("DM_MODEL_ORCHESTRATOR").unwrap_or(defaults.orchestrator),
            file_structure: env::var("DM_MODEL_FILE_STRUCTURE").unwrap_or(defaults.file_structure),
            code_writer: env::var("DM_MODEL_CODE_WRITER").unwrap_or(defaults.code_writer),
            error_agent: env::var("DM_MODEL_ERROR").unwrap_or(defaults.error_agent),
            review: env::var("DM_MODEL_REVIEW").unwrap_or(defaults.review),
            integration: env::var("DM_MODEL_INTEGRATION").unwrap_or(defaults.integration),
            embeddings: env::var("DM_MODEL_EMBEDDINGS").unwrap_or(defaults.embeddings),
            completions: env::var("DM_MODEL_COMPLETIONS").unwrap_or(defaults.completions),
        }
    }
}

/// Full backend configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub redis_url: String,
    pub ollama_endpoint: String,
    /// Passed to every Ollama `generate` request as `options.num_ctx`.
    pub ollama_num_ctx: u32,
    pub models: ModelAssignments,
}

fn default_ollama_num_ctx() -> u32 {
    8192
}

impl Config {
    /// Primary loader used by the app. Preference order:
    /// 1) YAML config file (`distributed-models.yaml` or `$DM_CONFIG`)
    /// 2) environment vars / `.env` fallback
    pub fn load() -> Self {
        let _ = dotenvy::dotenv();
        let path = Self::resolve_config_path();
        if path.exists() {
            match Self::from_yaml_file(&path) {
                Ok(config) => return config,
                Err(err) => tracing::warn!(
                    "failed reading config file {} ({err}); falling back to env",
                    path.display()
                ),
            }
        }
        Self::from_env()
    }

    pub fn resolve_config_path() -> PathBuf {
        env::var("DM_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("distributed-models.yaml"))
    }

    pub fn from_yaml_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let yaml: YamlConfigFile = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing yaml {}", path.display()))?;
        Ok(yaml.into_config())
    }

    pub fn save_yaml_file(&self, path: &Path) -> Result<()> {
        let yaml = YamlConfigFile::from_config(self);
        let body = serde_yaml::to_string(&yaml).context("encoding config yaml")?;
        std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn from_env() -> Self {
        Self {
            host: env::var("DM_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: env::var("DM_PORT")
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(3000),
            redis_url: env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string()),
            ollama_endpoint: env::var("OLLAMA_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            ollama_num_ctx: env::var("DM_OLLAMA_NUM_CTX")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(default_ollama_num_ctx),
            models: ModelAssignments::from_env(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct YamlModels {
    orchestrator: String,
    file_structure: String,
    code_writer: String,
    error_agent: String,
    review: String,
    #[serde(default)]
    integration: String,
    #[serde(default)]
    embeddings: String,
    #[serde(default)]
    completions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct YamlConfigFile {
    host: String,
    port: u16,
    redis_url: String,
    ollama_endpoint: String,
    #[serde(default = "default_ollama_num_ctx")]
    ollama_num_ctx: u32,
    models: YamlModels,
    /// Ignored — kept only so YAML files written when MCP existed still deserialize.
    #[serde(default)]
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    mcp_servers: Vec<serde_yaml::Value>,
}

impl YamlConfigFile {
    fn into_config(self) -> Config {
        let defaults = ModelAssignments::defaults();
        let or_default = |value: String, fallback: String| -> String {
            if value.trim().is_empty() {
                fallback
            } else {
                value
            }
        };
        Config {
            host: self.host,
            port: self.port,
            redis_url: self.redis_url,
            ollama_endpoint: self.ollama_endpoint,
            ollama_num_ctx: self.ollama_num_ctx,
            models: ModelAssignments {
                orchestrator: self.models.orchestrator,
                file_structure: self.models.file_structure,
                code_writer: self.models.code_writer,
                error_agent: self.models.error_agent,
                review: self.models.review,
                integration: or_default(self.models.integration, defaults.integration),
                embeddings: or_default(self.models.embeddings, defaults.embeddings),
                completions: or_default(self.models.completions, defaults.completions),
            },
        }
    }

    fn from_config(config: &Config) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            redis_url: config.redis_url.clone(),
            ollama_endpoint: config.ollama_endpoint.clone(),
            ollama_num_ctx: config.ollama_num_ctx,
            models: YamlModels {
                orchestrator: config.models.orchestrator.clone(),
                file_structure: config.models.file_structure.clone(),
                code_writer: config.models.code_writer.clone(),
                error_agent: config.models.error_agent.clone(),
                review: config.models.review.clone(),
                integration: config.models.integration.clone(),
                embeddings: config.models.embeddings.clone(),
                completions: config.models.completions.clone(),
            },
            mcp_servers: Vec::new(),
        }
    }
}
