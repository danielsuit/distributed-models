//! Agent runtime. Each agent is a long-running tokio task that consumes its
//! Redis inbox queue and produces messages or extension events.

use crate::bus::Bus;
use crate::config::{Config, ModelAssignments};
use crate::index::SemanticIndex;
use crate::job_cancel::JobCancellation;
use crate::messages::{Agent, ClientEvent};
use crate::ollama::OllamaClient;
use crate::proposals::ProposalStore;
use std::sync::Arc;
use tokio::sync::RwLock;

pub mod code_writer;
pub mod error_agent;
pub mod file_structure;
pub mod integration;
pub mod orchestrator;
pub mod review;

/// Shared inputs every agent needs to start running.
#[derive(Clone)]
pub struct AgentRuntime {
    pub config: Config,
    pub models: Arc<RwLock<ModelAssignments>>,
    pub bus: Bus,
    pub ollama: OllamaClient,
    pub proposals: ProposalStore,
    pub job_cancel: JobCancellation,
    pub semantic_index: SemanticIndex,
}

impl AgentRuntime {
    pub async fn model_for(&self, agent: Agent) -> String {
        let models = self.models.read().await;
        match agent {
            Agent::Orchestrator => models.orchestrator.clone(),
            Agent::FileStructure => models.file_structure.clone(),
            Agent::CodeWriter => models.code_writer.clone(),
            Agent::ErrorAgent => models.error_agent.clone(),
            Agent::Review => models.review.clone(),
            Agent::Integration => models.integration.clone(),
            Agent::Client => models.orchestrator.clone(),
        }
    }

    /// Best-effort SSE for sidebar context metering; ignores publish failures.
    pub async fn emit_prompt_estimate(&self, job_id: &str, agent: Agent, system: Option<&str>, prompt: &str) {
        let approximate_tokens = crate::messages::approximate_llm_turn_tokens_utf8(system, prompt);
        if let Err(err) = self
            .bus
            .publish_event(&ClientEvent::PromptEstimate {
                job_id: job_id.to_string(),
                agent,
                approximate_tokens,
            })
            .await
        {
            tracing::debug!("emit_prompt_estimate skipped: {err}");
        }
    }
}

/// Spawn every agent task. Failures inside an agent are logged but never kill
/// the process; the agent loop reconnects on its own iteration.
pub fn spawn_all(rt: AgentRuntime) {
    tokio::spawn(orchestrator::run(rt.clone()));
    tokio::spawn(file_structure::run(rt.clone()));
    tokio::spawn(code_writer::run(rt.clone()));
    tokio::spawn(integration::run(rt.clone()));
    tokio::spawn(error_agent::run(rt.clone()));
    tokio::spawn(review::run(rt));
}
