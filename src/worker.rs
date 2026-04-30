// src/worker.rs
use config::{Config, Config as WorkerConfig};
use reqwest::Client;
use redis::{Commands, ConnectionPool, RedisError};
use serde_json::json;

pub async fn run_worker() {
    let config = Config::load();
    let client = Client::new();
    let pool: ConnectionPool = redis::Client::open(config.redis_url).unwrap().into();

    loop {
        let redis_client = redis::Client::open(config.redis_url).unwrap();
        let mut conn = redis_client.get_async_connection().await?;
        if let Some(subtask) = conn.brpop::<_, _, String>(vec!["subtasks"], 0).await? {
            let task = subtask.1;
            let result = client.post(&config.ollama_endpoint)
                .json(&json!({ "input": task, "model": "qwen2.5-coder:7b" }))
                .send()
                .await?
                .text()
                .await?;

            let redis_client = redis::Client::open(config.redis_url).unwrap();
            let mut conn = redis_client.get_async_connection().await?;
            conn.lpush("results", result).await?;
        }
    }
}