//! Small text utilities shared by the ingest pipeline, the validators, and
//! distill mining.

/// Collapse whitespace for formatting-insensitive comparison.
pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whitespace-collapse `s`, tracking each normalized char's source byte offset.
fn normalized_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut map = Vec::new();
    let mut last_ws = true;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if !last_ws {
                out.push(' ');
                map.push(i);
                last_ws = true;
            }
        } else {
            out.push(ch);
            map.push(i);
            last_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// If `needle` appears in `haystack` up to whitespace differences, return the
/// exact haystack slice (trimmed) — recovering the verbatim quote even when
/// a model mangled line breaks.
pub fn find_verbatim(haystack: &str, needle: &str) -> Option<String> {
    let (h_norm, map) = normalized_with_map(haystack);
    let (n_norm, _) = normalized_with_map(needle);
    if n_norm.is_empty() {
        return None;
    }
    let pos = h_norm.find(&n_norm)?;
    let start_char = h_norm[..pos].chars().count();
    let end_char = start_char + n_norm.chars().count();
    let src_start = *map.get(start_char)?;
    let src_last = *map.get(end_char - 1)?;
    let last_len = haystack[src_last..].chars().next()?.len_utf8();
    Some(haystack[src_start..src_last + last_len].trim().to_string())
}

/// Truncate at a char boundary at or below `cap` bytes.
pub fn truncate_chars(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The text under `## <heading>` (exact match after the `## `), up to the
/// next `## ` heading or EOF.
pub fn section<'a>(body: &'a str, heading: &str) -> Option<&'a str> {
    let needle = format!("## {heading}");
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if line.trim_end() == needle {
            let start = offset + line.len();
            let rest = &body[start..];
            let end = rest.find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
        offset += line.len();
    }
    None
}

/// Top-level `- ` bullet items of a block of markdown.
pub fn bullet_items(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("- "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_verbatim_tolerates_whitespace() {
        let hay = "The quick\n  brown   fox\njumps.";
        assert_eq!(
            find_verbatim(hay, "quick brown fox jumps."),
            Some("quick\n  brown   fox\njumps.".to_string())
        );
        assert_eq!(find_verbatim(hay, "quick red fox"), None);
    }

    #[test]
    fn find_verbatim_handles_multibyte() {
        let hay = "héllo wörld — done";
        assert_eq!(find_verbatim(hay, "wörld — done"), Some("wörld — done".to_string()));
    }

    #[test]
    fn section_extracts_until_next_heading() {
        let body = "# T\n\n## Summary\n\nStuff here.\nMore.\n\n## Key claims\n\n- one\n- two\n";
        assert_eq!(section(body, "Summary").unwrap().trim(), "Stuff here.\nMore.");
        assert_eq!(bullet_items(section(body, "Key claims").unwrap()), vec!["one", "two"]);
        assert!(section(body, "Missing").is_none());
    }
}
