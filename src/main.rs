use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::{broadcast, Mutex};
use tracing_subscriber::{prelude::*, EnvFilter};

mod agents;
mod bus;
mod cli;
mod config;
mod messages;
mod ollama;
mod proposals;
mod server;

use crate::bus::Bus;
use crate::config::Config;
use crate::ollama::OllamaClient;
use crate::proposals::ProposalStore;
use crate::server::AppState;

#[derive(Parser, Debug)]
#[command(
    name = "distributed-models",
    version,
    about = "Local multi-agent AI coding assistant powered by Ollama and Redis"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the backend daemon (default).
    Serve,
    /// Send a single chat message to a running backend and stream the response.
    Chat {
        /// The user message to send.
        message: String,
        /// Optional workspace root to attach to the request.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Automatically accept every file proposal without prompting.
        #[arg(long)]
        auto_accept: bool,
        /// Stop streaming after this many seconds of silence.
        #[arg(long, default_value_t = 600)]
        idle_timeout: u64,
    },
    /// Hit the backend's `/health` endpoint and print the JSON response.
    Health,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let config = Config::from_env();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::Chat {
            message,
            workspace,
            auto_accept,
            idle_timeout,
        } => {
            cli::run_chat(
                &config,
                &message,
                workspace.map(|p| p.to_string_lossy().into_owned()),
                auto_accept,
                Duration::from_secs(idle_timeout),
            )
            .await
        }
        Command::Health => cli::run_health(&config).await,
    }
}

async fn serve(config: Config) -> Result<()> {
    tracing::info!(
        host = %config.host,
        port = config.port,
        redis = %config.redis_url,
        ollama = %config.ollama_endpoint,
        "starting distributed-models backend"
    );

    let bus = Bus::connect(&config.redis_url).await?;
    let ollama = OllamaClient::new(config.ollama_endpoint.clone());
    let proposals = ProposalStore::new();
    let (events_tx, _events_rx) = broadcast::channel(256);

    let state = AppState {
        config: config.clone(),
        bus: bus.clone(),
        proposals: proposals.clone(),
        events_tx: events_tx.clone(),
        workspace_root: Arc::new(Mutex::new(None)),
    };

    let _pump = server::spawn_event_pump(config.redis_url.clone(), events_tx.clone());

    agents::spawn_all(agents::AgentRuntime {
        config: config.clone(),
        bus: bus.clone(),
        ollama: ollama.clone(),
        proposals: proposals.clone(),
    });

    server::run_server(state).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,distributed_models=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();
}
