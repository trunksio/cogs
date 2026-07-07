//! The pure (target-independent) browser engine: a `VaultIndex` plus the
//! query surface Cogitarium needs, emitting the same JSON shapes as
//! cogs-server's /api endpoints so the graph view ports mechanically.
//!
//! Edge derivation matches cogs-graph's sync semantics exactly
//! (sync.rs::write_note_edges): frontmatter edges targeting resources use the
//! raw path verbatim; note targets resolve through the LinkResolver with the
//! source note's dir as tiebreak; the wikilink edge dedupes on raw target
//! text and skips self-links; masked (in-code) links are included.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use cogs_core::config::{EdgeTarget, Vault, VaultConfig};
use cogs_core::index::VaultIndex;
use cogs_core::note::{ParsedNote, ParsedResource};
use cogs_core::parse::{parse_note, parse_resource};
use cogs_core::resolve::Resolution;
use cogs_core::scan::VaultScanner;

pub struct Engine {
    config: VaultConfig,
    scanner: VaultScanner,
    index: VaultIndex,
    /// Raw markdown per note id (the /api/notes/:id `markdown` field — native
    /// reads it from disk; the browser hands it to us at upsert time).
    raw: BTreeMap<String, String>,
    resources: BTreeMap<String, ParsedResource>,
}

impl Engine {
    pub fn new(config_toml: &str) -> Result<Self> {
        let config: VaultConfig = if config_toml.trim().is_empty() {
            VaultConfig::default()
        } else {
            toml::from_str(config_toml).context("parsing cogs.toml")?
        };
        config.validate()?;
        // VaultScanner only reads globsets from the config; the root path is
        // never touched unless walk() is called (native-only).
        let vault = Vault::from_config(std::path::PathBuf::new(), config.clone())?;
        let scanner = VaultScanner::new(&vault)?;
        Ok(Self {
            config,
            scanner,
            index: VaultIndex::default(),
            raw: BTreeMap::new(),
            resources: BTreeMap::new(),
        })
    }

    pub fn is_note(&self, rel_path: &str) -> bool {
        self.scanner.is_note(rel_path)
    }

    pub fn is_resource(&self, rel_path: &str) -> bool {
        self.scanner.is_resource(rel_path)
    }

    pub fn upsert(&mut self, rel_path: &str, content: &str) -> String {
        let note = parse_note(rel_path, content, &self.config);
        let id = note.id.clone();
        self.raw.insert(id.clone(), content.to_string());
        self.index.upsert(note);
        id
    }

    pub fn upsert_resource(&mut self, rel_path: &str, meta_text: &str, is_markdown: bool) {
        let r = parse_resource(rel_path, meta_text, is_markdown, &self.config);
        self.resources.insert(rel_path.to_string(), r);
    }

    pub fn remove_by_path(&mut self, rel_path: &str) {
        if let Some(n) = self.index.remove_by_path(rel_path) {
            self.raw.remove(&n.id);
        }
        self.resources.remove(rel_path);
    }

    pub fn rebuild_derived(&mut self) {
        self.index.rebuild_derived();
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// = /api/meta (cogs-server lib.rs:83), minus the filesystem root.
    pub fn meta(&self) -> Value {
        json!({
            "kinds": self.config.kinds.values,
            "edges": self.edge_names(),
            "has_resources": self.config.resources.is_some(),
            "stale_after_days": self.config.diagnostics.stale_after_days,
        })
    }

    fn edge_names(&self) -> Vec<String> {
        self.config.edges.iter().map(|e| e.name.clone()).collect()
    }

    /// All derived (source_id, target, edge_name, to_resource) tuples —
    /// sync.rs::write_note_edges semantics.
    fn derive_edges(&self) -> Vec<(String, String, String, bool)> {
        let mut out = Vec::new();
        let resolver = self.index.resolver();
        for note in self.index.notes() {
            for e in self.config.frontmatter_edges() {
                let field = e.field.as_deref().unwrap_or_default();
                for item in note.edge_fields.iter().filter(|i| i.field == field) {
                    match e.target {
                        EdgeTarget::Resource => {
                            out.push((note.id.clone(), item.value.clone(), e.name.clone(), true));
                        }
                        EdgeTarget::Note => {
                            if let Resolution::Resolved(t) = resolver.resolve(&item.value, &note.dir)
                            {
                                out.push((note.id.clone(), t, e.name.clone(), false));
                            }
                        }
                    }
                }
            }
            if let Some(e) = self.config.wikilink_edge() {
                let mut seen: HashSet<&str> = HashSet::new();
                for link in &note.links {
                    if !seen.insert(link.target.as_str()) {
                        continue;
                    }
                    let Resolution::Resolved(t) = resolver.resolve(&link.target, &note.dir) else {
                        continue;
                    };
                    if t == note.id {
                        continue;
                    }
                    out.push((note.id.clone(), t, e.name.clone(), false));
                }
            }
        }
        out
    }

    fn node_json(n: &ParsedNote) -> Value {
        json!({
            "id": n.id,
            "label": n.title,
            "kind": n.kind,
            "status": n.status,
            "updated": n.updated.map(|d| d.to_string()),
            "tags": n.tags,
            "dir": n.dir,
            "path": n.rel_path,
        })
    }

    /// = /api/graph (cogs-server lib.rs:101). Notes sorted by id and edges in
    /// derivation order keyed (source, type, target) for deterministic output.
    pub fn graph_snapshot(&self, include_resources: bool) -> Value {
        let include_resources = include_resources && self.config.resources.is_some();
        let mut notes: Vec<&ParsedNote> = self.index.notes().collect();
        notes.sort_by(|a, b| a.id.cmp(&b.id));
        let nodes: Vec<Value> = notes.iter().map(|n| Self::node_json(n)).collect();

        let mut edges: Vec<Value> = self
            .derive_edges()
            .into_iter()
            .filter(|(_, _, _, to_res)| !to_res || include_resources)
            .map(|(source, target, ty, _)| json!({"source": source, "target": target, "type": ty}))
            .collect();
        edges.sort_by(|a, b| a.to_string().cmp(&b.to_string()));

        let resources: Vec<Value> = if include_resources {
            self.resources
                .values()
                .map(|r| {
                    json!({
                        "id": r.rel_path,
                        "label": r.title,
                        "captured": r.captured.map(|d| d.to_string()),
                        "source_date": r.source_date.map(|d| d.to_string()),
                    })
                })
                .collect()
        } else {
            vec![]
        };

        json!({ "nodes": nodes, "edges": edges, "resources": resources })
    }

    /// = /api/notes/:id (cogs-server lib.rs:151): note fields + raw markdown +
    /// typed outlinks/backlinks (capped at 100, like native).
    pub fn note(&self, id: &str) -> Option<Value> {
        let n = self.index.get(id)?;
        let mut outlinks = Vec::new();
        let mut backlinks = Vec::new();
        for (source, target, ty, to_res) in self.derive_edges() {
            if source == id && outlinks.len() < 100 {
                let title = if to_res {
                    self.resources.get(&target).map(|r| r.title.clone())
                } else {
                    self.index.get(&target).map(|t| t.title.clone())
                };
                outlinks.push(json!({"type": ty, "id": target, "title": title}));
            }
            if !to_res && target == id && source != id && backlinks.len() < 100 {
                let title = self.index.get(&source).map(|s| s.title.clone());
                backlinks.push(json!({"type": ty, "id": source, "title": title}));
            }
        }
        Some(json!({
            "id": n.id,
            "path": n.rel_path,
            "title": n.title,
            "kind": n.kind,
            "status": n.status,
            "updated": n.updated.map(|d| d.to_string()),
            "tags": n.tags,
            "frontmatter": n.frontmatter_json,
            "markdown": self.raw.get(id),
            "outlinks": outlinks,
            "backlinks": backlinks,
        }))
    }

    /// Undirected n-hop expansion over derived note↔note edges — the ask
    /// pipeline's coverage step (cogs-ask lib.rs::expand semantics).
    pub fn expand(&self, seed_ids: &[String], hops: usize) -> Vec<String> {
        let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let edges = self.derive_edges();
        for (s, t, _, to_res) in &edges {
            if *to_res {
                continue;
            }
            adj.entry(s.as_str()).or_default().push(t.as_str());
            adj.entry(t.as_str()).or_default().push(s.as_str());
        }
        let mut seen: HashSet<String> = seed_ids.iter().cloned().collect();
        let mut frontier: Vec<String> = seed_ids.to_vec();
        let mut out = Vec::new();
        for _ in 0..hops {
            let mut next = Vec::new();
            for id in &frontier {
                for nb in adj.get(id.as_str()).into_iter().flatten() {
                    if seen.insert(nb.to_string()) {
                        out.push(nb.to_string());
                        next.push(nb.to_string());
                    }
                }
            }
            frontier = next;
        }
        out
    }

    /// One search document per note — the exact fields native FTS indexes
    /// (title + wikilink-stripped body_text, cogs-graph schema.rs), plus id
    /// and tags for the browser index. Sorted by id.
    pub fn search_docs(&self) -> Value {
        let mut notes: Vec<&ParsedNote> = self.index.notes().collect();
        notes.sort_by(|a, b| a.id.cmp(&b.id));
        Value::Array(
            notes
                .iter()
                .map(|n| {
                    json!({
                        "id": n.id,
                        "title": n.title,
                        "body": n.body_text,
                        "tags": n.tags,
                    })
                })
                .collect(),
        )
    }

    /// Orphans, stale notes (vs `today`, ISO date), and contradiction pairs —
    /// the /api/health surface the viz health overlay uses.
    pub fn health(&self, today_iso: &str) -> Value {
        let orphans: Vec<Value> = {
            let mut o: Vec<&ParsedNote> = self.index.orphans();
            o.sort_by(|a, b| a.id.cmp(&b.id));
            o.iter().map(|n| json!({"id": n.id, "title": n.title})).collect()
        };
        let stale: Vec<Value> = match (
            self.config.diagnostics.stale_after_days,
            chrono::NaiveDate::parse_from_str(today_iso, "%Y-%m-%d"),
        ) {
            (Some(days), Ok(today)) => {
                let cutoff = today - chrono::Duration::days(days as i64);
                let mut s: Vec<&ParsedNote> = self
                    .index
                    .notes()
                    .filter(|n| n.updated.map(|u| u < cutoff).unwrap_or(false))
                    .collect();
                s.sort_by(|a, b| a.id.cmp(&b.id));
                s.iter()
                    .map(|n| json!({"id": n.id, "updated": n.updated.map(|d| d.to_string())}))
                    .collect()
            }
            _ => vec![],
        };
        let mut contradictions: Vec<Value> = self
            .derive_edges()
            .into_iter()
            .filter(|(_, _, ty, to_res)| ty == "CONTRADICTS" && !to_res)
            .map(|(s, t, _, _)| json!({"source": s, "target": t}))
            .collect();
        contradictions.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        json!({ "orphans": orphans, "stale": stale, "contradictions": contradictions })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[vault]
notes = ["wiki/**/*.md"]
exclude = ["wiki/index.md"]
id_strip_prefix = "wiki/"
[resources]
paths = ["raw/**/*"]
[[edges]]
name = "CITES"
source = "wikilinks"
[[edges]]
name = "SOURCE_OF"
source = "frontmatter"
field = "source_refs"
target = "resource"
[tags]
inline = false
"#;

    fn engine() -> Engine {
        let mut e = Engine::new(CFG).unwrap();
        e.upsert(
            "wiki/concepts/a.md",
            "---\ntitle: A\nkind: concept\nsource_refs:\n  - raw/x.md\n---\nSee [[b]] and [[b]] again and [[a]].\n",
        );
        e.upsert("wiki/concepts/b.md", "---\ntitle: B\nkind: concept\n---\nBack to [[a]].\n");
        e.upsert_resource("raw/x.md", "---\ntitle: X\ncaptured_at: 2026-01-01\n---\nraw body", true);
        e.rebuild_derived();
        e
    }

    #[test]
    fn snapshot_shape_and_edge_semantics() {
        let e = engine();
        let g = e.graph_snapshot(false);
        assert_eq!(g["nodes"].as_array().unwrap().len(), 2);
        // wikilinks deduped (two [[b]] → one edge), self-link [[a]] skipped,
        // resource edge excluded without include_resources
        let edges = g["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 2, "{edges:?}");
        assert!(edges.iter().all(|e| e["type"] == "CITES"));

        let g = e.graph_snapshot(true);
        assert_eq!(g["edges"].as_array().unwrap().len(), 3);
        assert_eq!(g["resources"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn note_detail_includes_markdown_and_links() {
        let e = engine();
        let n = e.note("concepts-a").unwrap();
        assert_eq!(n["title"], "A");
        assert!(n["markdown"].as_str().unwrap().contains("See [[b]]"));
        let out = n["outlinks"].as_array().unwrap();
        assert_eq!(out.len(), 2); // SOURCE_OF raw/x.md + CITES b
        assert_eq!(n["backlinks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn markdown_path_link_edges_appear_in_snapshot_when_enabled() {
        // Google OKF-style config: [links] markdown_paths feeds the same
        // wikilink-source edge, so the browser engine's graph_snapshot must
        // show markdown-link edges without any wasm-specific code.
        const OKF_CFG: &str = r#"
[notes.fields]
kind = "type"
updated = "timestamp"
[links]
markdown_paths = true
[tags]
inline = false
"#;
        let mut e = Engine::new(OKF_CFG).unwrap();
        e.upsert(
            "tables/orders.md",
            "---\ntype: BigQuery Table\ntitle: Orders\n---\nJoins [customers](/tables/customers.md); \
             see the [sales dataset](../datasets/sales.md) and [[datasets/sales]] again.\n\
             Broken: [churn](/models/churn.md). In code: `[x](/tables/customers.md)`.\n",
        );
        e.upsert(
            "tables/customers.md",
            "---\ntype: BigQuery Table\ntitle: Customers\n---\nOne row per customer.\n",
        );
        e.upsert(
            "datasets/sales.md",
            "---\ntype: BigQuery Dataset\ntitle: Sales\n---\nContains [orders](./../tables/orders.md).\n",
        );
        e.rebuild_derived();

        let g = e.graph_snapshot(false);
        let edges: Vec<(String, String)> = g["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (e["source"].as_str().unwrap().to_string(), e["target"].as_str().unwrap().to_string())
            })
            .collect();
        // orders → customers (absolute), orders → sales (relative ../,
        // deduped with the equivalent wikilink), sales → orders (relative);
        // broken + in-code links produce nothing.
        assert_eq!(
            edges,
            vec![
                ("datasets-sales".into(), "tables-orders".into()),
                ("tables-orders".into(), "datasets-sales".into()),
                ("tables-orders".into(), "tables-customers".into()),
            ],
            "{edges:?}"
        );
        assert!(g["edges"].as_array().unwrap().iter().all(|e| e["type"] == "LINKS_TO"));
    }

    #[test]
    fn expand_and_health() {
        let e = engine();
        let ex = e.expand(&["concepts-a".into()], 1);
        assert_eq!(ex, vec!["concepts-b".to_string()]);
        let h = e.health("2026-07-06");
        assert!(h["orphans"].as_array().unwrap().is_empty());
    }
}
