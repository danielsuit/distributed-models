use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{broadcast, Mutex};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::bus::Bus;
use crate::config::{queues, Config};
use crate::messages::{
    Agent, ExtensionEvent, ExtensionRequest, FileChange, Message,
};
use crate::proposals::ProposalStore;

/// Shared application state used by axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub bus: Bus,
    pub proposals: ProposalStore,
    /// Broadcast channel populated by a single Redis pub/sub task and consumed
    /// by every connected websocket client. A channel buffer of 256 is plenty
    /// for the rate of events the agents produce.
    pub events_tx: broadcast::Sender<ExtensionEvent>,
    /// Tracks the most recent workspace root the extension reported, so agents
    /// can include it in their prompts.
    pub workspace_root: Arc<Mutex<Option<String>>>,
}

/// Bring the HTTP + WS server online.
pub async fn run_server(state: AppState) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", state.config.host, state.config.port).parse()?;
    tracing::info!("HTTP/WS server listening on {addr}");

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/ws", get(ws_handler))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> &'static str {
    "Distributed Models backend is running. Connect via /ws."
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(json!({
        "status": "ok",
        "redis": state.config.redis_url,
        "ollama": state.config.ollama_endpoint,
        "models": state.config.models,
    }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut events_rx = state.events_tx.subscribe();

    // Forward broadcast events to this websocket client.
    let send_state = state.clone();
    let send_task = tokio::spawn(async move {
        // Welcome message so the sidebar knows the backend is live.
        let _ = send_state; // currently unused, reserved for future hooks
        loop {
            match events_rx.recv().await {
                Ok(event) => {
                    let payload = match serde_json::to_string(&event) {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!("encoding extension event failed: {err}");
                            continue;
                        }
                    };
                    if sender.send(WsMessage::Text(payload)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("websocket client lagged, dropped {skipped} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Pump incoming websocket messages into the orchestrator/file-structure
    // queues.
    let recv_state = state.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            let WsMessage::Text(text) = msg else {
                continue;
            };
            let request: ExtensionRequest = match serde_json::from_str(&text) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!("malformed extension request {err}: {text}");
                    continue;
                }
            };

            if let Err(err) = handle_extension_request(&recv_state, request).await {
                tracing::warn!("handling extension request failed: {err}");
            }
        }
    });

    let _ = tokio::join!(send_task, recv_task);
}

async fn handle_extension_request(
    state: &AppState,
    request: ExtensionRequest,
) -> Result<()> {
    match request {
        ExtensionRequest::UserMessage { text, workspace_root } => {
            if let Some(root) = workspace_root.clone() {
                *state.workspace_root.lock().await = Some(root);
            }
            let job_id = Uuid::new_v4().to_string();
            let mut message = Message::new(Agent::Extension, Agent::Orchestrator, "user_message")
                .with_context(json!({
                    "user_message": text,
                    "workspace_root": workspace_root.or_else(|| {
                        state.workspace_root.try_lock().ok().and_then(|w| w.clone())
                    }),
                }));
            message.job_id = job_id;
            state.bus.dispatch(&message).await?;
        }
        ExtensionRequest::FileSnapshot { workspace_root, files } => {
            *state.workspace_root.lock().await = Some(workspace_root.clone());
            let message = Message::new(Agent::Extension, Agent::FileStructure, "snapshot")
                .with_context(json!({
                    "workspace_root": workspace_root,
                    "files": files,
                }));
            state.bus.dispatch(&message).await?;
        }
        ExtensionRequest::FileChange { workspace_root, change } => {
            *state.workspace_root.lock().await = Some(workspace_root.clone());
            let task = match &change {
                FileChange::Created { .. } => "created",
                FileChange::Changed { .. } => "changed",
                FileChange::Deleted { .. } => "deleted",
            };
            let message = Message::new(Agent::Extension, Agent::FileStructure, task)
                .with_context(json!({
                    "workspace_root": workspace_root,
                    "change": change,
                }));
            state.bus.dispatch(&message).await?;
        }
        ExtensionRequest::Diagnostics { workspace_root, diagnostics } => {
            *state.workspace_root.lock().await = Some(workspace_root.clone());
            let message = Message::new(Agent::Extension, Agent::ErrorAgent, "diagnostics")
                .with_context(json!({
                    "workspace_root": workspace_root,
                    "diagnostics": diagnostics,
                }));
            state.bus.dispatch(&message).await?;
        }
        ExtensionRequest::ProposalDecision { proposal_id, accepted } => {
            let resolved = state.proposals.resolve(&proposal_id, accepted);
            if !resolved {
                tracing::warn!("decision for unknown proposal {proposal_id}");
            }
        }
    }
    Ok(())
}

/// Spawn a long-lived task that subscribes to the Redis events channel and
/// re-broadcasts every event onto the in-process broadcast channel that the
/// websocket sessions read from.
pub fn spawn_event_pump(
    redis_url: String,
    events_tx: broadcast::Sender<ExtensionEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(err) = run_event_pump(&redis_url, &events_tx).await {
                tracing::error!("event pump crashed, restarting: {err}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_event_pump(
    redis_url: &str,
    events_tx: &broadcast::Sender<ExtensionEvent>,
) -> Result<()> {
    let client = redis::Client::open(redis_url)?;
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(queues::EVENTS_CHANNEL).await?;

    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: String = msg.get_payload()?;
        match serde_json::from_str::<ExtensionEvent>(&payload) {
            Ok(event) => {
                let _ = events_tx.send(event);
            }
            Err(err) => {
                tracing::warn!("decoding event {err}: {payload}");
            }
        }
    }
    Ok(())
}
