// src/orchestrator.rs
use axum::{
    routing::post, Router, Json,
};
use config::{Config, Config as OrchestratorConfig};
use reqwest::Client;
use redis::{Commands, ConnectionPool, RedisError};
use serde_json::json;
use std::sync::Arc;

pub async fn run_orchestrator() {
    let config = Config::load();
    let client = Client::new();
    let pool: ConnectionPool = redis::Client::open(config.redis_url).unwrap().into();

    let app = Router::new()
        .route("/task", post(handle_task))
        .with_state(Arc::new((client, pool)));

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn handle_task(
    Json(payload): Json<serde_json::Value>,
    State(state): State<Arc<(Client, ConnectionPool)>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load();
    let client = state.0.clone();
    let pool = state.1.clone();

    let input = payload.get("input").unwrap().as_str().unwrap();
    let subtasks: Vec<String> = // split the task into subtasks
        vec![input.to_string()]; // example

    for subtask in subtasks {
        let redis_client = redis::Client::open(config.redis_url).unwrap();
        let mut conn = redis_client.get_async_connection().await?;
        conn.lpush("subtasks", subtask).await?;
    }

    Ok(())
}