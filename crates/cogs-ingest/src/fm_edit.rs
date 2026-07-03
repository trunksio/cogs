//! Textual frontmatter edits for existing pages. Never re-serialises YAML:
//! real vault files carry duplicate keys and comments that a round-trip would
//! destroy. We locate the frontmatter block and splice lines.

use anyhow::{bail, Result};

use cogs_core::parse::split_frontmatter;

/// Append `item` to the top-level `field:` list, handling block lists, flow
/// lists (`[a, b]` / `[]`), and a missing key. No-op when already present.
pub fn add_list_item(file_text: &str, field: &str, item: &str) -> Result<String> {
    let (yaml, body) = split(file_text)?;
    let lines: Vec<&str> = yaml.lines().collect();

    let key_idx = find_key(&lines, field);
    let new_yaml = match key_idx {
        None => {
            // Missing key: append a block list at the end of the frontmatter.
            let mut ls: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            ls.push(format!("{field}:"));
            ls.push(format!("  - {item}"));
            ls.join("\n")
        }
        Some(i) => {
            let value = lines[i][field.len() + 1..].trim();
            if value.is_empty() || value.starts_with('#') {
                // Block list: skip existing items, insert after the last one.
                let mut end = i + 1;
                let mut indent = "  ";
                while end < lines.len() {
                    let l = lines[end];
                    let t = l.trim_start();
                    if t.starts_with("- ") || t == "-" {
                        indent = &l[..l.len() - t.len()];
                        if t[1..].trim() == item {
                            return Ok(file_text.to_string());
                        }
                        end += 1;
                    } else {
                        break;
                    }
                }
                let mut ls: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
                ls.insert(end, format!("{indent}- {item}"));
                ls.join("\n")
            } else if value.starts_with('[') {
                // Flow list on one line.
                let inner = value.trim_start_matches('[').trim_end_matches(']').trim();
                let present = inner
                    .split(',')
                    .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
                    .any(|s| s == item);
                if present {
                    return Ok(file_text.to_string());
                }
                let new_value = if inner.is_empty() {
                    format!("[{item}]")
                } else {
                    format!("[{inner}, {item}]")
                };
                let mut ls: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
                ls[i] = format!("{field}: {new_value}");
                ls.join("\n")
            } else {
                bail!("frontmatter field {field:?} is a scalar, not a list");
            }
        }
    };
    Ok(rebuild(&new_yaml, body))
}

/// Set the top-level scalar `field:` to `value`, replacing the last
/// occurrence (duplicate keys are last-wins in this vault's YAML dialect) or
/// appending the key when missing.
pub fn set_scalar(file_text: &str, field: &str, value: &str) -> Result<String> {
    let (yaml, body) = split(file_text)?;
    let mut lines: Vec<String> = yaml.lines().map(|s| s.to_string()).collect();
    let last = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_key_line(l, field))
        .map(|(i, _)| i)
        .last();
    match last {
        Some(i) => lines[i] = format!("{field}: {value}"),
        None => lines.push(format!("{field}: {value}")),
    }
    Ok(rebuild(&lines.join("\n"), body))
}

fn split(file_text: &str) -> Result<(&str, &str)> {
    let (yaml, _, body, _) = split_frontmatter(file_text);
    match yaml {
        Some(y) => Ok((y, body)),
        None => bail!("file has no frontmatter block"),
    }
}

fn rebuild(yaml: &str, body: &str) -> String {
    format!("---\n{yaml}\n---\n{body}")
}

fn is_key_line(line: &str, field: &str) -> bool {
    line.starts_with(field) && line[field.len()..].starts_with(':')
}

fn find_key(lines: &[&str], field: &str) -> Option<usize> {
    lines.iter().position(|l| is_key_line(l, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "---\ntitle: T\nsource_refs:\n  - raw/a.md\n  - raw/b.md\ntags: [x]\n---\nbody [[link]]\n";

    #[test]
    fn block_list_appends_after_last_item() {
        let out = add_list_item(BLOCK, "source_refs", "raw/c.md").unwrap();
        assert!(out.contains("  - raw/b.md\n  - raw/c.md\ntags:"), "out:\n{out}");
        assert!(out.ends_with("---\nbody [[link]]\n"));
    }

    #[test]
    fn block_list_is_idempotent() {
        let out = add_list_item(BLOCK, "source_refs", "raw/b.md").unwrap();
        assert_eq!(out, BLOCK);
    }

    #[test]
    fn flow_list_grows_and_empty_flow_fills() {
        let text = "---\ntags: [a, b]\nrefs: []\n---\nbody\n";
        let out = add_list_item(text, "tags", "c").unwrap();
        assert!(out.contains("tags: [a, b, c]"));
        let out = add_list_item(&out, "refs", "raw/x.md").unwrap();
        assert!(out.contains("refs: [raw/x.md]"));
        // idempotent on flow too
        let again = add_list_item(&out, "tags", "b").unwrap();
        assert_eq!(again, out);
    }

    #[test]
    fn missing_key_inserted_as_block() {
        let text = "---\ntitle: T\n---\nbody\n";
        let out = add_list_item(text, "source_refs", "raw/a.md").unwrap();
        assert!(out.contains("title: T\nsource_refs:\n  - raw/a.md\n---"), "out:\n{out}");
    }

    #[test]
    fn scalar_field_errors() {
        let text = "---\ntitle: T\n---\nbody\n";
        assert!(add_list_item(text, "title", "x").is_err());
    }

    #[test]
    fn set_scalar_replaces_last_duplicate_and_preserves_comments() {
        let text = "---\n# comment kept\nupdated: 2026-01-01\ntitle: T\nupdated: 2026-02-02\n---\nbody\n";
        let out = set_scalar(text, "updated", "2026-07-03").unwrap();
        assert!(out.contains("# comment kept"));
        assert!(out.contains("updated: 2026-01-01")); // first duplicate untouched
        assert!(out.contains("updated: 2026-07-03"));
        assert!(!out.contains("updated: 2026-02-02"));
    }

    #[test]
    fn set_scalar_inserts_when_missing() {
        let out = set_scalar("---\ntitle: T\n---\nbody\n", "updated", "2026-07-03").unwrap();
        assert!(out.contains("title: T\nupdated: 2026-07-03\n---"));
    }

    #[test]
    fn no_frontmatter_is_an_error() {
        assert!(add_list_item("just a body\n", "source_refs", "x").is_err());
    }

    #[test]
    fn indented_similar_keys_are_not_matched() {
        let text = "---\nmeta:\n  tags: [inner]\ntags: [outer]\n---\nbody\n";
        let out = add_list_item(text, "tags", "new").unwrap();
        assert!(out.contains("tags: [outer, new]"));
        assert!(out.contains("  tags: [inner]"));
    }
}
