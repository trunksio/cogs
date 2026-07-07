//! Tolerant handling of LLM JSON replies: balanced-JSON extraction from
//! prose/fenced output, salvage of max_tokens truncations, and the combined
//! parse ladder every pipeline reply goes through (native teacher calls via
//! cogs-llm, browser ingest via cogs-wasm).

use anyhow::{Context, Result};

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

/// The full tolerant parse ladder for a model reply: balanced JSON if
/// present, else truncation salvage; then typed parse, unwrapping the
/// one-element-array wrapper (`[{...}]`) local models sometimes emit.
/// This is THE reply parser — native teacher calls and browser ingest both
/// go through it.
pub fn parse_json_reply<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    let candidate: Option<String> =
        extract_json(raw).map(str::to_string).or_else(|| repair_truncated_json(raw));
    candidate.context("no JSON object/array in model reply").and_then(|s| {
        serde_json::from_str(&s)
            .or_else(|e| {
                // Models sometimes wrap the object in a one-element array:
                // [{...}] — unwrap and retry.
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(serde_json::Value::Array(a)) if a.len() == 1 && a[0].is_object() => {
                        serde_json::from_value(a.into_iter().next().unwrap())
                    }
                    _ => Err(e),
                }
            })
            .context("model reply was not the expected JSON shape")
    })
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
    fn parse_reply_handles_fences_truncation_and_array_wrap() {
        #[derive(serde::Deserialize)]
        struct T {
            a: u32,
        }
        // fenced
        assert_eq!(parse_json_reply::<T>("```json\n{\"a\": 1}\n```").unwrap().a, 1);
        // one-element array wrapper
        assert_eq!(parse_json_reply::<T>(r#"[{"a": 2}]"#).unwrap().a, 2);
        // truncated mid-generation
        assert_eq!(parse_json_reply::<T>(r#"{"a": 3, "b": [{"x": "y"}, {"x": "unfin"#).unwrap().a, 3);
        // hopeless
        assert!(parse_json_reply::<T>("no json").is_err());
    }
}
