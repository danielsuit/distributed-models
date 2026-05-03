// We hold a synchronous `MutexGuard` across awaits intentionally. The mutex
// only serialises tests in this file (not real production logic) and the
// async-aware alternative would require restructuring every test fixture
// for no functional benefit.
#![allow(clippy::await_holding_lock)]

//! End-to-end integration test that drives the full agent pipeline:
//!
//!   client -> POST /chat -> orchestrator -> code_writer -> integration
//!          -> review -> file_proposal -> POST /proposal/:id -> assistant_message
//!
//! It boots a mocked Ollama server (a tiny axum router that returns canned
//! JSON keyed off the system prompt), the real backend with all six agents,
//! and a real Redis. When no Redis is reachable we skip with a print so the
//! suite stays green on hosts that don't have it.
//!
//! This is the "future development" safety net: when you tweak an agent's
//! system prompt or pipeline order, this test catches the obvious breakages
//! (missed routing, malformed events, accept/reject deadlocks) without
//! waiting for a real model.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

/// Serialise every test in this file. They all spin up agents that BLPOP the
/// same Redis queues, so two tests in parallel would steal each other's
/// messages.
fn shared_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Recover from poisoning so a panicking test doesn't make the rest skip.
fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    match shared_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

use axum::{extract::State, routing::post, Json, Router};
use distributed_models::agents::{self, AgentRuntime};
use distributed_models::bus::Bus;
use distributed_models::config::{Config, ModelAssignments};
use distributed_models::job_cancel::JobCancellation;
use distributed_models::messages::{ChatRequest, ChatResponse};
use distributed_models::ollama::OllamaClient;
use distributed_models::proposals::ProposalStore;
use distributed_models::server::{self, build_router, AppState};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, Mutex, RwLock};

/// Pick a canned response based on the calling agent's system prompt.
/// Keep the prompts permissive so future prompt tweaks don't immediately
/// break this test — we match on a stable keyword in each one.
fn canned_response(system: &str) -> String {
    let s = system.to_lowercase();
    if s.contains("orchestrator") && (s.contains("strict json") || s.contains("decide whether")) {
        return json!({
            "plan": "Write a Rust hello world.",
            "need_files": false,
            "file_query": "",
            "need_code": true,
            "target_file": "src/main.rs",
            "code_instruction": "Write a Rust hello world program",
            "final_answer": ""
        })
        .to_string();
    }
    if s.contains("final speaker") {
        return "All done — created src/main.rs.".to_string();
    }
    if s.contains("senior software engineer") {
        return json!({
            "operations": [
                {
                    "action": "create",
                    "file": "src/main.rs",
                    "content": "fn main() { println!(\"hello\"); }"
                }
            ],
            "summary": "Bootstrapped a hello world."
        })
        .to_string();
    }
    if s.contains("code review agent") {
        return json!({
            "approved": true,
            "reason": "Looks fine.",
            "problems": []
        })
        .to_string();
    }
    if s.contains("integration coherence agent") {
        return json!({
            "operations": [],
            "summary": "",
        })
        .to_string();
    }
    if s.contains("file-relevance ranker") {
        return String::new();
    }
    if s.contains("debugging assistant") {
        return "Fix the most impactful errors first.".to_string();
    }
    String::new()
}

#[derive(Debug, Deserialize, Serialize)]
struct GenerateBody {
    model: String,
    prompt: String,
    system: Option<String>,
    stream: bool,
    options: Option<serde_json::Value>,
}

async fn ollama_generate(Json(body): Json<GenerateBody>) -> Json<serde_json::Value> {
    let response = canned_response(body.system.as_deref().unwrap_or(""));
    Json(json!({
        "model": body.model,
        "response": response,
        "done": true,
    }))
}

async fn spawn_mock_ollama() -> String {
    let app = Router::new().route("/api/generate", post(ollama_generate));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{}", addr)
}

async fn try_bus(redis_url: &str, prefix: String) -> Option<Bus> {
    match tokio::time::timeout(
        Duration::from_secs(2),
        Bus::connect_with_prefix(redis_url, prefix),
    )
    .await
    {
        Ok(Ok(bus)) => Some(bus),
        _ => {
            eprintln!("[skip] no Redis at {redis_url}; skipping agent_flow test");
            None
        }
    }
}

/// A unique queue prefix per test invocation. Stray daemons or parallel
/// tests on the default queue names will not interfere with us.
fn unique_prefix() -> String {
    format!("dm-test-{}-", uuid::Uuid::new_v4())
}

fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_test_writer()
            .try_init();
    });
}

/// Drive a full chat from POST /chat to job_complete using a mocked Ollama.
#[tokio::test]
async fn full_pipeline_creates_file_via_proposal_accept() {
    init_tracing();
    let _guard = lock_test();
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string());
    let prefix = unique_prefix();
    let Some(bus) = try_bus(&redis_url, prefix.clone()).await else {
        return;
    };

    // Drain any leftover messages from prior runs so we start clean.
    drain_queues(&bus).await;

    let ollama_url = spawn_mock_ollama().await;
    let ollama = OllamaClient::new(ollama_url.clone());

    let config = Config {
        host: "127.0.0.1".into(),
        port: 0,
        redis_url: redis_url.clone(),
        ollama_endpoint: ollama_url,
        ollama_num_ctx: 8192,
        models: ModelAssignments {
            orchestrator: "test:orchestrator".into(),
            file_structure: "test:file_structure".into(),
            code_writer: "test:code_writer".into(),
            error_agent: "test:error".into(),
            review: "test:review".into(),
            integration: "test:integration".into(),
            embeddings: "test:embeddings".into(),
            completions: "test:completions".into(),
        },
    };

    let proposals = ProposalStore::new();
    let job_cancel = JobCancellation::default();
    let (events_tx, _) = broadcast::channel(256);

    let state = AppState {
        config: config.clone(),
        config_path: std::path::PathBuf::from("distributed-models.test.yaml"),
        models: Arc::new(RwLock::new(config.models.clone())),
        bus: bus.clone(),
        proposals: proposals.clone(),
        job_cancel: job_cancel.clone(),
        events_tx: events_tx.clone(),
        workspace_root: Arc::new(Mutex::new(None)),
        ollama: ollama.clone(),
        semantic_index: distributed_models::index::SemanticIndex::new(),
    };

    let _pump = server::spawn_event_pump(
        redis_url.clone(),
        bus.prefix().to_string(),
        events_tx.clone(),
    );
    // Give the pump a moment to actually call SUBSCRIBE on Redis pub/sub
    // before we light up agents that may publish events.
    tokio::time::sleep(Duration::from_millis(150)).await;

    agents::spawn_all(AgentRuntime {
        config: config.clone(),
        models: Arc::new(RwLock::new(config.models.clone())),
        bus: bus.clone(),
        ollama,
        proposals: proposals.clone(),
        job_cancel: job_cancel.clone(),
        semantic_index: distributed_models::index::SemanticIndex::new(),
    });

    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{}", addr);

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // Subscribe to events first so we don't miss anything between chat and
    // the orchestrator firing its first event.
    let mut events_rx = events_tx.subscribe();

    let chat: ChatResponse = http
        .post(format!("{base}/chat"))
        .json(&ChatRequest {
            text: "make hello world".into(),
            workspace_root: Some("/tmp/dm-test".into()),
            history: Vec::new(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!chat.job_id.is_empty());

    let mut saw_proposal = false;
    let mut saw_assistant = false;
    let mut saw_complete = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !saw_complete {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(15), events_rx.recv()).await
        else {
            panic!(
                "timed out before job completed (saw_proposal={saw_proposal} saw_assistant={saw_assistant})",
            );
        };
        let value = serde_json::to_value(&event).unwrap();
        if value["job_id"] != chat.job_id {
            continue;
        }
        match value["type"].as_str() {
            Some("file_proposal") => {
                saw_proposal = true;
                assert_eq!(value["operation"]["action"], "create");
                assert_eq!(value["operation"]["file"], "src/main.rs");
                let proposal_id = value["proposal_id"].as_str().unwrap().to_string();
                let response = http
                    .post(format!("{base}/proposal/{proposal_id}"))
                    .json(&json!({ "accepted": true }))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
            }
            Some("assistant_message") => {
                saw_assistant = true;
                assert!(!value["text"].as_str().unwrap_or("").is_empty());
            }
            Some("job_complete") => {
                saw_complete = true;
            }
            Some("error") => panic!("unexpected error event: {value}"),
            _ => {}
        }
    }

    assert!(saw_proposal, "expected at least one file_proposal event");
    assert!(saw_assistant, "expected an assistant_message event");
    assert!(saw_complete, "expected a job_complete event");

    // Cleanup so a re-run doesn't see leftovers.
    drain_queues(&bus).await;
}

/// Drive a chat that the planner says needs no code work, only an answer.
#[tokio::test]
async fn pipeline_short_circuits_when_planner_says_no_code() {
    let _guard = lock_test();
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string());
    let prefix = unique_prefix();
    let Some(bus) = try_bus(&redis_url, prefix.clone()).await else {
        return;
    };
    drain_queues(&bus).await;

    // Mock Ollama that always returns a "no code" plan.
    let app = Router::new()
        .route(
            "/api/generate",
            post(
                |State(_): State<()>, Json(_body): Json<GenerateBody>| async move {
                    Json(json!({
                        "model": "test",
                        "response": json!({
                            "plan": "Just answer the user.",
                            "need_files": false,
                            "file_query": "",
                            "need_code": false,
                            "target_file": "",
                            "code_instruction": "",
                            "final_answer": "Hello there!",
                        }).to_string(),
                        "done": true,
                    }))
                },
            ),
        )
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let ollama_url = format!("http://{}", addr);
    let ollama = OllamaClient::new(ollama_url.clone());

    let config = Config {
        host: "127.0.0.1".into(),
        port: 0,
        redis_url: redis_url.clone(),
        ollama_endpoint: ollama_url,
        ollama_num_ctx: 8192,
        models: ModelAssignments::from_env(),
    };

    let (events_tx, _) = broadcast::channel(256);
    let proposals = ProposalStore::new();
    let job_cancel = JobCancellation::default();
    let state = AppState {
        config: config.clone(),
        config_path: std::path::PathBuf::from("distributed-models.test.yaml"),
        models: Arc::new(RwLock::new(config.models.clone())),
        bus: bus.clone(),
        proposals: proposals.clone(),
        job_cancel: job_cancel.clone(),
        events_tx: events_tx.clone(),
        workspace_root: Arc::new(Mutex::new(None)),
        ollama: ollama.clone(),
        semantic_index: distributed_models::index::SemanticIndex::new(),
    };
    let _pump = server::spawn_event_pump(
        redis_url.clone(),
        bus.prefix().to_string(),
        events_tx.clone(),
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    agents::spawn_all(AgentRuntime {
        config: config.clone(),
        models: Arc::new(RwLock::new(config.models.clone())),
        bus: bus.clone(),
        ollama,
        proposals: proposals.clone(),
        job_cancel: job_cancel.clone(),
        semantic_index: distributed_models::index::SemanticIndex::new(),
    });

    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{}", addr);

    let http = reqwest::Client::new();
    let mut events_rx = events_tx.subscribe();

    let chat: ChatResponse = http
        .post(format!("{base}/chat"))
        .json(&ChatRequest {
            text: "hi".into(),
            workspace_root: None,
            history: Vec::new(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut saw_assistant_text: Option<String> = None;
    let mut saw_complete = false;
    let mut saw_proposal = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && !saw_complete {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(10), events_rx.recv()).await
        else {
            panic!("timed out waiting for short-circuit completion");
        };
        let value = serde_json::to_value(&event).unwrap();
        if value["job_id"] != chat.job_id {
            continue;
        }
        match value["type"].as_str() {
            Some("file_proposal") => saw_proposal = true,
            Some("assistant_message") => {
                saw_assistant_text = Some(value["text"].as_str().unwrap_or("").to_string());
            }
            Some("job_complete") => saw_complete = true,
            _ => {}
        }
    }

    assert!(saw_complete, "expected job_complete");
    assert_eq!(
        saw_assistant_text.as_deref(),
        Some("Hello there!"),
        "no-code plan should surface final_answer verbatim"
    );
    assert!(
        !saw_proposal,
        "no file proposals should be emitted when planner says need_code=false"
    );

    drain_queues(&bus).await;
}

/// POP all messages from every queue. Belt-and-braces cleanup so a partially
/// failed previous run can't poison later tests.
async fn drain_queues(bus: &Bus) {
    use distributed_models::config::queues::{
        CODE_WRITER, ERROR_AGENT, FILE_STRUCTURE, INTEGRATION, ORCHESTRATOR, REVIEW,
    };
    for queue in [
        ORCHESTRATOR,
        FILE_STRUCTURE,
        CODE_WRITER,
        INTEGRATION,
        ERROR_AGENT,
        REVIEW,
    ] {
        for _ in 0..16 {
            let res = bus.next_message(queue, 0.05).await.ok().flatten();
            if res.is_none() {
                break;
            }
        }
    }
}
