use std::ops::Range;
use std::sync::LazyLock;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::config::VaultConfig;
use crate::note::{
    parse_date_lenient, EdgeFieldItem, Heading, Link, ParsedNote, ParsedResource,
};

/// `[[target]]`, `[[target|alias]]`, `[[target#anchor]]`, `[[t#a|alias]]`.
/// Group 1 = target, 2 = #anchor, 3 = |alias. Matches sync_graph.py's target
/// grammar (`[^\]|#\n]+`) for parity.
static WIKILINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[\[([^\]\|#\n]+)(#[^\]\|\n]*)?(\|[^\]\n]*)?\]\]").unwrap()
});

/// Full token used when stripping for FTS — matches sync_graph.py's
/// WIKILINK_FULL_RE so body_text (and therefore body_hash) is identical.
static WIKILINK_FULL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]\n]+?)\]\]").unwrap());

static INLINE_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s(])#([A-Za-z][\w/-]*)").unwrap());

/// Inline markdown link `[text](dest)` (group 1 = optional image `!`, 2 =
/// text, 3 = raw destination incl. optional `"title"`). Feeds the OKF-style
/// path-link scan; the destination grammar is deliberately simple (no
/// parens/newlines) — OKF cross-links are plain `.md` paths.
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[([^\]\n]*)\]\(([^()\n]+)\)").unwrap());

/// Any URI scheme prefix (`http:`, `https:`, `mailto:`, …).
static SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:").unwrap());

pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Split a document into (frontmatter_yaml, frontmatter_range, body, body_offset).
/// Frontmatter must start at byte 0 with `---\n` and end at a line that is
/// exactly `---`. Returns None for the frontmatter parts when absent.
pub fn split_frontmatter(text: &str) -> (Option<&str>, Option<Range<usize>>, &str, usize) {
    let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"));
    let Some(rest) = rest else {
        return (None, None, text, 0);
    };
    let opener_len = text.len() - rest.len();
    let mut search_from = 0usize;
    while let Some(pos) = rest[search_from..].find("\n---") {
        let line_start = search_from + pos + 1; // index of '-' in rest
        let after = &rest[line_start + 3..];
        // closing fence must be a full line: `---` then newline or EOF
        let (consumed, valid) = if let Some(r) = after.strip_prefix("\r\n") {
            (rest.len() - r.len(), true)
        } else if let Some(r) = after.strip_prefix('\n') {
            (rest.len() - r.len(), true)
        } else if after.is_empty() {
            (rest.len(), true)
        } else {
            (0, false)
        };
        if valid {
            let yaml = &rest[..search_from + pos];
            let fm_end = opener_len + consumed;
            let body = &text[fm_end..];
            return (Some(yaml), Some(0..fm_end), body, fm_end);
        }
        search_from = line_start + 3;
    }
    (None, None, text, 0)
}

/// Byte ranges of the body covered by code (fenced/indented blocks and inline
/// spans). Offsets are relative to the body slice passed in.
fn code_mask_ranges(body: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let parser = Parser::new_ext(body, Options::empty());
    let mut depth = 0usize;
    let mut start = 0usize;
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                if depth == 0 {
                    start = range.start;
                }
                depth += 1;
            }
            Event::End(TagEnd::CodeBlock) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    ranges.push(start..range.end);
                }
            }
            Event::Code(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {
                ranges.push(range.clone());
            }
            _ => {}
        }
    }
    ranges
}

fn in_ranges(pos: usize, ranges: &[Range<usize>]) -> bool {
    ranges.iter().any(|r| r.contains(&pos))
}

/// Scan wikilinks in `body`; spans are absolute file offsets (body_offset added).
pub fn scan_wikilinks(body: &str, body_offset: usize) -> Vec<Link> {
    let mask = code_mask_ranges(body);
    WIKILINK_RE
        .captures_iter(body)
        .map(|c| {
            let m = c.get(0).unwrap();
            let t = c.get(1).unwrap();
            Link {
                target: t.as_str().trim().to_string(),
                target_span: body_offset + t.start()..body_offset + t.end(),
                anchor: c
                    .get(2)
                    .map(|a| a.as_str().trim_start_matches('#').trim().to_string())
                    .filter(|a| !a.is_empty()),
                alias: c
                    .get(3)
                    .map(|a| a.as_str().trim_start_matches('|').trim().to_string())
                    .filter(|a| !a.is_empty()),
                span: body_offset + m.start()..body_offset + m.end(),
                masked: in_ranges(m.start(), &mask),
            }
        })
        .collect()
}

/// Collapse `.`/`..` segments in a bundle-root-relative path. None when `..`
/// climbs above the root (an escaping link can never resolve — tolerated).
fn normalize_path(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            c => out.push(c),
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("/"))
    }
}

/// Scan inline markdown links whose destination is a `.md` path — the Google
/// Open Knowledge Format cross-link convention (OKF v0.1 §5). Two forms:
/// bundle-root-absolute (`/tables/customers.md`) and relative to the linking
/// file's directory (`./other.md`, `../x.md`). External URLs (any scheme),
/// anchor-only, image, and non-`.md` destinations are ignored. The emitted
/// target is the path-form the resolver already handles: vault-relative path
/// minus the configured id prefix and `.md` ('/'→'-' + lowercase happen in
/// LinkResolver). A target with no matching note resolves Broken downstream —
/// tolerated, no edge, per spec.
///
/// Unlike wikilinks (kept when masked, for sync_graph.py parity), markdown
/// links inside fenced/inline code are skipped entirely — OKF bodies are
/// dense with SQL/code blocks and in-code paths are not assertions.
pub fn scan_markdown_path_links(
    body: &str,
    body_offset: usize,
    rel_path: &str,
    strip_prefix: &str,
) -> Vec<Link> {
    let mask = code_mask_ranges(body);
    let src_dir = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut links = Vec::new();
    for c in MD_LINK_RE.captures_iter(body) {
        let m = c.get(0).unwrap();
        if !c[1].is_empty() || in_ranges(m.start(), &mask) {
            continue; // image, or inside code
        }
        let dest_group = c.get(3).unwrap();
        // The destination is the first whitespace-separated token; the rest
        // is an optional markdown `"title"`.
        let Some(dest) = dest_group.as_str().split_whitespace().next() else {
            continue;
        };
        if dest.starts_with('#') || SCHEME_RE.is_match(dest) {
            continue; // anchor-only, or external (http(s):, mailto:, …)
        }
        let (path, anchor) = match dest.split_once('#') {
            Some((p, a)) => (p, Some(a)),
            None => (dest, None),
        };
        if !path.ends_with(".md") {
            continue;
        }
        // Bundle-root-absolute vs relative to the linking file's directory.
        let joined = match path.strip_prefix('/') {
            Some(abs) => abs.to_string(),
            None if src_dir.is_empty() => path.to_string(),
            None => format!("{src_dir}/{path}"),
        };
        let Some(vault_rel) = normalize_path(&joined) else {
            continue; // `..` escaped the vault root — tolerated, no edge
        };
        let stripped = vault_rel.strip_prefix(strip_prefix).unwrap_or(&vault_rel);
        let Some(target) = stripped.strip_suffix(".md").filter(|t| !t.is_empty()) else {
            continue;
        };
        let dest_off = dest_group.as_str().find(dest).unwrap_or(0);
        let ts_start = body_offset + dest_group.start() + dest_off;
        links.push(Link {
            target: target.to_string(),
            anchor: anchor.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()),
            alias: Some(c[2].trim().to_string()).filter(|a| !a.is_empty()),
            span: body_offset + m.start()..body_offset + m.end(),
            target_span: ts_start..ts_start + path.len(),
            masked: false,
        });
    }
    links
}

/// Replace `[[target|alias]]` with alias, `[[target]]`/`[[target#a]]` with the
/// last path segment of the target. Byte-for-byte port of
/// sync_graph.py::strip_wikilinks_for_fts so body hashes line up.
pub fn strip_wikilinks_for_fts(body: &str) -> String {
    WIKILINK_FULL_RE
        .replace_all(body, |caps: &regex::Captures| {
            let chunk = &caps[1];
            if let Some((_, alias)) = chunk.split_once('|') {
                alias.trim().to_string()
            } else {
                chunk
                    .split_once('#')
                    .map(|(t, _)| t)
                    .unwrap_or(chunk)
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            }
        })
        .to_string()
}

fn heading_outline(body: &str, body_offset: usize) -> Vec<Heading> {
    let mut headings = Vec::new();
    let parser = Parser::new_ext(body, Options::empty());
    let mut current: Option<(u8, usize, String)> = None;
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some((level as u8, range.start, String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, buf)) = current.as_mut() {
                    buf.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, start, text)) = current.take() {
                    headings.push(Heading {
                        level,
                        text: text.trim().to_string(),
                        span: body_offset + start..body_offset + range.end,
                    });
                }
            }
            _ => {}
        }
    }
    headings
}

/// Parse a YAML frontmatter payload into JSON, tolerating real-world mess the
/// way PyYAML does: duplicate keys are last-wins, parse failures yield Null.
/// (Deserializing into serde_json::Value matters: serde_yaml_ng's own Value
/// type rejects duplicate keys, which real vault files contain.)
pub fn yaml_to_json(yaml: &str) -> serde_json::Value {
    serde_yaml_ng::from_str::<serde_json::Value>(yaml).unwrap_or(serde_json::Value::Null)
}

fn yaml_str(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_str_list(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::Array(items)) => items.iter().filter_map(yaml_str).collect(),
        Some(v) => yaml_str(v).into_iter().collect(),
        None => vec![],
    }
}

/// Derive note id / bare slug / subdir from a vault-relative path, exactly as
/// sync_graph.py does (strip prefix, strip .md, '/'→'-').
pub fn derive_ids(rel_path: &str, strip_prefix: &str) -> (String, String, String) {
    let stripped = rel_path.strip_prefix(strip_prefix).unwrap_or(rel_path);
    let no_ext = stripped.strip_suffix(".md").unwrap_or(stripped);
    let id = no_ext.replace('/', "-");
    let slug = no_ext.rsplit('/').next().unwrap_or(no_ext).to_string();
    let parts: Vec<&str> = no_ext.split('/').collect();
    let dir = if parts.len() > 1 { parts[0].to_string() } else { String::new() };
    (id, slug, dir)
}

/// Parse one note file's text into a ParsedNote according to the vault config.
pub fn parse_note(rel_path: &str, text: &str, cfg: &VaultConfig) -> ParsedNote {
    let (yaml, fm_range, body, body_offset) = split_frontmatter(text);
    let meta = yaml.map(yaml_to_json).unwrap_or(serde_json::Value::Null);
    let get = |key: &str| meta.get(key);

    let (id, slug, dir) = derive_ids(rel_path, &cfg.vault.id_strip_prefix);

    let fields = &cfg.notes.fields;
    let title = get(&fields.title)
        .and_then(yaml_str)
        .unwrap_or_else(|| id.clone());
    let kind = get(&fields.kind).and_then(yaml_str);
    let status = get(&fields.status).and_then(yaml_str);
    let created = get(&fields.created).and_then(|v| parse_date_lenient(v));
    let updated = get(&fields.updated).and_then(|v| parse_date_lenient(v));

    let mut tags = yaml_str_list(get(&cfg.tags.field));
    let mut links = scan_wikilinks(body, body_offset);
    if cfg.links.markdown_paths {
        links.extend(scan_markdown_path_links(
            body,
            body_offset,
            rel_path,
            &cfg.vault.id_strip_prefix,
        ));
    }
    if cfg.tags.inline {
        let mask = code_mask_ranges(body);
        for c in INLINE_TAG_RE.captures_iter(body) {
            let m = c.get(2).unwrap();
            if !in_ranges(m.start(), &mask) {
                let t = m.as_str().to_string();
                if !tags.contains(&t) {
                    tags.push(t);
                }
            }
        }
    }

    let edge_fields = cfg
        .frontmatter_edges()
        .flat_map(|e| {
            let field = e.field.clone().unwrap_or_default();
            yaml_str_list(get(&field))
                .into_iter()
                .map(move |value| EdgeFieldItem { field: field.clone(), value })
        })
        .collect();

    let body_text = strip_wikilinks_for_fts(body);
    let body_hash = sha256_hex(&body_text);
    let frontmatter_json = meta.to_string();

    ParsedNote {
        rel_path: rel_path.to_string(),
        id,
        slug,
        dir,
        title,
        kind,
        status,
        created,
        updated,
        tags,
        frontmatter_json,
        frontmatter_range: fm_range,
        body_text,
        body_hash,
        links,
        edge_fields,
        headings: heading_outline(body, body_offset),
    }
}

/// Parse resource metadata (the file itself when .md, else its sibling .meta.md
/// content). `body_md` is the markdown whose frontmatter describes the resource.
pub fn parse_resource(
    rel_path: &str,
    meta_text: &str,
    is_markdown: bool,
    cfg: &VaultConfig,
) -> ParsedResource {
    let res_cfg = cfg.resources.clone().unwrap_or_default();
    let (yaml, _, body, _) = split_frontmatter(meta_text);
    let meta = yaml.map(yaml_to_json).unwrap_or(serde_json::Value::Null);

    let title = meta
        .get("title")
        .and_then(yaml_str)
        .unwrap_or_else(|| {
            rel_path.rsplit('/').next().unwrap_or(rel_path).to_string()
        });
    let captured = meta
        .get(&res_cfg.date_fields.captured)
        .and_then(|v| parse_date_lenient(v));
    let source_date = meta
        .get(&res_cfg.date_fields.source)
        .and_then(|v| parse_date_lenient(v));
    // First configured field whose value looks like a URL ('source: clip'
    // style kind-tokens are skipped) — port of upsert_raw_file.
    let url = res_cfg.url_fields.iter().find_map(|f| {
        meta.get(f)
            .and_then(yaml_str)
            .filter(|s| s.starts_with("http") || s.starts_with("file:"))
    });

    let (body_text, body_hash) = if is_markdown {
        let b = body.to_string();
        let h = sha256_hex(&b);
        (b, h)
    } else {
        (String::new(), String::new())
    };

    ParsedResource {
        rel_path: rel_path.to_string(),
        title,
        captured,
        source_date,
        url,
        body_text,
        body_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VaultConfig {
        VaultConfig::default()
    }

    #[test]
    fn frontmatter_split_basic() {
        let text = "---\ntitle: Foo\n---\nbody here";
        let (yaml, range, body, off) = split_frontmatter(text);
        assert_eq!(yaml.unwrap().trim(), "title: Foo");
        assert_eq!(range.unwrap(), 0..19);
        assert_eq!(body, "body here");
        assert_eq!(off, 19);
    }

    #[test]
    fn frontmatter_absent() {
        let (yaml, range, body, off) = split_frontmatter("# Just a doc\n");
        assert!(yaml.is_none() && range.is_none());
        assert_eq!(body, "# Just a doc\n");
        assert_eq!(off, 0);
    }

    #[test]
    fn frontmatter_requires_full_line_close() {
        // `---suffix` is not a closing fence
        let text = "---\ntitle: x\n---tail\nmore\n---\nbody";
        let (yaml, _, body, _) = split_frontmatter(text);
        assert_eq!(yaml.unwrap(), "title: x\n---tail\nmore");
        assert_eq!(body, "body");
    }

    #[test]
    fn wikilink_variants() {
        let body = "See [[agentic-unit]] and [[au-contract#anchor]] and [[concepts/foo|Foo]].";
        let links = scan_wikilinks(body, 0);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].target, "agentic-unit");
        assert_eq!(links[1].anchor.as_deref(), Some("anchor"));
        assert_eq!(links[2].target, "concepts/foo");
        assert_eq!(links[2].alias.as_deref(), Some("Foo"));
        assert_eq!(&body[links[0].span.clone()], "[[agentic-unit]]");
    }

    #[test]
    fn wikilink_in_code_is_masked() {
        let body = "real [[one]]\n\n```\nfake [[two]]\n```\nand `[[three]]` inline";
        let links = scan_wikilinks(body, 0);
        let masked: Vec<bool> = links.iter().map(|l| l.masked).collect();
        assert_eq!(masked, vec![false, true, true]);
    }

    #[test]
    fn fts_strip_matches_python_semantics() {
        assert_eq!(
            strip_wikilinks_for_fts("x [[a|Alias]] y [[concepts/foo]] z [[bar#sec]]"),
            "x Alias y foo z bar"
        );
    }

    #[test]
    fn derive_ids_matches_python() {
        let (id, slug, dir) = derive_ids("wiki/concepts/agentic-unit.md", "wiki/");
        assert_eq!(id, "concepts-agentic-unit");
        assert_eq!(slug, "agentic-unit");
        assert_eq!(dir, "concepts");

        let (id, slug, dir) = derive_ids("wiki/top.md", "wiki/");
        assert_eq!(id, "top");
        assert_eq!(slug, "top");
        assert_eq!(dir, "");

        let (id, _, dir) = derive_ids("notes/a/b/c.md", "");
        assert_eq!(id, "notes-a-b-c");
        assert_eq!(dir, "notes");
    }

    #[test]
    fn parse_note_full() {
        let text = "---\ntitle: Agentic Unit\nkind: concept\nstatus: stable\nupdated: 2026-05-01\ntags: [aoa, core]\ncontradicts: [old-take]\n---\n# Heading\n\nLinks to [[au-contract]].\n";
        let mut c = cfg();
        c.edges.push(crate::config::EdgeConfig {
            name: "CONTRADICTS".into(),
            source: crate::config::EdgeSource::Frontmatter,
            field: Some("contradicts".into()),
            target: crate::config::EdgeTarget::Note,
        });
        let n = parse_note("concepts/agentic-unit.md", text, &c);
        assert_eq!(n.id, "concepts-agentic-unit");
        assert_eq!(n.title, "Agentic Unit");
        assert_eq!(n.kind.as_deref(), Some("concept"));
        assert_eq!(n.updated.unwrap().to_string(), "2026-05-01");
        assert_eq!(n.tags, vec!["aoa", "core"]);
        assert_eq!(n.links.len(), 1);
        assert_eq!(n.edge_fields.len(), 1);
        assert_eq!(n.edge_fields[0].value, "old-take");
        assert_eq!(n.headings.len(), 1);
        assert!(n.body_text.contains("Links to au-contract."));
    }

    #[test]
    fn duplicate_yaml_keys_are_last_wins() {
        // PyYAML semantics: real vault files have duplicate keys (e.g. two
        // `updated:` lines) and must still parse, last value winning.
        let text = "---\ntitle: T\nupdated: 2026-04-28\ntags: [a]\nupdated: 2026-04-30\n---\nbody";
        let n = parse_note("t.md", text, &cfg());
        assert_eq!(n.updated.unwrap().to_string(), "2026-04-30");
        assert_eq!(n.tags, vec!["a"]);
    }

    #[test]
    fn inline_tags_scanned_when_enabled() {
        let n = parse_note("a.md", "uses #rust and #graph-db but not http://x.com#frag", &cfg());
        assert!(n.tags.contains(&"rust".to_string()));
        assert!(n.tags.contains(&"graph-db".to_string()));
        assert!(!n.tags.iter().any(|t| t == "frag"));
    }

    fn md_cfg() -> VaultConfig {
        let mut c = cfg();
        c.links.markdown_paths = true;
        c
    }

    #[test]
    fn markdown_path_links_off_by_default() {
        let n = parse_note("a.md", "See [customers](/tables/customers.md).", &cfg());
        assert!(n.links.is_empty());
    }

    #[test]
    fn markdown_link_both_forms() {
        // Absolute = bundle-root-relative; relative = against the file's dir.
        let text = "See [customers](/tables/customers.md) and [peer](./other.md) and [bare](sibling.md).";
        let n = parse_note("tables/orders.md", text, &md_cfg());
        let targets: Vec<&str> = n.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["tables/customers", "tables/other", "tables/sibling"]);
        // Alias carries the link text; spans point at the destination path.
        assert_eq!(n.links[0].alias.as_deref(), Some("customers"));
        assert_eq!(&text[n.links[0].target_span.clone()], "/tables/customers.md");
    }

    #[test]
    fn markdown_link_dotdot_traversal() {
        let n = parse_note(
            "tables/orders.md",
            "See [metric](../references/metrics/event_count.md) and [escape](../../nope.md).",
            &md_cfg(),
        );
        // `..` normalizes within the bundle; escaping the root is dropped.
        assert_eq!(n.links.len(), 1);
        assert_eq!(n.links[0].target, "references/metrics/event_count");
    }

    #[test]
    fn markdown_link_in_code_is_not_extracted() {
        // Unlike wikilinks (masked but kept), in-code markdown links are
        // skipped entirely — they must never become edges.
        let text = "real [a](/a.md)\n\n```\nfake [b](/b.md)\n```\nand `[c](/c.md)` inline";
        let n = parse_note("top.md", text, &md_cfg());
        assert_eq!(n.links.len(), 1);
        assert_eq!(n.links[0].target, "a");
    }

    #[test]
    fn markdown_non_md_and_external_ignored() {
        let text = "[ext](https://example.com/x.md) [mail](mailto:a@b.c) \
                    [anchor](#section) [img alt](pic.png) ![shot](diagram.md) \
                    [dash](https://example.com/dash)";
        let n = parse_note("top.md", text, &md_cfg());
        assert!(n.links.is_empty(), "{:?}", n.links);
    }

    #[test]
    fn markdown_link_anchor_and_title() {
        let n = parse_note(
            "guides/setup.md",
            "See [schema](/tables/orders.md#schema) and [titled](./x.md \"a title\").",
            &md_cfg(),
        );
        assert_eq!(n.links[0].target, "tables/orders");
        assert_eq!(n.links[0].anchor.as_deref(), Some("schema"));
        assert_eq!(n.links[1].target, "guides/x");
    }

    #[test]
    fn markdown_link_strips_id_prefix() {
        let mut c = md_cfg();
        c.vault.id_strip_prefix = "wiki/".into();
        let n = parse_note("wiki/tables/orders.md", "[c](/wiki/tables/customers.md) [r](./peer.md)", &c);
        let targets: Vec<&str> = n.links.iter().map(|l| l.target.as_str()).collect();
        // Both land in id space (prefix stripped), like wikilink path targets.
        assert_eq!(targets, vec!["tables/customers", "tables/peer"]);
    }

    #[test]
    fn markdown_links_coexist_with_wikilinks() {
        let n = parse_note(
            "tables/orders.md",
            "Wiki [[tables/customers]] and md [sales](/datasets/sales.md).",
            &md_cfg(),
        );
        let targets: Vec<&str> = n.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["tables/customers", "datasets/sales"]);
        // Broken-by-content is a resolution concern, not a parse error: a
        // target for a file that doesn't exist still parses fine.
        assert!(n.links.iter().all(|l| !l.masked));
    }

    #[test]
    fn resource_url_field_skips_kind_tokens() {
        let c = {
            let mut c = cfg();
            c.resources = Some(Default::default());
            c
        };
        let text = "---\ntitle: A clip\nsource: clip\nurl: https://example.com/x\ncaptured_at: 2026-01-02\n---\nBody.";
        let r = parse_resource("raw/clips/2026-01-02-x.md", text, true, &c);
        assert_eq!(r.url.as_deref(), Some("https://example.com/x"));
        assert_eq!(r.captured.unwrap().to_string(), "2026-01-02");
        assert_eq!(r.body_text, "Body.");
    }
}
