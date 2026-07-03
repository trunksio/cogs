//! Pluggable chat-completion providers for `cogs ask` and ingest.
//!
//! Mirrors the embedding-provider pattern in cogs-graph: a small trait, a
//! blocking HTTP client (callers hop to a worker thread — never call this on
//! a tokio worker or an editor's latency path), and a config-driven factory.
//!
//! Two backends cover everything: an OpenAI-compatible client (omlx, Ollama,
//! OpenAI, vLLM — anything speaking /v1/chat/completions) and a native
//! Anthropic adapter. The long-term "finetuned model embedded in cogs" plan
//! lands as another OpenAI-compatible endpoint (a managed local MLX server),
//! so it slots in here without touching callers.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use cogs_core::config::LlmSection;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: Role::System, content: s.into() }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: Role::User, content: s.into() }
    }
    pub fn assistant(s: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: s.into() }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionParams {
    pub temperature: f32,
    pub max_tokens: u32,
    /// Hint that the reply must be a single JSON value. OpenAI-compatible
    /// providers get response_format=json_object; others get a strong system
    /// nudge (added by the caller) — we also defensively extract JSON.
    pub json: bool,
}

impl Default for CompletionParams {
    fn default() -> Self {
        Self { temperature: 0.2, max_tokens: 2048, json: false }
    }
}

pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    fn complete(&self, messages: &[Message], params: &CompletionParams) -> Result<String>;
}

/// Complete and parse the reply as JSON of type T, tolerating models that wrap
/// JSON in prose or ```json fences. Free function (not a trait method) so
/// `ChatProvider` stays object-safe / usable as `dyn ChatProvider`.
pub fn complete_json<T: serde::de::DeserializeOwned>(
    provider: &dyn ChatProvider,
    messages: &[Message],
    params: &CompletionParams,
) -> Result<T> {
    let mut p = params.clone();
    p.json = true;
    let raw = provider.complete(messages, &p)?;
    let slice = extract_json(&raw)
        .with_context(|| format!("no JSON object/array in model reply: {raw:.200}"))?;
    serde_json::from_str(slice)
        .with_context(|| format!("model reply was not the expected JSON shape: {slice:.200}"))
}

/// Find the first balanced JSON object or array in a string (handles fenced
/// or prose-wrapped replies). Returns the slice, not a parsed value. Public so
/// callers that need the raw reply (e.g. ingest training capture) can complete
/// and parse in two steps.
pub fn extract_json(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            match b {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            x if x == open => depth += 1,
            x if x == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---- OpenAI-compatible (omlx / Ollama / OpenAI / vLLM) -------------------

pub struct OpenAiCompatProvider {
    label: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::blocking::Client,
}

impl OpenAiCompatProvider {
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

impl ChatProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.label
    }

    fn complete(&self, messages: &[Message], params: &CompletionParams) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": params.temperature,
            "max_tokens": params.max_tokens,
            "stream": false,
        });
        if params.json {
            body["response_format"] = json!({ "type": "json_object" });
        }
        let mut req = self.client.post(self.endpoint()).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().context("LLM request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            // Some local servers reject response_format; retry once without it.
            if params.json && (status.is_client_error()) {
                let mut p = params.clone();
                p.json = false;
                return self.complete(messages, &p);
            }
            bail!("LLM provider {} returned {status}: {text:.300}", self.label);
        }
        let v: serde_json::Value = resp.json().context("LLM reply not JSON")?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .context("LLM reply missing choices[0].message.content")
    }
}

// ---- Anthropic ------------------------------------------------------------

pub struct AnthropicProvider {
    model: String,
    api_key: String,
    client: reqwest::blocking::Client,
}

impl ChatProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn complete(&self, messages: &[Message], params: &CompletionParams) -> Result<String> {
        // Anthropic takes system separately from the turn list.
        let system: String = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let turns: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| {
                let role = if m.role == Role::Assistant { "assistant" } else { "user" };
                json!({ "role": role, "content": m.content })
            })
            .collect();
        let body = json!({
            "model": self.model,
            "max_tokens": params.max_tokens,
            "temperature": params.temperature,
            "system": system,
            "messages": turns,
        });
        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .context("Anthropic request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            bail!("Anthropic returned {status}: {text:.300}");
        }
        let v: serde_json::Value = resp.json().context("Anthropic reply not JSON")?;
        v["content"][0]["text"]
            .as_str()
            .map(str::to_string)
            .context("Anthropic reply missing content[0].text")
    }
}

/// Build a provider from `[llm]` config. Resolves provider-default base URLs
/// and reads API keys from the environment.
pub fn make_provider(cfg: &LlmSection) -> Result<Box<dyn ChatProvider>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()?;

    let env_key = |default_var: &str| -> Option<String> {
        let var = if cfg.api_key_env.is_empty() { default_var } else { cfg.api_key_env.as_str() };
        std::env::var(var).ok().filter(|s| !s.is_empty())
    };

    let provider = cfg.provider.to_lowercase();
    let default_base = match provider.as_str() {
        "omlx" => "http://localhost:8000/v1",
        "ollama" => "http://localhost:11434/v1",
        "openai" => "https://api.openai.com/v1",
        _ => "",
    };
    let base_url = if cfg.base_url.is_empty() { default_base.to_string() } else { cfg.base_url.clone() };

    match provider.as_str() {
        "anthropic" => {
            let api_key = env_key("ANTHROPIC_API_KEY")
                .context("[llm] provider=anthropic requires ANTHROPIC_API_KEY")?;
            Ok(Box::new(AnthropicProvider { model: cfg.model.clone(), api_key, client }))
        }
        // omlx / ollama / openai / vLLM / any OpenAI-compatible endpoint.
        _ => {
            if base_url.is_empty() {
                bail!(
                    "[llm] provider={:?} needs base_url (OpenAI-compatible endpoint)",
                    cfg.provider
                );
            }
            let api_key = if provider == "openai" {
                Some(env_key("OPENAI_API_KEY").context("[llm] provider=openai requires OPENAI_API_KEY")?)
            } else {
                env_key("OPENAI_API_KEY") // optional for local servers
            };
            Ok(Box::new(OpenAiCompatProvider {
                label: cfg.provider.clone(),
                base_url,
                model: cfg.model.clone(),
                api_key,
                client,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_object() {
        assert_eq!(extract_json(r#"{"a":1}"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extracts_fenced_object() {
        let s = "Here you go:\n```json\n{\"a\": [1,2], \"b\": \"x\"}\n```\ndone";
        assert_eq!(extract_json(s), Some("{\"a\": [1,2], \"b\": \"x\"}"));
    }

    #[test]
    fn extracts_array_and_ignores_braces_in_strings() {
        let s = r#"prefix [{"k": "}{"}] suffix"#;
        assert_eq!(extract_json(s), Some(r#"[{"k": "}{"}]"#));
    }

    #[test]
    fn none_when_no_json() {
        assert_eq!(extract_json("just prose"), None);
    }

    #[test]
    fn provider_factory_defaults_to_omlx_localhost() {
        let cfg = LlmSection::default();
        let p = make_provider(&cfg).unwrap();
        assert_eq!(p.name(), "omlx");
    }
}
