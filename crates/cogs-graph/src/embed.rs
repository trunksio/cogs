//! Embedding providers (Ollama, OpenAI). Blocking HTTP — embedding runs
//! inside the sync path on the indexer thread, never on a latency-critical
//! request path.

use anyhow::{bail, Context, Result};
use cogs_core::config::EmbeddingsSection;

pub trait EmbeddingProvider: Send + Sync {
    fn dim(&self) -> u32;
    /// Embed a document (bare). Used when indexing note/resource bodies.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Embed a search query. For asymmetric models (Qwen3-Embedding etc.) the
    /// query carries a retrieval instruction the document side omits; the
    /// default is symmetric (same as `embed`).
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed(text)
    }
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

/// OpenAI-compatible `/v1/embeddings` (OpenAI cloud, omlx, vLLM, LM Studio).
/// `base_url` selects the server; an optional bearer key covers the cloud.
pub struct OpenAiCompatEmbedProvider {
    base_url: String,
    model: String,
    api_key: Option<String>,
    char_cap: usize,
    /// Retrieval instruction for query embeddings (asymmetric models); empty
    /// means symmetric.
    query_instruction: String,
    client: reqwest::blocking::Client,
}

impl OpenAiCompatEmbedProvider {
    fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let snippet: String = if text.is_empty() {
            " ".into()
        } else {
            text.chars().take(self.char_cap).collect()
        };
        let mut req = self
            .client
            .post(format!("{}/embeddings", self.base_url.trim_end_matches('/')))
            .json(&serde_json::json!({ "model": self.model, "input": snippet }));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp: serde_json::Value = req.send()?.error_for_status()?.json()?;
        let vec = resp["data"][0]["embedding"]
            .as_array()
            .context("no embedding in openai-compatible response")?
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        Ok(vec)
    }
}

impl EmbeddingProvider for OpenAiCompatEmbedProvider {
    fn dim(&self) -> u32 {
        0
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_text(text)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        if self.query_instruction.is_empty() {
            self.embed_text(text)
        } else {
            // Qwen3-Embedding query format.
            self.embed_text(&format!("Instruct: {}\nQuery: {}", self.query_instruction, text))
        }
    }
}

/// Build a provider from config and probe it once: a misconfigured provider
/// or dim mismatch fails fast instead of failing per-note inside the loop
/// (port of sync_graph.py::_make_embed_fn).
pub fn make_provider(cfg: &EmbeddingsSection) -> Result<Box<dyn EmbeddingProvider>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    // Resolve an OpenAI-compatible base URL per provider; `endpoint` overrides.
    let default_base = match cfg.provider.as_str() {
        "omlx" => "http://localhost:8000/v1",
        "openai" => "https://api.openai.com/v1",
        _ => "",
    };
    let base_url = if cfg.endpoint.is_empty() { default_base.to_string() } else { cfg.endpoint.clone() };

    // Bearer key from the configured env var (omlx/cloud); falls back to
    // OPENAI_API_KEY so existing setups keep working.
    let key_from_env = || -> Option<String> {
        let var = if cfg.api_key_env.is_empty() { "OPENAI_API_KEY" } else { cfg.api_key_env.as_str() };
        std::env::var(var).ok().filter(|s| !s.is_empty())
    };

    let provider: Box<dyn EmbeddingProvider> = match cfg.provider.as_str() {
        // Native Ollama API (/api/embeddings) — kept for existing vaults.
        "ollama" => Box::new(OllamaProvider {
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            char_cap: cfg.char_cap as usize,
            client,
        }),
        // OpenAI cloud — requires a key.
        "openai" => Box::new(OpenAiCompatEmbedProvider {
            base_url,
            model: cfg.model.clone(),
            api_key: Some(
                key_from_env()
                    .context("embeddings provider 'openai' requires OPENAI_API_KEY")?,
            ),
            char_cap: cfg.char_cap as usize,
            query_instruction: cfg.query_instruction.clone(),
            client,
        }),
        // omlx and any other OpenAI-compatible local server (key optional).
        _ => {
            if base_url.is_empty() {
                bail!(
                    "embeddings provider {:?} needs [embeddings].endpoint \
                     (an OpenAI-compatible /v1 base URL)",
                    cfg.provider
                );
            }
            Box::new(OpenAiCompatEmbedProvider {
                base_url,
                model: cfg.model.clone(),
                api_key: key_from_env(),
                char_cap: cfg.char_cap as usize,
                query_instruction: cfg.query_instruction.clone(),
                client,
            })
        }
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
