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

pub mod training;

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

/// Salvage a JSON value that was cut off mid-generation (max_tokens): trim
/// back to the last complete element and close the open brackets. Returns
/// None when the text isn't a truncated JSON prefix (malformed nesting, or
/// nothing complete to keep).
pub fn repair_truncated_json(s: &str) -> Option<String> {
    let start = s.find(['{', '['])?;
    let bytes = s[start..].as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    // Position just past the last complete value, and the open-bracket stack
    // at that moment.
    let mut last_good: Option<(usize, Vec<u8>)> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            match b {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => {
                    in_str = false;
                    last_good = Some((i + 1, stack.clone()));
                }
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                if stack.pop() != Some(b) {
                    return None; // malformed, not merely truncated
                }
                if stack.is_empty() {
                    return None; // complete value — nothing to repair
                }
                last_good = Some((i + 1, stack.clone()));
            }
            _ => {}
        }
    }
    let (end, mut open) = last_good?;
    let mut out = s[start..start + end].trim_end().trim_end_matches(',').trim_end().to_string();
    // The cut may land right after a bare object KEY (`…, "key"` with its
    // value lost to truncation) — detect it by what precedes the final
    // string: ':' means value (fine), '{' or ',' inside an object means key.
    if open.last() == Some(&b'}') && out.ends_with('"') {
        if let Some(qopen) = find_string_open(&out) {
            let before = out[..qopen].trim_end();
            if before.ends_with(['{', ',']) {
                let mut cut = before.len();
                if before.ends_with(',') {
                    cut -= 1;
                }
                out.truncate(cut);
            }
        }
    }
    // Don't leave freshly opened but empty containers behind (`…, {` would
    // close into an empty {} list item and can break typed parsing).
    loop {
        let trimmed = out.trim_end().trim_end_matches(',').trim_end().to_string();
        let last = trimmed.as_bytes().last().copied();
        let expected = open.last().copied();
        match (last, expected) {
            (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']')) => {
                open.pop();
                out = trimmed;
                out.pop();
                out = out.trim_end().trim_end_matches(',').trim_end().to_string();
            }
            _ => break,
        }
    }
    if out.len() <= 1 {
        return None; // nothing complete survived
    }
    for closer in open.iter().rev() {
        out.push(*closer as char);
    }
    Some(out)
}

/// Byte offset of the opening quote of a string that ends at the last byte
/// of `s` (which must be `"`), honouring backslash escapes.
fn find_string_open(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = bytes.len().checked_sub(2)?;
    loop {
        if bytes[i] == b'"' {
            // count preceding backslashes; even = real quote
            let mut bs = 0;
            while i > bs && bytes[i - 1 - bs] == b'\\' {
                bs += 1;
            }
            if bs % 2 == 0 {
                return Some(i);
            }
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

// ---- OpenAI-compatible (omlx / Ollama / OpenAI / vLLM) -------------------

pub struct OpenAiCompatProvider {
    label: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Whether json-mode calls set `response_format: json_object`.
    response_format: bool,
    /// Extra body fields merged into every request ([llm].extra_body).
    extra_body: serde_json::Value,
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
        if params.json && self.response_format {
            body["response_format"] = json!({ "type": "json_object" });
        }
        if let Some(extra) = self.extra_body.as_object() {
            for (k, v) in extra {
                body[k] = v.clone();
            }
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
        .timeout(std::time::Duration::from_secs(cfg.timeout_secs.max(30)))
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
            // omlx's json_object constrained decoding degenerates (emits
            // float arrays), so it defaults off there; prompts demand JSON
            // regardless and callers parse tolerantly.
            let response_format = cfg.response_format.unwrap_or(provider != "omlx");
            Ok(Box::new(OpenAiCompatProvider {
                label: cfg.provider.clone(),
                base_url,
                model: cfg.model.clone(),
                api_key,
                response_format,
                extra_body: cfg.extra_body.clone(),
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
    fn repairs_truncation_mid_string_dropping_the_partial_value() {
        let cut = r#"{"summary": "ok", "key_claims": [{"text": "one"}, {"text": "two, unfin"#;
        let fixed = repair_truncated_json(cut).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        // the complete claim survives; the half-generated one is dropped,
        // never half-kept
        assert_eq!(v["key_claims"][0]["text"], "one");
        assert!(fixed.matches("unfin").count() == 0);
    }

    #[test]
    fn repairs_truncation_after_bare_key() {
        let cut = r#"{"summary": "ok", "key_claims": [{"text": "one"}], "quotes""#;
        let fixed = repair_truncated_json(cut).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["key_claims"][0]["text"], "one");
        assert!(v.get("quotes").is_none());
    }

    #[test]
    fn repairs_truncation_after_dangling_colon() {
        let cut = r#"{"a": [1, 2], "b": {"x": "y", "z":"#;
        let fixed = repair_truncated_json(cut).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["b"]["x"], "y");
        assert_eq!(v["a"][1], 2);
    }

    #[test]
    fn repair_refuses_complete_or_malformed() {
        assert!(repair_truncated_json(r#"{"a": 1}"#).is_none());
        assert!(repair_truncated_json(r#"{"a": ]"#).is_none());
        assert!(repair_truncated_json("no json here").is_none());
    }

    #[test]
    fn provider_factory_defaults_to_omlx_localhost() {
        let cfg = LlmSection::default();
        let p = make_provider(&cfg).unwrap();
        assert_eq!(p.name(), "omlx");
    }
}
