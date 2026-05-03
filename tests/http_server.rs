//! End-to-end style tests for the REST surface. We boot the real router in
//! a background task and hit it over HTTP. The bus underneath is a real
//! Redis connection, so when no Redis is reachable we skip with a print
//! rather than failing — keeps the suite green on CI runners that don't
//! have Redis available.

use std::sync::Arc;
use std::time::Duration;

use distributed_models::bus::Bus;
use distributed_models::config::{Config, ModelAssignments};
use distributed_models::job_cancel::JobCancellation;
use distributed_models::messages::{
    ChatRequest, ChatResponse, FileChange, FileChangeRequest, FileEntry, FileSnapshotRequest,
    ProposalDecisionRequest, UpdateModelsRequest,
};
use distributed_models::proposals::ProposalStore;
use distributed_models::server::{build_router, AppState};
use reqwest::Client;
use std::path::PathBuf;
use tokio::sync::{broadcast, Mutex, RwLock};
use uuid::Uuid;

fn http_test_config_path() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR").map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join("distributed-models-integration-tests")
    });
    std::fs::create_dir_all(&base).unwrap_or_else(|err| {
        panic!("mkdir {}: {err}", base.display())
    });
    base.join(format!("distributed-models.test-{}.yaml", Uuid::new_v4()))
}

fn redis_url_for_test() -> String {
    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string())
}

async fn try_bus() -> Option<Bus> {
    let url = redis_url_for_test();
    match tokio::time::timeout(Duration::from_secs(2), Bus::connect(&url)).await {
        Ok(Ok(bus)) => Some(bus),
        _ => {
            eprintln!("[skip] no Redis at {url}; skipping integration test");
            None
        }
    }
}

async fn spawn_server() -> Option<(Client, String, AppState)> {
    let bus = try_bus().await?;
    let config = Config {
        host: "127.0.0.1".into(),
        port: 0,
        redis_url: redis_url_for_test(),
        ollama_endpoint: "http://127.0.0.1:11434".into(),
        ollama_num_ctx: 8192,
        models: ModelAssignments::from_env(),
    };
    let (events_tx, _events_rx) = broadcast::channel(64);
    let job_cancel = JobCancellation::default();
    let state = AppState {
        config: config.clone(),
        config_path: http_test_config_path(),
        models: Arc::new(RwLock::new(config.models.clone())),
        bus,
        proposals: ProposalStore::new(),
        job_cancel,
        events_tx,
        workspace_root: Arc::new(Mutex::new(None)),
        ollama: distributed_models::ollama::OllamaClient::new(
            config.ollama_endpoint.clone(),
        ),
        semantic_index: distributed_models::index::SemanticIndex::new(),
    };
    let router = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let http = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    Some((http, format!("http://{}", addr), state))
}

#[tokio::test]
async fn root_endpoint_responds() {
    let Some((http, base, _)) = spawn_server().await else {
        return;
    };
    let body = http
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Distributed Models"));
}

#[tokio::test]
async fn health_endpoint_returns_config() {
    let Some((http, base, state)) = spawn_server().await else {
        return;
    };
    let body: serde_json::Value = http
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["redis"], state.config.redis_url);
    assert_eq!(body["ollama"], state.config.ollama_endpoint);
    assert!(body["models"]["orchestrator"].is_string());
}

#[tokio::test]
async fn chat_endpoint_returns_job_id_and_dispatches_message() {
    let Some((http, base, _state)) = spawn_server().await else {
        return;
    };
    let response: ChatResponse = http
        .post(format!("{base}/chat"))
        .json(&ChatRequest {
            text: "ping".into(),
            workspace_root: Some("/tmp/ws".into()),
            history: Vec::new(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!response.job_id.is_empty());
}

#[tokio::test]
async fn cancel_job_endpoint_returns_no_content() {
    let Some((http, base, _)) = spawn_server().await else {
        return;
    };
    let job_id = "test-job-123";
    let response = http
        .post(format!("{base}/job/{job_id}/cancel"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn file_snapshot_endpoint_accepts_payload() {
    let Some((http, base, _)) = spawn_server().await else {
        return;
    };
    let response = http
        .post(format!("{base}/file-snapshot"))
        .json(&FileSnapshotRequest {
            workspace_root: "/tmp/ws".into(),
            files: vec![FileEntry {
                path: "a.rs".into(),
                size: 0,
                is_dir: false,
                symbols: None,
            }],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
}

#[tokio::test]
async fn file_change_endpoint_accepts_payload() {
    let Some((http, base, _)) = spawn_server().await else {
        return;
    };
    let response = http
        .post(format!("{base}/file-change"))
        .json(&FileChangeRequest {
            workspace_root: "/tmp/ws".into(),
            change: FileChange::Created {
                path: "src/new.rs".into(),
            },
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
}

#[tokio::test]
async fn proposal_decision_unknown_returns_404() {
    let Some((http, base, _)) = spawn_server().await else {
        return;
    };
    let response = http
        .post(format!("{base}/proposal/does-not-exist"))
        .json(&ProposalDecisionRequest { accepted: true })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn proposal_decision_resolves_pending_proposal() {
    let Some((http, base, state)) = spawn_server().await else {
        return;
    };
    let proposal_id = "test-proposal".to_string();
    let waiter = state.proposals.register(proposal_id.clone());

    let response = http
        .post(format!("{base}/proposal/{proposal_id}"))
        .json(&ProposalDecisionRequest { accepted: true })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let resolved = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("decision must resolve quickly")
        .expect("oneshot must succeed");
    assert!(resolved);
}

#[tokio::test]
async fn events_endpoint_streams_published_events() {
    let Some((http, base, state)) = spawn_server().await else {
        return;
    };

    // Open the SSE stream in a task and push a synthetic event in.
    let url = format!("{base}/events");
    let client = http.clone();
    let collector = tokio::spawn(async move {
        let response = client.get(&url).send().await.unwrap();
        let mut stream = response.bytes_stream();
        let mut buf = String::new();
        // Read up to ~1 second waiting for our event to land.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let next = tokio::time::timeout(
                Duration::from_millis(200),
                futures_util::StreamExt::next(&mut stream),
            )
            .await;
            if let Ok(Some(Ok(chunk))) = next {
                if let Ok(text) = std::str::from_utf8(&chunk) {
                    buf.push_str(text);
                    if buf.contains("agent_status") {
                        break;
                    }
                }
            }
        }
        buf
    });

    // Give the SSE handler a moment to subscribe, then publish an event into
    // the in-process broadcast channel directly. We deliberately avoid Redis
    // here so the test is independent of Redis pub/sub timing.
    tokio::time::sleep(Duration::from_millis(150)).await;
    state
        .events_tx
        .send(distributed_models::messages::ClientEvent::AgentStatus {
            job_id: "j1".into(),
            agent: distributed_models::messages::Agent::Orchestrator,
            status: "planning".into(),
        })
        .unwrap();

    let buf = collector.await.unwrap();
    assert!(
        buf.contains("agent_status"),
        "expected agent_status to land in SSE stream, got: {buf}"
    );
    assert!(buf.contains("planning"));
}

#[tokio::test]
async fn config_endpoint_returns_models() {
    let Some((http, base, _)) = spawn_server().await else {
        return;
    };
    let body: serde_json::Value = http
        .get(format!("{base}/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["models"]["orchestrator"].is_string());
    assert!(body["models"]["code_writer"].is_string());
    assert_eq!(body["ollama_num_ctx"], 8192);
    assert!(body["context_window_effective"].is_number());
    assert!(
        body["context_window_native"].is_null() || body["context_window_native"].is_number(),
        "expected native context absent or numeric, got {:?}",
        body["context_window_native"],
    );
}

#[tokio::test]
async fn config_models_endpoint_updates_runtime_models() {
    let Some((http, base, state)) = spawn_server().await else {
        return;
    };
    let response = http
        .post(format!("{base}/config/models"))
        .json(&UpdateModelsRequest {
            models: distributed_models::messages::ModelAssignmentsPayload {
                orchestrator: "qwen2.5-coder:7b".into(),
                file_structure: "llama3.2:3b".into(),
                code_writer: "deepseek-coder-v2:16b".into(),
                error_agent: "qwen2.5-coder:7b".into(),
                review: "qwen2.5-coder:7b".into(),
                integration: "qwen2.5-coder:7b".into(),
                embeddings: "nomic-embed-text:latest".into(),
                completions: "qwen2.5-coder:7b".into(),
            },
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let current = state.models.read().await.clone();
    assert_eq!(current.code_writer, "deepseek-coder-v2:16b");
}
