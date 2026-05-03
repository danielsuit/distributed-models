use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Serialize)]
struct ShowBody<'a> {
    model: &'a str,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    embedding: Vec<f32>,
}

fn json_unsigned_int(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|u| u32::try_from(u).ok()),
        _ => None,
    }
}

/// Parse `*.context_length` from an Ollama `/api/show` JSON body.
pub(crate) fn parse_native_context_from_show(json: &Value) -> Option<u32> {
    if let Some(model_info) = json.get("model_info").and_then(|v| v.as_object()) {
        let family = json
            .get("details")
            .and_then(|d| d.get("family"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                json.get("details")?
                    .get("families")?
                    .as_array()?
                    .first()?
                    .as_str()
            });

        if let Some(f) = family {
            let key = format!("{f}.context_length");
            if let Some(n) = model_info.get(&key).and_then(json_unsigned_int) {
                return Some(n);
            }
        }

        let mut best: Option<u32> = None;
        for (k, val) in model_info {
            if !k.ends_with(".context_length") {
                continue;
            }
            if let Some(u) = json_unsigned_int(val) {
                best = Some(best.map_or(u, |b| b.max(u)));
            }
        }
        if let Some(u) = best {
            return Some(u);
        }
    }

    let params = json.get("parameters")?.as_str()?;
    for line in params.lines() {
        let mut t = line.trim();
        t = match t.strip_prefix("PARAMETER").map(str::trim) {
            Some(rest) => rest,
            None => t,
        };
        let rest = match t.strip_prefix("num_ctx") {
            Some(r) => r.trim(),
            None => continue,
        };
        if let Ok(n) = rest.parse::<u32>() {
            return Some(n);
        }
    }

    None
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

    /// Best-effort look-up of GGUF-trained context tokens for `model`.
    ///
    /// Uses a short HTTP timeout so `/config` stays responsive when Ollama is down.
    pub async fn native_context_capacity(&self, model: &str) -> Result<Option<u32>> {
        let short = Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("ollama show client");
        let url = format!("{}/api/show", self.base_url);
        let name = model.trim();
        if name.is_empty() {
            return Ok(None);
        }
        let response = short
            .post(&url)
            .json(&ShowBody { model: name })
            .send()
            .await
            .with_context(|| format!("POST ollama show at {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            tracing::debug!(
                status = %status,
                body = %text,
                "ollama show returned non-success for model {}",
                name
            );
            return Ok(None);
        }

        let v: Value = response.json().await.context("decode ollama show json")?;
        Ok(parse_native_context_from_show(&v))
    }

    /// Run a prompt against a specific model. Returns the model's textual
    /// completion. Failures bubble up as `anyhow::Error` so callers can
    /// surface them on the bus.
    pub async fn generate(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
        num_ctx: u32,
    ) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);
        let body = GenerateRequest {
            model,
            prompt,
            system,
            stream: false,
            options: Some(GenerateOptions {
                temperature: Some(0.2),
                num_ctx: Some(num_ctx),
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

        let parsed: GenerateResponse = response.json().await.context("decoding ollama response")?;

        if !parsed.done {
            tracing::warn!("ollama response not marked done; using partial output");
        }

        Ok(parsed.response)
    }

    /// Compute an embedding for `prompt` using `model`. Used by the
    /// semantic codebase index. Note: Ollama also serves
    /// `/api/embed` (newer); we use `/api/embeddings` for broader model
    /// compatibility.
    pub async fn embed(&self, model: &str, prompt: &str) -> Result<Vec<f32>> {
        let url = format!("{}/api/embeddings", self.base_url);
        let body = EmbedRequest { model, prompt };
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("posting to ollama embeddings at {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("ollama embeddings returned {status}: {text}");
        }
        let parsed: EmbedResponse =
            response.json().await.context("decoding ollama embedding")?;
        if parsed.embedding.is_empty() {
            anyhow::bail!("ollama returned an empty embedding for model {model}");
        }
        Ok(parsed.embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_llama_style_context_length() {
        let body = json!({
            "details": { "family": "llama" },
            "model_info": { "llama.context_length": 131072_u64 }
        });
        assert_eq!(parse_native_context_from_show(&body), Some(131072));
    }

    #[test]
    fn parses_num_ctx_from_parameters_fallback() {
        let body = json!({
            "parameters": "num_ctx   4096\ntemperature 0.5\n"
        });
        assert_eq!(parse_native_context_from_show(&body), Some(4096));
    }

    #[test]
    fn picks_max_ambiguous_suffixes() {
        let body = json!({
            "model_info": {
                "vision.context_length": 2048_u64,
                "llama.context_length": 8192_u64
            }
        });
        assert_eq!(parse_native_context_from_show(&body), Some(8192));
    }
}
