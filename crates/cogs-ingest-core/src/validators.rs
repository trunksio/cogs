//! The hard validators that gate every ingest — native `cogs ingest` and
//! browser ingest (Cogitarium via cogs-wasm) run EXACTLY this code.
//!
//! Pure functions: no filesystem, graph, or LLM access. Callers supply the
//! world as plain data — existing slugs/ids as sets (native builds them from
//! disk + graph, the browser from its index), link resolution as a
//! `LinkResolver` — so the quality gate exists exactly once.

use std::collections::{BTreeMap, HashSet};

use cogs_core::parse::{scan_wikilinks, strip_wikilinks_for_fts};
use cogs_core::resolve::{LinkResolver, Resolution};

use crate::text::{find_verbatim, normalize, truncate_chars};
use crate::{Extraction, NewPageSpec};

pub const MAX_CLAIMS: usize = 12;
pub const MAX_QUOTES: usize = 6;
pub const MAX_NEW_PAGES: usize = 6;

fn slug_re() -> regex::Regex {
    regex::Regex::new(r"^[a-z0-9][a-z0-9-]{1,60}$").unwrap()
}

/// Enforce the hard rules on a stage-1 extraction. Degrades with warnings
/// wherever salvageable; the one unsalvageable case — no key claims survive —
/// is returned as-is (empty `key_claims`) for the caller to abort on.
///
/// `existing_source_slugs` drives slug-collision suffixing: the set of slugs
/// already taken in the source dir (file stems natively, index ids in the
/// browser). `fallback_slug` replaces a malformed model slug (natively
/// derived from the raw filename via [`slug_from_filename`]).
pub fn validate_extraction(
    mut ex: Extraction,
    raw_body: &str,
    existing_source_slugs: &HashSet<String>,
    fallback_slug: &str,
) -> (Extraction, Vec<String>) {
    let mut warnings = Vec::new();

    ex.summary = ex.summary.trim().to_string();

    // Claims: single-line, non-trivial, deduped, capped. The length gate
    // drops structural junk that tolerant parsing can let through
    // ("text", "quotes:[{", …) — no real claim is under 15 chars.
    let mut seen = HashSet::new();
    ex.key_claims.retain_mut(|c| {
        c.text = c.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if c.text.len() < 15 {
            if !c.text.is_empty() {
                warnings.push(format!("dropped junk claim {:?}", c.text));
            }
            return false;
        }
        seen.insert(c.text.to_lowercase())
    });
    if ex.key_claims.is_empty() {
        // Unsalvageable — the caller aborts the ingest; nothing else to fix.
        return (ex, warnings);
    }
    if ex.key_claims.len() > MAX_CLAIMS {
        warnings.push(format!(
            "model produced {} claims; keeping the first {MAX_CLAIMS}",
            ex.key_claims.len()
        ));
        ex.key_claims.truncate(MAX_CLAIMS);
    }

    // Last resort for a claims-only extraction that survived the content
    // retry: a summary built from the (validated) claims beats failing
    // the whole file. Flagged for review.
    if ex.summary.is_empty() {
        ex.summary = ex
            .key_claims
            .iter()
            .take(3)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        warnings.push(
            "model never produced a summary — synthesized one from the top claims \
             (review it)"
                .into(),
        );
    }

    // Quotes must be verbatim (whitespace-tolerant) substrings of the raw
    // body; the recovered raw slice replaces the model's rendition.
    ex.quotes = ex
        .quotes
        .drain(..)
        .filter_map(|mut q| match find_verbatim(raw_body, &q.text) {
            Some(exact) => {
                q.text = exact;
                Some(q)
            }
            None => {
                warnings.push(format!(
                    "dropped non-verbatim quote: {:?}",
                    truncate_chars(&q.text, 80)
                ));
                None
            }
        })
        .collect();
    ex.quotes.truncate(MAX_QUOTES);

    // Entities: drop empties (truncation-repair artifacts).
    ex.entities.retain(|e| !e.name.trim().is_empty());

    // Tags: lowercase tokens.
    let mut seen_tags = HashSet::new();
    ex.tags.retain_mut(|t| {
        *t = t.trim().to_lowercase().replace(' ', "-");
        !t.is_empty() && seen_tags.insert(t.clone())
    });
    ex.tags.truncate(6);

    // Slug: well-formed, non-colliding with what already exists.
    if !slug_re().is_match(&ex.suggested_slug) {
        if !ex.suggested_slug.is_empty() {
            warnings.push(format!(
                "model slug {:?} is malformed; using {:?}",
                ex.suggested_slug, fallback_slug
            ));
        }
        ex.suggested_slug = fallback_slug.to_string();
    }
    let base = ex.suggested_slug.clone();
    let mut n = 1;
    while existing_source_slugs.contains(&ex.suggested_slug) {
        n += 1;
        ex.suggested_slug = format!("{base}-{n}");
    }
    if n > 1 {
        warnings.push(format!("slug {base:?} taken; using {:?}", ex.suggested_slug));
    }

    (ex, warnings)
}

/// Validate the weave stage's proposed new pages: well-formed slug, dir drawn
/// from the configured `new_pages` map (dir → implied kind), deduped, not
/// already existing (`existing_ids` holds note ids — graph natively, index in
/// the browser), kind/title fallbacks applied, capped at [`MAX_NEW_PAGES`].
pub fn validate_new_pages(
    specs: Vec<NewPageSpec>,
    new_page_dirs: &BTreeMap<String, String>,
    kinds: &[String],
    existing_ids: &HashSet<String>,
) -> (Vec<NewPageSpec>, Vec<String>) {
    let slug_re = slug_re();
    let mut warnings = Vec::new();
    let mut new_pages = Vec::new();
    let mut seen_new: HashSet<String> = HashSet::new();
    for mut spec in specs {
        spec.slug = spec.slug.trim().to_lowercase();
        if !slug_re.is_match(&spec.slug) {
            warnings.push(format!("dropped new page with malformed slug {:?}", spec.slug));
            continue;
        }
        let Some(dir_kind) = new_page_dirs.get(&spec.dir) else {
            warnings.push(format!(
                "dropped new page {:?}: dir must be one of {:?}, got {:?}",
                spec.slug,
                new_page_dirs.keys().collect::<Vec<_>>(),
                spec.dir
            ));
            continue;
        };
        let id = format!("{}-{}", spec.dir, spec.slug);
        if !seen_new.insert(id.clone()) {
            continue; // model proposed the same page twice in one plan
        }
        if existing_ids.contains(&id) {
            warnings.push(format!("proposed new page {id} already exists — not recreating"));
            continue;
        }
        if spec.kind.is_empty() || (!kinds.is_empty() && !kinds.contains(&spec.kind)) {
            spec.kind = dir_kind.clone();
        }
        if spec.title.trim().is_empty() {
            spec.title = spec.slug.replace('-', " ");
        }
        new_pages.push(spec);
        if new_pages.len() >= MAX_NEW_PAGES {
            break;
        }
    }
    (new_pages, warnings)
}

/// Validate the weave stage's linked claims against the originals: claims may
/// gain `[[...]]` brackets but never be rewritten (a rewritten claim reverts
/// to the original), unresolvable link targets are unwrapped, and a plan with
/// the wrong claim count reverts wholesale to the plain claims.
pub fn validate_linked_claims(
    linked: Vec<String>,
    plain_claims: &[String],
    resolver: &LinkResolver,
    source_dir: &str,
) -> (Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut linked = linked;
    if linked.len() != plain_claims.len() {
        warnings.push(format!(
            "weave returned {} claims for {} inputs; keeping the originals unlinked",
            linked.len(),
            plain_claims.len()
        ));
        linked = plain_claims.to_vec();
    }
    for (i, lc) in linked.iter_mut().enumerate() {
        let cleaned = sanitize_links(lc, resolver, source_dir, &mut warnings);
        if normalize(&strip_wikilinks_for_fts(&cleaned)) != normalize(&plain_claims[i]) {
            warnings.push(format!(
                "weave rewrote claim {} — keeping the original text",
                i + 1
            ));
            *lc = plain_claims[i].clone();
        } else {
            *lc = cleaned;
        }
    }
    (linked, warnings)
}

/// Unwrap every wikilink in `text` whose target does not resolve — replaced
/// by its alias (or the target's display text) — so a hallucinated target can
/// never reach the vault as a broken link.
pub fn sanitize_links(
    text: &str,
    resolver: &LinkResolver,
    source_dir: &str,
    warnings: &mut Vec<String>,
) -> String {
    let mut out = text.to_string();
    let links = scan_wikilinks(text, 0);
    for link in links.iter().rev() {
        if matches!(resolver.resolve(&link.target, source_dir), Resolution::Resolved(_)) {
            continue;
        }
        let display = link
            .alias
            .clone()
            .unwrap_or_else(|| link.target.rsplit('/').next().unwrap_or("").to_string());
        warnings.push(format!("unwrapped unresolvable link [[{}]]", link.target));
        out.replace_range(link.span.clone(), &display);
    }
    out
}

/// Does any wikilink in `text` resolve to `id`?
pub fn links_to(text: &str, resolver: &LinkResolver, source_dir: &str, id: &str) -> bool {
    scan_wikilinks(text, 0)
        .iter()
        .any(|l| resolver.resolve(&l.target, source_dir).id() == Some(id))
}

/// Fallback slug from the raw filename: strip the date prefix and extension,
/// squash anything non-slug.
pub fn slug_from_filename(rel_path: &str) -> String {
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .trim_end_matches(".md");
    let date_re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}-").unwrap();
    let stem = date_re.replace(stem, "");
    let mut slug: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.len() < 2 {
        "capture".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Claim, Quote};

    fn extraction() -> Extraction {
        serde_json::from_str(
            r#"{
              "summary": "  A summary.  ",
              "key_claims": [
                {"text": "Anthropic announced a   registry for MCP servers.", "entities": []},
                {"text": "junk", "entities": []},
                {"text": "anthropic announced a registry for mcp servers.", "entities": []},
                {"text": "The registry verifies publisher identity before listing.", "entities": []}
              ],
              "quotes": [
                {"text": "verifies publisher   identity", "location": "para 2"},
                {"text": "this quote is entirely fabricated", "location": "para 9"}
              ],
              "entities": [{"name": "  ", "kind": "entity", "blurb": ""}, {"name": "MCP", "kind": "entity", "blurb": ""}],
              "topics": [],
              "suggested_slug": "Bad Slug!!",
              "tags": ["MCP", "Registry Stuff", "mcp"],
              "author": null,
              "publisher": null
            }"#,
        )
        .unwrap()
    }

    const RAW: &str = "Anthropic announced a registry for MCP servers.\n\nThe registry \
                       indexes community servers and \"verifies publisher identity\" \
                       before listing.\n";

    #[test]
    fn validate_extraction_enforces_every_gate() {
        let existing: HashSet<String> = ["fallback-slug".into()].into();
        let (ex, warnings) = validate_extraction(extraction(), RAW, &existing, "fallback-slug");

        assert_eq!(ex.summary, "A summary.");
        // junk dropped, duplicate deduped case-insensitively, whitespace collapsed
        let texts: Vec<&str> = ex.key_claims.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Anthropic announced a registry for MCP servers.",
                "The registry verifies publisher identity before listing."
            ]
        );
        assert!(warnings.iter().any(|w| w.contains("dropped junk claim \"junk\"")));
        // fabricated quote dropped; verbatim one recovered as the exact slice
        assert_eq!(ex.quotes.len(), 1);
        assert_eq!(ex.quotes[0].text, "verifies publisher identity");
        assert!(warnings.iter().any(|w| w.contains("non-verbatim quote")));
        // empty entity dropped
        assert_eq!(ex.entities.len(), 1);
        // tags lowercased, tokenized, deduped
        assert_eq!(ex.tags, vec!["mcp", "registry-stuff"]);
        // malformed slug → fallback, then suffixed past the collision
        assert_eq!(ex.suggested_slug, "fallback-slug-2");
        assert!(warnings.iter().any(|w| w.contains("is malformed")));
        assert!(warnings.iter().any(|w| w.contains("taken; using")));
    }

    #[test]
    fn validate_extraction_caps_claims_and_quotes() {
        let mut ex = extraction();
        ex.key_claims = (0..20)
            .map(|i| Claim { text: format!("A sufficiently long claim number {i}."), entities: vec![] })
            .collect();
        ex.quotes = (0..10)
            .map(|_| Quote { text: "verifies publisher identity".into(), location: String::new() })
            .collect();
        let (ex, warnings) = validate_extraction(ex, RAW, &HashSet::new(), "f");
        assert_eq!(ex.key_claims.len(), MAX_CLAIMS);
        assert_eq!(ex.quotes.len(), MAX_QUOTES);
        assert!(warnings.iter().any(|w| w.contains("keeping the first 12")));
    }

    #[test]
    fn validate_extraction_synthesizes_missing_summary() {
        let mut ex = extraction();
        ex.summary = "  ".into();
        let (ex, warnings) = validate_extraction(ex, RAW, &HashSet::new(), "f");
        assert!(ex.summary.starts_with("Anthropic announced a registry"));
        assert!(warnings.iter().any(|w| w.contains("never produced a summary")));
    }

    #[test]
    fn validate_extraction_returns_empty_claims_for_caller_to_abort_on() {
        let mut ex = extraction();
        ex.key_claims = vec![Claim { text: "short".into(), entities: vec![] }];
        let (ex, _) = validate_extraction(ex, RAW, &HashSet::new(), "f");
        assert!(ex.key_claims.is_empty());
    }

    fn dirs() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("concepts".to_string(), "concept".to_string()),
            ("entities".to_string(), "entity".to_string()),
        ])
    }

    fn spec(slug: &str, dir: &str, title: &str, kind: &str) -> NewPageSpec {
        NewPageSpec {
            slug: slug.into(),
            dir: dir.into(),
            title: title.into(),
            kind: kind.into(),
            blurb: String::new(),
        }
    }

    #[test]
    fn validate_new_pages_enforces_every_gate() {
        let kinds = vec!["concept".to_string(), "entity".to_string()];
        let existing: HashSet<String> = ["entities-a2a-protocol".into()].into();
        let specs = vec![
            spec("  MCP-Registry ", "entities", "MCP Registry", "entity"),
            spec("Bad Slug!", "entities", "x", "entity"),
            spec("orphan", "attic", "x", "entity"),
            spec("mcp-registry", "entities", "dup", "entity"),
            spec("a2a-protocol", "entities", "dup", "entity"),
            spec("kindless", "concepts", "", "not-a-kind"),
        ];
        let (pages, warnings) = validate_new_pages(specs, &dirs(), &kinds, &existing);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].slug, "mcp-registry");
        // dir-implied kind replaces an unknown one; empty title falls back
        assert_eq!(pages[1].kind, "concept");
        assert_eq!(pages[1].title, "kindless");
        assert!(warnings.iter().any(|w| w.contains("bad slug!")));
        assert!(warnings.iter().any(|w| w.contains("dir must be one of")));
        assert!(warnings.iter().any(|w| w.contains("already exists")));
    }

    #[test]
    fn validate_new_pages_caps_at_max() {
        let kinds = vec![];
        let specs: Vec<NewPageSpec> =
            (0..10).map(|i| spec(&format!("page-{i}"), "concepts", "t", "concept")).collect();
        let (pages, _) = validate_new_pages(specs, &dirs(), &kinds, &HashSet::new());
        assert_eq!(pages.len(), MAX_NEW_PAGES);
    }

    fn resolver() -> LinkResolver {
        LinkResolver::new(
            [("entities-mcp-registry", "mcp-registry"), ("sources-announce", "announce")]
                .into_iter(),
        )
    }

    #[test]
    fn validate_linked_claims_unwraps_reverts_and_keeps() {
        let plain = vec![
            "Anthropic announced a registry for MCP servers.".to_string(),
            "The registry verifies publisher identity.".to_string(),
        ];
        let linked = vec![
            "Anthropic announced a [[mcp-registry|registry]] for [[ghost|MCP]] servers.".to_string(),
            "A completely rewritten claim.".to_string(),
        ];
        let (out, warnings) = validate_linked_claims(linked, &plain, &resolver(), "sources");
        assert_eq!(out[0], "Anthropic announced a [[mcp-registry|registry]] for MCP servers.");
        assert_eq!(out[1], plain[1]);
        assert!(warnings.iter().any(|w| w.contains("unwrapped unresolvable link [[ghost]]")));
        assert!(warnings.iter().any(|w| w.contains("rewrote claim 2")));
    }

    #[test]
    fn validate_linked_claims_reverts_wholesale_on_count_mismatch() {
        let plain = vec!["One claim only here.".to_string(), "And a second one.".to_string()];
        let (out, warnings) =
            validate_linked_claims(vec!["Just one.".into()], &plain, &resolver(), "sources");
        assert_eq!(out, plain);
        assert!(warnings.iter().any(|w| w.contains("2 inputs")));
    }

    #[test]
    fn links_to_resolves_through_aliases() {
        let r = resolver();
        assert!(links_to("cites [[mcp-registry|the registry]]", &r, "sources", "entities-mcp-registry"));
        assert!(!links_to("cites [[ghost]]", &r, "sources", "entities-mcp-registry"));
    }

    #[test]
    fn slug_fallback_strips_date_and_ext() {
        assert_eq!(
            slug_from_filename("raw/clips/2026-07-03-Some Article!.md"),
            "some-article"
        );
        assert_eq!(slug_from_filename("raw/x.md"), "capture");
    }
}
