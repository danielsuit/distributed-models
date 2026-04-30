//! Agent runtime. Each agent is a long-running tokio task that consumes its
//! Redis inbox queue and produces messages or extension events.

use crate::bus::Bus;
use crate::config::Config;
use crate::ollama::OllamaClient;
use crate::proposals::ProposalStore;

pub mod code_writer;
pub mod error_agent;
pub mod file_structure;
pub mod orchestrator;
pub mod review;

/// Shared inputs every agent needs to start running.
#[derive(Clone)]
pub struct AgentRuntime {
    pub config: Config,
    pub bus: Bus,
    pub ollama: OllamaClient,
    pub proposals: ProposalStore,
}

/// Spawn every agent task. Failures inside an agent are logged but never kill
/// the process; the agent loop reconnects on its own iteration.
pub fn spawn_all(rt: AgentRuntime) {
    tokio::spawn(orchestrator::run(rt.clone()));
    tokio::spawn(file_structure::run(rt.clone()));
    tokio::spawn(code_writer::run(rt.clone()));
    tokio::spawn(error_agent::run(rt.clone()));
    tokio::spawn(review::run(rt));
}
