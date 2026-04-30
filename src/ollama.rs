use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// A thin async wrapper around the parts of the Ollama HTTP API we actually
/// use. The agents only need the non-streaming `generate` endpoint to keep the
/// wire format simple.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    base_url: String,
    http: Client,
}

#[derive(Debug, Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    /// `false` makes Ollama return one consolidated JSON object instead of an
    /// NDJSON stream.
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<GenerateOptions>,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct GenerateOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GenerateResponse {
    pub response: String,
    #[serde(default)]
    pub done: bool,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .expect("ollama reqwest client"),
        }
    }

    /// Run a prompt against a specific model. Returns the model's textual
    /// completion. Failures bubble up as `anyhow::Error` so callers can
    /// surface them on the bus.
    pub async fn generate(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
    ) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);
        let body = GenerateRequest {
            model,
            prompt,
            system,
            stream: false,
            options: Some(GenerateOptions {
                temperature: Some(0.2),
                num_ctx: Some(8192),
            }),
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("posting to ollama at {url}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("ollama returned {status}: {text}");
        }

        let parsed: GenerateResponse =
            response.json().await.context("decoding ollama response")?;

        if !parsed.done {
            tracing::warn!("ollama response not marked done; using partial output");
        }

        Ok(parsed.response)
    }
}
