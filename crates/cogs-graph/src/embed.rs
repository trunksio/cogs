//! Embedding providers (Ollama, OpenAI). Blocking HTTP — embedding runs
//! inside the sync path on the indexer thread, never on a latency-critical
//! request path.

use anyhow::{bail, Context, Result};
use cogs_core::config::EmbeddingsSection;

pub trait EmbeddingProvider: Send + Sync {
    fn dim(&self) -> u32;
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

pub struct OllamaProvider {
    endpoint: String,
    model: String,
    char_cap: usize,
    client: reqwest::blocking::Client,
}

impl EmbeddingProvider for OllamaProvider {
    fn dim(&self) -> u32 {
        0 // unknown until probed; make_provider verifies against config
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Ollama's nomic-style models 500 on inputs beyond their token
        // context. The char cap approximates that, but token-dense content
        // (URLs, code) can still blow it — halve and retry down to 1000 chars.
        let mut cap = self.char_cap;
        loop {
            let snippet: String = if text.is_empty() {
                " ".into()
            } else {
                text.chars().take(cap).collect()
            };
            let resp = self
                .client
                .post(format!("{}/api/embeddings", self.endpoint))
                .json(&serde_json::json!({ "model": self.model, "prompt": snippet }))
                .send()?;
            if resp.status().is_server_error() && cap > 1000 {
                cap /= 2;
                continue;
            }
            let resp: serde_json::Value = resp.error_for_status()?.json()?;
            let vec: Vec<f32> = resp
                .get("embedding")
                .and_then(|v| v.as_array())
                .context("no embedding in ollama response")?
                .iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect();
            return Ok(vec);
        }
    }
}

pub struct OpenAiProvider {
    model: String,
    api_key: String,
    char_cap: usize,
    client: reqwest::blocking::Client,
}

impl EmbeddingProvider for OpenAiProvider {
    fn dim(&self) -> u32 {
        0
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let snippet: String = if text.is_empty() {
            " ".into()
        } else {
            text.chars().take(self.char_cap).collect()
        };
        let resp: serde_json::Value = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "model": self.model, "input": snippet }))
            .send()?
            .error_for_status()?
            .json()?;
        let vec = resp["data"][0]["embedding"]
            .as_array()
            .context("no embedding in openai response")?
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        Ok(vec)
    }
}

/// Build a provider from config and probe it once: a misconfigured provider
/// or dim mismatch fails fast instead of failing per-note inside the loop
/// (port of sync_graph.py::_make_embed_fn).
pub fn make_provider(cfg: &EmbeddingsSection) -> Result<Box<dyn EmbeddingProvider>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let provider: Box<dyn EmbeddingProvider> = match cfg.provider.as_str() {
        "ollama" => Box::new(OllamaProvider {
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            char_cap: cfg.char_cap as usize,
            client,
        }),
        "openai" => Box::new(OpenAiProvider {
            model: cfg.model.clone(),
            api_key: std::env::var("OPENAI_API_KEY")
                .context("embeddings provider 'openai' requires OPENAI_API_KEY")?,
            char_cap: cfg.char_cap as usize,
            client,
        }),
        other => bail!("unknown embeddings provider {other:?} (expected ollama|openai)"),
    };
    let sample = provider
        .embed("cogs embedding startup probe")
        .with_context(|| format!("embedding provider {:?} failed startup probe", cfg.provider))?;
    if sample.len() as u32 != cfg.dim {
        bail!(
            "provider {:?} returned dim={} but config says dim={}; \
             fix [embeddings].dim (the DB will rebuild automatically)",
            cfg.provider,
            sample.len(),
            cfg.dim
        );
    }
    Ok(provider)
}
