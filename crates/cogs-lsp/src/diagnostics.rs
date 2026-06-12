//! Diagnostics: broken/ambiguous links, unknown kinds, missing required fields.

use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use cogs_core::config::{Severity, VaultConfig};
use cogs_core::note::ParsedNote;
use cogs_core::resolve::Resolution;

use crate::pos::span_to_range;

fn severity(s: Severity) -> Option<DiagnosticSeverity> {
    match s {
        Severity::Allow => None,
        Severity::Warn => Some(DiagnosticSeverity::WARNING),
        Severity::Error => Some(DiagnosticSeverity::ERROR),
    }
}

fn top_of_file() -> Range {
    Range::new(Position::new(0, 0), Position::new(0, 3))
}

pub fn for_note(
    note: &ParsedNote,
    resolutions: &[Resolution],
    cfg: &VaultConfig,
    text: &str,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    // Link resolution diagnostics (masked links don't count).
    for (link, res) in note.links.iter().zip(resolutions) {
        if link.masked {
            continue;
        }
        match res {
            Resolution::Broken => {
                if let Some(sev) = severity(cfg.diagnostics.broken_link) {
                    out.push(Diagnostic {
                        range: span_to_range(text, &link.span),
                        severity: Some(sev),
                        source: Some("cogs".into()),
                        message: format!("unresolved link: [[{}]]", link.target),
                        ..Default::default()
                    });
                }
            }
            Resolution::Ambiguous(candidates) => {
                if let Some(sev) = severity(cfg.diagnostics.ambiguous_link) {
                    out.push(Diagnostic {
                        range: span_to_range(text, &link.span),
                        severity: Some(sev),
                        source: Some("cogs".into()),
                        message: format!(
                            "ambiguous link [[{}]] — candidates: {}",
                            link.target,
                            candidates.join(", ")
                        ),
                        ..Default::default()
                    });
                }
            }
            Resolution::Resolved(_) => {}
        }
    }

    // Unknown kind.
    if !cfg.kinds.values.is_empty() {
        if let Some(kind) = &note.kind {
            if !cfg.kinds.values.contains(kind) {
                if let Some(sev) = severity(cfg.kinds.unknown) {
                    out.push(Diagnostic {
                        range: top_of_file(),
                        severity: Some(sev),
                        source: Some("cogs".into()),
                        message: format!(
                            "unknown kind {kind:?} (expected one of: {})",
                            cfg.kinds.values.join(", ")
                        ),
                        ..Default::default()
                    });
                }
            }
        }
    }

    // Required frontmatter fields per kind.
    if let Some(kind) = &note.kind {
        if let Some(required) = cfg.diagnostics.required_fields.get(kind) {
            let fm: serde_json::Value =
                serde_json::from_str(&note.frontmatter_json).unwrap_or(serde_json::Value::Null);
            for field in required {
                let present = fm
                    .get(field)
                    .map(|v| !v.is_null() && !(v.is_array() && v.as_array().unwrap().is_empty()))
                    .unwrap_or(false);
                if !present {
                    out.push(Diagnostic {
                        range: top_of_file(),
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("cogs".into()),
                        message: format!("kind {kind:?} requires frontmatter field {field:?}"),
                        ..Default::default()
                    });
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cogs_core::parse::parse_note;
    use cogs_core::VaultIndex;

    #[test]
    fn broken_and_ambiguous_links_flagged() {
        let cfg = VaultConfig::default();
        let mut idx = VaultIndex::default();
        let text_a = "see [[missing]] and `[[masked]]`";
        idx.upsert(parse_note("a.md", text_a, &cfg));
        idx.rebuild_derived();
        let note = idx.get("a").unwrap();
        let diags = for_note(note, idx.resolutions("a"), &cfg, text_a);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("missing"));
    }

    #[test]
    fn required_fields_checked() {
        let mut cfg = VaultConfig::default();
        cfg.kinds.values = vec!["source".into()];
        cfg.diagnostics
            .required_fields
            .insert("source".into(), vec!["source_refs".into()]);
        let text = "---\nkind: source\n---\nno refs here";
        let mut idx = VaultIndex::default();
        idx.upsert(parse_note("s.md", text, &cfg));
        idx.rebuild_derived();
        let diags = for_note(idx.get("s").unwrap(), idx.resolutions("s"), &cfg, text);
        assert!(diags.iter().any(|d| d.message.contains("source_refs")));
    }
}
