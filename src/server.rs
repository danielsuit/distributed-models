use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::bus::Bus;
use crate::config::{queues, Config, ModelAssignments};
use crate::job_cancel::JobCancellation;
use crate::messages::{
    Agent, ChatRequest, ChatResponse, ClientEvent, DiagnosticsRequest, FileChange,
    FileChangeRequest, FileSnapshotRequest, Message, ModelAssignmentsPayload,
    ProposalDecisionRequest, RuntimeConfigResponse, UpdateModelsRequest,
};
use crate::ollama::OllamaClient;
use crate::proposals::ProposalStore;

/// Body for `POST /complete` — inline ghost-text completion request.
#[derive(Debug, Deserialize)]
pub struct CompleteRequest {
    /// Code preceding the cursor.
    pub prefix: String,
    /// Code following the cursor (used for fill-in-the-middle prompts).
    #[serde(default)]
    pub suffix: String,
    /// Optional file path so the model can pick up language hints.
    #[serde(default)]
    pub file: Option<String>,
    /// Optional language identifier (rust, ts, py, …). When absent we
    /// infer from the file extension.
    #[serde(default)]
    pub language: Option<String>,
    /// Maximum number of tokens / chars to generate. Capped at 256.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Body for `POST /index/build` — kicks off (or refreshes) the semantic
/// codebase index for `workspace_root`.
#[derive(Debug, Deserialize)]
pub struct IndexBuildRequest {
    pub workspace_root: String,
}

/// Shared application state used by axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub config_path: PathBuf,
    pub models: Arc<RwLock<ModelAssignments>>,
    pub bus: Bus,
    pub proposals: ProposalStore,
    pub job_cancel: JobCancellation,
    /// Broadcast channel populated by a single Redis pub/sub task and consumed
    /// by every connected SSE client.
    pub events_tx: broadcast::Sender<ClientEvent>,
    /// Tracks the most recent workspace root the editor reported, so agents
    /// can include it in their prompts.
    pub workspace_root: Arc<Mutex<Option<String>>>,
    /// Used by the inline-completion endpoint, which calls Ollama directly
    /// rather than going through the agent pipeline.
    pub ollama: OllamaClient,
    pub semantic_index: crate::index::SemanticIndex,
}

/// Build the axum router. Exposed as a function so tests can spin it up
/// without binding a port.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/chat", post(chat))
        .route("/events", get(events))
        .route("/config", get(read_config))
        .route("/config/models", post(update_models))
        .route("/file-snapshot", post(file_snapshot))
        .route("/file-change", post(file_change))
        .route("/diagnostics", post(diagnostics))
        .route("/proposal/:id", post(proposal_decision))
        .route("/job/:id/cancel", post(cancel_job))
        .route("/complete", post(complete))
        .route("/index/build", post(index_build))
        .with_state(state)
        .layer(CorsLayer::permissive())
}

/// Bring the HTTP server online.
pub async fn run_server(state: AppState) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", state.config.host, state.config.port).parse()?;
    tracing::info!("HTTP server listening on http://{addr}");

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> &'static str {
    "Distributed Models backend is running. POST /chat or stream GET /events."
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let models = state.models.read().await.clone();
    Json(json!({
        "status": "ok",
        "redis": state.config.redis_url,
        "ollama": state.config.ollama_endpoint,
        "models": models,
        "port": state.config.port,
    }))
}

async fn build_runtime_config_snapshot(
    state: &AppState,
    assignments: ModelAssignments,
) -> RuntimeConfigResponse {
    let request = state.config.ollama_num_ctx;
    let orchestrator_model = assignments.orchestrator.trim();
    let client = OllamaClient::new(state.config.ollama_endpoint.clone());
    let native = match client.native_context_capacity(orchestrator_model).await {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(
                error = %err,
                model = orchestrator_model,
                "ollama native context look-up unavailable for /config"
            );
            None
        }
    };
    let effective = native.map(|n| n.min(request)).unwrap_or(request);
    RuntimeConfigResponse {
        host: state.config.host.clone(),
        port: state.config.port,
        redis_url: state.config.redis_url.clone(),
        ollama_endpoint: state.config.ollama_endpoint.clone(),
        models: to_payload(assignments),
        ollama_num_ctx: request,
        context_window_native: native,
        context_window_effective: effective,
    }
}

async fn read_config(State(state): State<AppState>) -> impl IntoResponse {
    let models = state.models.read().await.clone();
    Json(build_runtime_config_snapshot(&state, models).await)
}

async fn update_models(
    State(state): State<AppState>,
    Json(body): Json<UpdateModelsRequest>,
) -> Result<Json<RuntimeConfigResponse>, AppError> {
    let defaults = crate::config::ModelAssignments::defaults();
    let coalesce = |value: &str, fallback: &str| -> String {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            fallback.to_string()
        } else {
            trimmed.to_string()
        }
    };
    let new_models = ModelAssignments {
        orchestrator: body.models.orchestrator.trim().to_string(),
        file_structure: body.models.file_structure.trim().to_string(),
        code_writer: body.models.code_writer.trim().to_string(),
        error_agent: body.models.error_agent.trim().to_string(),
        review: body.models.review.trim().to_string(),
        integration: body.models.integration.trim().to_string(),
        embeddings: coalesce(&body.models.embeddings, &defaults.embeddings),
        completions: coalesce(&body.models.completions, &defaults.completions),
    };
    validate_models(&new_models)?;
    {
        let mut models = state.models.write().await;
        *models = new_models.clone();
    }

    let mut persisted = state.config.clone();
    persisted.models = new_models.clone();
    persisted.save_yaml_file(&state.config_path)?;

    state
        .bus
        .publish_event(&ClientEvent::Log {
            job_id: "config".to_string(),
            agent: Agent::Client,
            message: "Updated model assignments from UI settings.".to_string(),
        })
        .await?;

    Ok(Json(
        build_runtime_config_snapshot(&state, new_models).await,
    ))
}

async fn chat(
    State(state): State<AppState>,
    Json(body): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, AppError> {
    let job_id = Uuid::new_v4().to_string();
    let workspace_root_body = body
        .workspace_root
        .as_deref()
        .and_then(crate::workspace_path::normalize_workspace_wire);
    if let Some(ref root) = workspace_root_body {
        *state.workspace_root.lock().await = Some(root.clone());
    }
    let workspace_root = workspace_root_body.or_else(|| {
        state
            .workspace_root
            .try_lock()
            .ok()
            .and_then(|w| w.clone())
            .and_then(|s| crate::workspace_path::normalize_workspace_wire(&s))
    });

    // Slash-command interception. Direct outcomes short-circuit the
    // orchestrator entirely; Rewrite outcomes replace the user text.
    let user_text = match crate::slash::resolve(&body.text) {
        Some(crate::slash::SlashOutcome::Direct(reply)) => {
            // Surface the canned reply via the same SSE stream the rest
            // of the pipeline uses, so the client UX is uniform.
            let job_id_ev = job_id.clone();
            let bus = state.bus.clone();
            tokio::spawn(async move {
                let _ = bus
                    .publish_event(&ClientEvent::AssistantMessage {
                        job_id: job_id_ev.clone(),
                        text: reply,
                    })
                    .await;
                let _ = bus
                    .publish_event(&ClientEvent::JobComplete { job_id: job_id_ev })
                    .await;
            });
            return Ok(Json(ChatResponse { job_id }));
        }
        Some(crate::slash::SlashOutcome::Rewrite(prompt)) => prompt,
        None => body.text.clone(),
    };

    let mut message = Message::new(Agent::Client, Agent::Orchestrator, "user_message")
        .with_context(json!({
            "user_message": user_text,
            "workspace_root": workspace_root,
            "history": body.history,
        }));
    message.job_id = job_id.clone();
    state.bus.dispatch(&message).await?;
    Ok(Json(ChatResponse { job_id }))
}

async fn cancel_job(State(state): State<AppState>, Path(job_id): Path<String>) -> StatusCode {
    state.job_cancel.request_cancel(&job_id);
    StatusCode::NO_CONTENT
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// If supplied, only events for this job are forwarded.
    #[serde(default)]
    job_id: Option<String>,
}

async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |item| {
        let filter = query.job_id.clone();
        async move {
            let event = item.ok()?;
            if let Some(filter) = filter {
                if event_job_id(&event) != filter {
                    return None;
                }
            }
            let payload = serde_json::to_string(&event).ok()?;
            Some(Ok::<_, Infallible>(Event::default().data(payload)))
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn event_job_id(event: &ClientEvent) -> &str {
    match event {
        ClientEvent::AgentStatus { job_id, .. }
        | ClientEvent::Log { job_id, .. }
        | ClientEvent::AssistantMessage { job_id, .. }
        | ClientEvent::FileProposal { job_id, .. }
        | ClientEvent::CommandProposal { job_id, .. }
        | ClientEvent::CommandResult { job_id, .. }
        | ClientEvent::Error { job_id, .. }
        | ClientEvent::PromptEstimate { job_id, .. }
        | ClientEvent::JobComplete { job_id, .. } => job_id,
    }
}

async fn file_snapshot(
    State(state): State<AppState>,
    Json(body): Json<FileSnapshotRequest>,
) -> Result<StatusCode, AppError> {
    *state.workspace_root.lock().await = Some(body.workspace_root.clone());
    let message =
        Message::new(Agent::Client, Agent::FileStructure, "snapshot").with_context(json!({
            "workspace_root": body.workspace_root,
            "files": body.files,
        }));
    state.bus.dispatch(&message).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn file_change(
    State(state): State<AppState>,
    Json(body): Json<FileChangeRequest>,
) -> Result<StatusCode, AppError> {
    *state.workspace_root.lock().await = Some(body.workspace_root.clone());
    let task = match &body.change {
        FileChange::Created { .. } => "created",
        FileChange::Changed { .. } => "changed",
        FileChange::Deleted { .. } => "deleted",
    };
    let message = Message::new(Agent::Client, Agent::FileStructure, task).with_context(json!({
        "workspace_root": body.workspace_root,
        "change": body.change,
    }));
    state.bus.dispatch(&message).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn diagnostics(
    State(state): State<AppState>,
    Json(body): Json<DiagnosticsRequest>,
) -> Result<StatusCode, AppError> {
    *state.workspace_root.lock().await = Some(body.workspace_root.clone());
    let message =
        Message::new(Agent::Client, Agent::ErrorAgent, "diagnostics").with_context(json!({
            "workspace_root": body.workspace_root,
            "diagnostics": body.diagnostics,
        }));
    state.bus.dispatch(&message).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn proposal_decision(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ProposalDecisionRequest>,
) -> impl IntoResponse {
    let resolved = state.proposals.resolve(&id, body.accepted);
    if resolved {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn complete(
    State(state): State<AppState>,
    Json(body): Json<CompleteRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let model = state.models.read().await.completions.clone();
    let prompt = build_fim_prompt(&body);
    // FIM-style completion. Most coder models tuned on FIM ignore the
    // system prompt; we still send a brief steering hint for those that
    // honour it.
    let raw = state
        .ollama
        .generate(
            &model,
            Some(
                "You are a code completion engine. Return ONLY the missing code that completes the prefix. \
No prose, no markdown fences, no explanations."
            ),
            &prompt,
            state.config.ollama_num_ctx,
        )
        .await?;
    let cap = body.max_tokens.unwrap_or(128).clamp(8, 256) as usize * 4;
    let trimmed = strip_completion_suffix(&raw, cap);
    Ok(Json(json!({ "completion": trimmed })))
}

async fn index_build(
    State(state): State<AppState>,
    Json(body): Json<IndexBuildRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let model = state.models.read().await.embeddings.clone();
    let path = std::path::PathBuf::from(&body.workspace_root);
    state
        .semantic_index
        .ensure_built(&state.ollama, &model, &path)
        .await?;
    Ok(Json(json!({
        "ok": true,
        "entries": state.semantic_index.entry_count(),
    })))
}

/// Build an Ollama-style FIM (fill-in-the-middle) prompt. We use the
/// qwen-coder / deepseek-coder convention — most modern coder models
/// recognise these tokens. For models that don't, the surrounding
/// context still steers them well enough to be useful.
fn build_fim_prompt(req: &CompleteRequest) -> String {
    let lang_hint = req
        .language
        .clone()
        .or_else(|| infer_language(req.file.as_deref()))
        .unwrap_or_default();
    let header = if lang_hint.is_empty() {
        String::new()
    } else {
        format!("// language: {lang_hint}\n")
    };
    format!(
        "{header}<|fim_prefix|>{}<|fim_suffix|>{}<|fim_middle|>",
        req.prefix, req.suffix
    )
}

fn infer_language(file: Option<&str>) -> Option<String> {
    let path = file?;
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Some("rust".into()),
        "ts" | "tsx" => Some("typescript".into()),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript".into()),
        "py" => Some("python".into()),
        "go" => Some("go".into()),
        "java" => Some("java".into()),
        "kt" | "kts" => Some("kotlin".into()),
        "swift" => Some("swift".into()),
        "rb" => Some("ruby".into()),
        "php" => Some("php".into()),
        "cs" => Some("csharp".into()),
        "c" | "h" => Some("c".into()),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some("cpp".into()),
        "html" | "htm" => Some("html".into()),
        "css" => Some("css".into()),
        "scss" | "sass" => Some("scss".into()),
        "json" | "jsonc" => Some("json".into()),
        "yaml" | "yml" => Some("yaml".into()),
        "toml" => Some("toml".into()),
        "md" | "mdx" => Some("markdown".into()),
        "sh" | "bash" => Some("bash".into()),
        "sql" => Some("sql".into()),
        other => Some(other.to_string()),
    }
}

/// Local FIM models often spit out trailing junk after the actual
/// completion — fence markers, end-of-stream tokens, the suffix again,
/// or speculative next blocks. Cut at the first sign of those so the
/// editor inserts a clean snippet.
pub fn strip_completion_suffix(raw: &str, cap: usize) -> String {
    let mut text = raw.to_string();
    for marker in [
        "<|fim_pad|>",
        "<|fim_suffix|>",
        "<|endoftext|>",
        "<|im_end|>",
        "<|file_sep|>",
        "<|repo_name|>",
        "```",
    ] {
        if let Some(idx) = text.find(marker) {
            text.truncate(idx);
        }
    }
    if text.chars().count() > cap {
        let truncated: String = text.chars().take(cap).collect();
        text = truncated;
    }
    text
}

/// Spawn a long-lived task that subscribes to the Redis events channel and
/// re-broadcasts every event onto the in-process broadcast channel that the
/// SSE handler reads from. `channel_prefix` matches the Bus's prefix; it is
/// empty in production and a per-test UUID in the integration suite.
pub fn spawn_event_pump(
    redis_url: String,
    channel_prefix: String,
    events_tx: broadcast::Sender<ClientEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(err) = run_event_pump(&redis_url, &channel_prefix, &events_tx).await {
                tracing::error!("event pump crashed, restarting: {err}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_event_pump(
    redis_url: &str,
    channel_prefix: &str,
    events_tx: &broadcast::Sender<ClientEvent>,
) -> Result<()> {
    let client = redis::Client::open(redis_url)?;
    let mut pubsub = client.get_async_pubsub().await?;
    let channel = format!("{channel_prefix}{}", queues::EVENTS_CHANNEL);
    pubsub.subscribe(&channel).await?;

    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: String = msg.get_payload()?;
        match serde_json::from_str::<ClientEvent>(&payload) {
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

/// Generic error wrapper so handlers can use `?` and surface 500s with a
/// readable JSON body.
pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("server error: {:?}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{}", self.0) })),
        )
            .into_response()
    }
}

fn to_payload(models: ModelAssignments) -> ModelAssignmentsPayload {
    ModelAssignmentsPayload {
        orchestrator: models.orchestrator,
        file_structure: models.file_structure,
        code_writer: models.code_writer,
        error_agent: models.error_agent,
        review: models.review,
        integration: models.integration,
        embeddings: models.embeddings,
        completions: models.completions,
    }
}

fn validate_models(models: &ModelAssignments) -> Result<(), AppError> {
    let entries = [
        ("orchestrator", &models.orchestrator),
        ("file_structure", &models.file_structure),
        ("code_writer", &models.code_writer),
        ("error_agent", &models.error_agent),
        ("review", &models.review),
        ("integration", &models.integration),
    ];
    for (name, value) in entries {
        if value.trim().is_empty() {
            return Err(AppError(anyhow::anyhow!(
                "model assignment `{name}` cannot be empty"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fim_prompt_inserts_markers() {
        let req = CompleteRequest {
            prefix: "fn add(a: i32, b: i32) -> i32 {\n    a + ".into(),
            suffix: "\n}\n".into(),
            file: Some("src/lib.rs".into()),
            language: None,
            max_tokens: None,
        };
        let prompt = build_fim_prompt(&req);
        assert!(prompt.contains("// language: rust"));
        assert!(prompt.contains("<|fim_prefix|>"));
        assert!(prompt.contains("<|fim_suffix|>"));
        assert!(prompt.contains("<|fim_middle|>"));
        // Prefix appears before suffix and middle tokens.
        let pre_idx = prompt.find("<|fim_prefix|>").unwrap();
        let suf_idx = prompt.find("<|fim_suffix|>").unwrap();
        let mid_idx = prompt.find("<|fim_middle|>").unwrap();
        assert!(pre_idx < suf_idx && suf_idx < mid_idx);
    }

    #[test]
    fn fim_prompt_uses_explicit_language_when_provided() {
        let req = CompleteRequest {
            prefix: "x = ".into(),
            suffix: "".into(),
            file: None,
            language: Some("python".into()),
            max_tokens: None,
        };
        assert!(build_fim_prompt(&req).contains("// language: python"));
    }

    #[test]
    fn strip_truncates_at_first_marker() {
        assert_eq!(
            strip_completion_suffix("hello<|endoftext|>world", 1024),
            "hello"
        );
        assert_eq!(strip_completion_suffix("hello\n```\nworld", 1024), "hello\n");
    }

    #[test]
    fn strip_caps_long_completions() {
        let input = "x".repeat(2048);
        let out = strip_completion_suffix(&input, 100);
        assert_eq!(out.chars().count(), 100);
    }

    #[test]
    fn infer_language_recognises_common_extensions() {
        assert_eq!(infer_language(Some("foo.rs")).as_deref(), Some("rust"));
        assert_eq!(
            infer_language(Some("a/b.tsx")).as_deref(),
            Some("typescript")
        );
        assert_eq!(infer_language(Some("x.py")).as_deref(), Some("python"));
        assert_eq!(infer_language(None), None);
    }
}
