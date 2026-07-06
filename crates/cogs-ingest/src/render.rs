//! Markdown rendering for everything ingest writes: the source page and the
//! `log.md` entry. Frontmatter is generated textually (we control it fully;
//! no YAML re-serialisation of existing files happens here).

use chrono::NaiveDate;

use cogs_core::note::ParsedResource;

use crate::{ContradictionFinding, Extraction};
use crate::retrieve::NearDuplicate;

/// Render the new source page per cogs' generated-page schema (Summary /
/// Key claims / Quotes / Contradictions / Cross-references — fixed sections
/// that distill mining depends on; where the page LIVES and what it's
/// stamped with comes from [ingest]). Claim texts are expected to already
/// carry their wikilinks (weave output).
pub fn source_page(
    ex: &Extraction,
    raw: &ParsedResource,
    raw_rel: &str,
    today: NaiveDate,
    cross_references: &[String],
    contradictions: &[ContradictionFinding],
    ingest: &cogs_core::config::IngestSection,
    model: &str,
) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("title: {}\n", yaml_scalar(&raw.title)));
    fm.push_str(&format!("kind: {}\n", yaml_scalar(&ingest.source_kind)));
    fm.push_str("status: draft\n");
    fm.push_str(&format!("updated: {today}\n"));
    if let Some(captured) = raw.captured {
        fm.push_str(&format!("captured_at: {captured}\n"));
    }
    if let Some(source_date) = raw.source_date {
        fm.push_str(&format!("source_date: {source_date}\n"));
    }
    fm.push_str(&format!("source_refs:\n  - {raw_rel}\n"));
    if let Some(author) = ex.author.as_deref().filter(|s| !s.trim().is_empty()) {
        fm.push_str(&format!("author: {}\n", yaml_scalar(author)));
    }
    if let Some(publisher) = ex.publisher.as_deref().filter(|s| !s.trim().is_empty()) {
        fm.push_str(&format!("publisher: {}\n", yaml_scalar(publisher)));
    }
    fm.push_str(&format!("tags: [{}]\n", ex.tags.iter().map(|t| yaml_scalar(t)).collect::<Vec<_>>().join(", ")));
    if !contradictions.is_empty() {
        let ids: Vec<String> =
            contradictions.iter().map(|c| yaml_scalar(&c.page_id)).collect();
        fm.push_str(&format!("contradicts: [{}]\n", ids.join(", ")));
    }
    if !ingest.owner.is_empty() {
        fm.push_str(&format!("owner: {}\n", yaml_scalar(&ingest.owner)));
    }
    if ingest.stamp_model {
        fm.push_str(&format!("ingested_by: {}\n", yaml_scalar(model)));
    }
    fm.push_str("---\n");

    let mut body = format!("\n# {}\n\n## Summary\n\n{}\n", raw.title, ex.summary.trim());

    if !ex.key_claims.is_empty() {
        body.push_str("\n## Key claims\n\n");
        for c in &ex.key_claims {
            body.push_str(&format!("- {}\n", c.text.trim()));
        }
    }

    if !ex.quotes.is_empty() {
        body.push_str("\n## Quotes\n\n");
        for q in &ex.quotes {
            let loc = q.location.trim();
            if loc.is_empty() {
                body.push_str(&format!("> \"{}\"\n\n", q.text.trim()));
            } else {
                body.push_str(&format!("> \"{}\" — {loc}\n\n", q.text.trim()));
            }
        }
    }

    if !contradictions.is_empty() {
        body.push_str("\n## Contradictions\n\n");
        for c in contradictions {
            body.push_str(&format!(
                "- Conflicts with [[{}]]: it says \"{}\", this source claims \"{}\". {}\n",
                c.page_id,
                c.existing_text.trim(),
                c.new_claim.trim(),
                c.explanation.trim()
            ));
        }
    }

    if !cross_references.is_empty() {
        body.push_str("\n## Cross-references\n\n");
        for id in cross_references {
            body.push_str(&format!("- [[{id}]]\n"));
        }
    }

    format!("{fm}{body}")
}

/// A stub page for a newly identified entity/concept: blurb + the claims that
/// mention it, citing the source.
pub fn new_page(
    spec: &crate::NewPageSpec,
    claims: &[String],
    source_slug: &str,
    today: NaiveDate,
    section_heading: &str,
    ingest: &cogs_core::config::IngestSection,
    model: &str,
) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("title: {}\n", yaml_scalar(&spec.title)));
    fm.push_str(&format!("kind: {}\n", yaml_scalar(&spec.kind)));
    fm.push_str("status: draft\n");
    fm.push_str(&format!("updated: {today}\n"));
    fm.push_str("tags: []\n");
    if !ingest.owner.is_empty() {
        fm.push_str(&format!("owner: {}\n", yaml_scalar(&ingest.owner)));
    }
    if ingest.stamp_model {
        fm.push_str(&format!("ingested_by: {}\n", yaml_scalar(model)));
    }
    fm.push_str("---\n");

    let mut body = format!("\n# {}\n", spec.title);
    if !spec.blurb.trim().is_empty() {
        body.push_str(&format!("\n{}\n", spec.blurb.trim()));
    }
    body.push_str(&format!("\n{section_heading}\n\n"));
    for c in claims {
        body.push_str(&format!("- {c}\n"));
    }
    body.push_str(&format!("\nSource: [[{source_slug}]]\n"));
    format!("{fm}{body}")
}

/// The section appended to an existing page: heading rendered here, body from
/// the (validated) model output.
pub fn update_section(section_heading: &str, section_md: &str) -> String {
    format!("\n{section_heading}\n\n{}\n", section_md.trim())
}

/// One appended audit-log entry in the vault's observed batch format.
#[allow(clippy::too_many_arguments)]
pub fn log_entry(
    today: NaiveDate,
    raw_title: &str,
    raw_rel: &str,
    source_dir: &str,
    source_slug: &str,
    pages_updated: &[String],
    pages_created: &[String],
    contradictions: &[ContradictionFinding],
    near_duplicates: &[NearDuplicate],
    run_id: &str,
    model: &str,
) -> String {
    let mut e = format!("\n## [{today}] ingest | {raw_title}\n");
    e.push_str(&format!("- source: {raw_rel}\n"));
    e.push_str(&format!("- source page created: {source_dir}/{source_slug}\n"));
    e.push_str(&format!(
        "- pages updated: {}\n",
        if pages_updated.is_empty() { "none".into() } else { pages_updated.join(", ") }
    ));
    e.push_str(&format!(
        "- pages created: {}\n",
        if pages_created.is_empty() { "none".into() } else { pages_created.join(", ") }
    ));
    if !contradictions.is_empty() {
        e.push_str(&format!(
            "- contradictions raised: {} ({})\n",
            contradictions.len(),
            contradictions
                .iter()
                .map(|c| format!("{} ⇄ {source_dir}/{source_slug}", c.page_id))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if !near_duplicates.is_empty() {
        e.push_str(&format!(
            "- near-duplicates flagged: {}\n",
            near_duplicates
                .iter()
                .map(|d| format!("{} ({}, {:.2})", d.id, d.via, d.score))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    e.push_str(&format!("- run: {run_id} (model {model})\n"));
    e
}

/// Quote a YAML scalar only when it needs it (matches the vault's hand-written
/// style: bare where possible, double-quoted otherwise).
fn yaml_scalar(s: &str) -> String {
    let s = s.trim();
    let bare_ok = !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || " -_./".contains(c))
        && !s.starts_with(['-', ' ', '.'])
        && !s.ends_with(' ')
        // scalars YAML would reinterpret
        && !matches!(s.to_ascii_lowercase().as_str(), "true" | "false" | "null" | "~" | "yes" | "no")
        && s.parse::<f64>().is_err();
    if bare_ok {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_scalar_quotes_only_when_needed() {
        assert_eq!(yaml_scalar("simple-slug"), "simple-slug");
        assert_eq!(yaml_scalar("A plain title 2.0 works"), "A plain title 2.0 works");
        assert_eq!(yaml_scalar("has: colon"), "\"has: colon\"");
        assert_eq!(yaml_scalar("quote \" inside"), "\"quote \\\" inside\"");
        assert_eq!(yaml_scalar("true"), "\"true\"");
        assert_eq!(yaml_scalar("3.14"), "\"3.14\"");
    }
}
