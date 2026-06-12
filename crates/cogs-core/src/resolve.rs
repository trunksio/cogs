use std::collections::{HashMap, HashSet};

/// Outcome of resolving a wikilink target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Resolved(String),
    /// Bare slug matched multiple notes and the same-dir tiebreak failed.
    Ambiguous(Vec<String>),
    Broken,
}

impl Resolution {
    pub fn id(&self) -> Option<&str> {
        match self {
            Resolution::Resolved(id) => Some(id),
            _ => None,
        }
    }
}

/// Slug index over the note set. Port of sync_graph.py's build_slug_index +
/// resolve_wikilink: targets are lowercased; path-form targets ('/' present)
/// map to full ids; bare slugs resolve when unique, or via the source note's
/// subdir tiebreak.
#[derive(Debug, Default, Clone)]
pub struct LinkResolver {
    full_ids: HashSet<String>,
    bare_to_full: HashMap<String, Vec<String>>,
}

impl LinkResolver {
    pub fn new<'a>(notes: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
        let mut full_ids = HashSet::new();
        let mut bare_to_full: HashMap<String, Vec<String>> = HashMap::new();
        for (id, slug) in notes {
            full_ids.insert(id.to_string());
            bare_to_full
                .entry(slug.to_string())
                .or_default()
                .push(id.to_string());
        }
        // Deterministic candidate order regardless of input order.
        for v in bare_to_full.values_mut() {
            v.sort();
        }
        Self { full_ids, bare_to_full }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.full_ids.contains(id)
    }

    pub fn resolve(&self, target: &str, source_dir: &str) -> Resolution {
        let raw = target.trim().to_lowercase();
        if raw.contains('/') {
            let candidate = raw.replace('/', "-");
            return if self.full_ids.contains(&candidate) {
                Resolution::Resolved(candidate)
            } else {
                Resolution::Broken
            };
        }
        let Some(candidates) = self.bare_to_full.get(&raw) else {
            return Resolution::Broken;
        };
        if candidates.len() == 1 {
            return Resolution::Resolved(candidates[0].clone());
        }
        if !source_dir.is_empty() {
            let prefix = format!("{source_dir}-");
            let same: Vec<&String> =
                candidates.iter().filter(|c| c.starts_with(&prefix)).collect();
            if same.len() == 1 {
                return Resolution::Resolved(same[0].clone());
            }
        }
        Resolution::Ambiguous(candidates.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> LinkResolver {
        LinkResolver::new(
            [
                ("concepts-agentic-unit", "agentic-unit"),
                ("concepts-registry", "registry"),
                ("entities-registry", "registry"),
                ("entities-mcp", "mcp"),
                ("top", "top"),
            ]
            .into_iter(),
        )
    }

    #[test]
    fn unique_bare_slug_resolves() {
        assert_eq!(
            resolver().resolve("agentic-unit", ""),
            Resolution::Resolved("concepts-agentic-unit".into())
        );
    }

    #[test]
    fn target_is_case_insensitive() {
        assert_eq!(
            resolver().resolve("  Agentic-Unit ", "concepts"),
            Resolution::Resolved("concepts-agentic-unit".into())
        );
    }

    #[test]
    fn path_form_resolves_to_full_id() {
        assert_eq!(
            resolver().resolve("concepts/agentic-unit", ""),
            Resolution::Resolved("concepts-agentic-unit".into())
        );
        assert_eq!(resolver().resolve("concepts/nope", ""), Resolution::Broken);
    }

    #[test]
    fn ambiguous_resolves_via_source_dir() {
        assert_eq!(
            resolver().resolve("registry", "concepts"),
            Resolution::Resolved("concepts-registry".into())
        );
        assert_eq!(
            resolver().resolve("registry", "entities"),
            Resolution::Resolved("entities-registry".into())
        );
    }

    #[test]
    fn ambiguous_without_tiebreak() {
        match resolver().resolve("registry", "") {
            Resolution::Ambiguous(c) => {
                assert_eq!(c, vec!["concepts-registry", "entities-registry"])
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
        // a dir that matches neither candidate also fails the tiebreak
        assert!(matches!(
            resolver().resolve("registry", "positions"),
            Resolution::Ambiguous(_)
        ));
    }

    #[test]
    fn unknown_slug_is_broken() {
        assert_eq!(resolver().resolve("nonexistent", ""), Resolution::Broken);
    }
}
