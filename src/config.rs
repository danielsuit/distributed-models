// src/config.rs
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub redis_url: String,
    pub ollama_endpoint: String,
    pub worker_uuid: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            redis_url: env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_string()),
            ollama_endpoint: env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434/api/generate".to_string()),
            worker_uuid: uuid::Uuid::new_v4().to_string(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        envy::from_env::<Config>().expect("Failed to load configuration")
    }
}
    }
}