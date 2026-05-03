//! Spin up an axum-based mock for Ollama's `/api/generate` endpoint and make
//! sure the `OllamaClient` posts the right body and decodes the response.

use std::sync::Arc;

use axum::{extract::State, routing::post, Json, Router};
use distributed_models::ollama::OllamaClient;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CapturedRequest {
    model: String,
    prompt: String,
    system: Option<String>,
    stream: bool,
    options: Option<serde_json::Value>,
}

#[derive(Clone, Default)]
struct MockState {
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    response: Arc<Mutex<String>>,
}

async fn generate(
    State(state): State<MockState>,
    Json(body): Json<CapturedRequest>,
) -> Json<serde_json::Value> {
    let response = state.response.lock().await.clone();
    state.captured.lock().await.push(body);
    Json(json!({
        "model": "test",
        "response": response,
        "done": true,
    }))
}

async fn spawn_mock() -> (MockState, String, tokio::task::JoinHandle<()>) {
    let state = MockState::default();
    *state.response.lock().await = "hello from mock".to_string();
    let app = Router::new()
        .route("/api/generate", post(generate))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, format!("http://{}", addr), handle)
}

#[tokio::test]
async fn ollama_client_posts_expected_body_and_decodes_response() {
    let (state, base_url, _server) = spawn_mock().await;
    let client = OllamaClient::new(base_url);

    let response = client
        .generate(
            "qwen2.5-coder:7b",
            Some("you are a helper"),
            "say hello",
            8192,
        )
        .await
        .unwrap();
    assert_eq!(response, "hello from mock");

    let captured = state.captured.lock().await;
    assert_eq!(captured.len(), 1);
    let req = &captured[0];
    assert_eq!(req.model, "qwen2.5-coder:7b");
    assert_eq!(req.prompt, "say hello");
    assert_eq!(req.system.as_deref(), Some("you are a helper"));
    assert!(!req.stream, "stream must be false so we get a single JSON");
    assert!(req.options.is_some());
}

#[tokio::test]
async fn ollama_client_returns_error_for_non_2xx() {
    let app = Router::new().route(
        "/api/generate",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "boom".to_string(),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OllamaClient::new(format!("http://{}", addr));

    let err = client
        .generate("m", None, "x", 8192)
        .await
        .expect_err("server returned 500, client must surface an error");
    assert!(format!("{err:?}").contains("500"));
}

#[tokio::test]
async fn ollama_client_trims_trailing_slash_in_base_url() {
    let (state, base_url, _server) = spawn_mock().await;
    let with_slash = format!("{base_url}/");
    let client = OllamaClient::new(with_slash);
    let response = client.generate("m", None, "p", 8192).await.unwrap();
    assert_eq!(response, "hello from mock");
    assert_eq!(state.captured.lock().await.len(), 1);
}
