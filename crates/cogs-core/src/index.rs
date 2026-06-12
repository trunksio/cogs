use std::collections::HashMap;

use crate::note::{LinkSpan, ParsedNote};
use crate::resolve::{LinkResolver, Resolution};

/// In-memory index over the parsed vault. This is the source of truth for
/// latency-critical LSP features (completion, references, diagnostics); the
/// graph DB mirrors it for queries the editor doesn't need synchronously.
///
/// Mutation pattern: upsert/remove notes, then call `rebuild_derived()` once
/// per batch — link resolution is global (a new note can fix or break links
/// anywhere), so derived state is recomputed wholesale. Cheap at 10k notes.
#[derive(Debug, Default)]
pub struct VaultIndex {
    notes: HashMap<String, ParsedNote>,
    path_to_id: HashMap<String, String>,
    resolver: LinkResolver,
    /// target note id -> spans of wikilinks pointing at it.
    backlinks: HashMap<String, Vec<LinkSpan>>,
    /// note id -> resolution per body link (same order as `links`).
    resolutions: HashMap<String, Vec<Resolution>>,
}

impl VaultIndex {
    pub fn upsert(&mut self, note: ParsedNote) {
        self.path_to_id.insert(note.rel_path.clone(), note.id.clone());
        self.notes.insert(note.id.clone(), note);
    }

    pub fn remove_by_path(&mut self, rel_path: &str) -> Option<ParsedNote> {
        let id = self.path_to_id.remove(rel_path)?;
        self.notes.remove(&id)
    }

    pub fn rebuild_derived(&mut self) {
        self.resolver = LinkResolver::new(
            self.notes.values().map(|n| (n.id.as_str(), n.slug.as_str())),
        );
        let mut backlinks: HashMap<String, Vec<LinkSpan>> = HashMap::new();
        let mut resolutions: HashMap<String, Vec<Resolution>> = HashMap::new();
        for note in self.notes.values() {
            let mut res = Vec::with_capacity(note.links.len());
            for link in &note.links {
                let r = self.resolver.resolve(&link.target, &note.dir);
                if let Resolution::Resolved(target_id) = &r {
                    if target_id != &note.id {
                        backlinks.entry(target_id.clone()).or_default().push(LinkSpan {
                            note_id: note.id.clone(),
                            span: link.span.clone(),
                            target_span: link.target_span.clone(),
                        });
                    }
                }
                res.push(r);
            }
            resolutions.insert(note.id.clone(), res);
        }
        self.backlinks = backlinks;
        self.resolutions = resolutions;
    }

    pub fn get(&self, id: &str) -> Option<&ParsedNote> {
        self.notes.get(id)
    }

    pub fn get_by_path(&self, rel_path: &str) -> Option<&ParsedNote> {
        self.path_to_id.get(rel_path).and_then(|id| self.notes.get(id))
    }

    pub fn notes(&self) -> impl Iterator<Item = &ParsedNote> {
        self.notes.values()
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    pub fn resolver(&self) -> &LinkResolver {
        &self.resolver
    }

    pub fn backlinks(&self, id: &str) -> &[LinkSpan] {
        self.backlinks.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Per-link resolutions for a note, aligned with `note.links` order.
    pub fn resolutions(&self, id: &str) -> &[Resolution] {
        self.resolutions.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Notes with zero inbound and zero resolved outbound links.
    pub fn orphans(&self) -> Vec<&ParsedNote> {
        self.notes
            .values()
            .filter(|n| {
                self.backlinks(&n.id).is_empty()
                    && !self
                        .resolutions(&n.id)
                        .iter()
                        .any(|r| matches!(r, Resolution::Resolved(t) if t != &n.id))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VaultConfig;
    use crate::parse::parse_note;

    fn index_from(files: &[(&str, &str)]) -> VaultIndex {
        let cfg = VaultConfig::default();
        let mut idx = VaultIndex::default();
        for (path, text) in files {
            idx.upsert(parse_note(path, text, &cfg));
        }
        idx.rebuild_derived();
        idx
    }

    #[test]
    fn backlinks_and_resolutions() {
        let idx = index_from(&[
            ("a.md", "links to [[b]] and [[missing]]"),
            ("b.md", "links back to [[a]]"),
        ]);
        assert_eq!(idx.backlinks("b").len(), 1);
        assert_eq!(idx.backlinks("b")[0].note_id, "a");
        assert_eq!(idx.backlinks("a").len(), 1);
        let res = idx.resolutions("a");
        assert_eq!(res[0], Resolution::Resolved("b".into()));
        assert_eq!(res[1], Resolution::Broken);
    }

    #[test]
    fn removing_a_note_breaks_links_after_rebuild() {
        let mut idx = index_from(&[("a.md", "see [[b]]"), ("b.md", "content")]);
        idx.remove_by_path("b.md");
        idx.rebuild_derived();
        assert_eq!(idx.resolutions("a")[0], Resolution::Broken);
        assert!(idx.get("b").is_none());
    }

    #[test]
    fn self_links_do_not_count_as_backlinks() {
        let idx = index_from(&[("a.md", "self ref [[a]]")]);
        assert!(idx.backlinks("a").is_empty());
    }

    #[test]
    fn orphan_detection() {
        let idx = index_from(&[
            ("a.md", "see [[b]]"),
            ("b.md", "linked"),
            ("c.md", "no links at all"),
        ]);
        let orphans: Vec<&str> = idx.orphans().iter().map(|n| n.id.as_str()).collect();
        assert_eq!(orphans, vec!["c"]);
    }
}
