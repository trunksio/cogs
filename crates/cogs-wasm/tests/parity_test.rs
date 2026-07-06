//! The anti-drift mechanism: the same fixture vault goes through (a) the
//! native pipeline — cogs-graph SyncEngine into LadybugDB, queried with the
//! exact Cypher /api/graph runs — and (b) the browser engine — Engine
//! upserts + graph_snapshot — and the resulting node/edge sets must be
//! identical.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine};
use cogs_wasm::engine::Engine;

const CONFIG: &str = r#"
[vault]
notes = ["wiki/**/*.md"]
exclude = ["wiki/index.md", "wiki/_lint/**"]
id_strip_prefix = "wiki/"

[resources]
paths = ["raw/**/*"]
exclude = ["raw/README.md"]

[kinds]
values = ["concept", "entity", "source"]

[[edges]]
name = "CITES"
source = "wikilinks"

[[edges]]
name = "SOURCE_OF"
source = "frontmatter"
field = "source_refs"
target = "resource"

[[edges]]
name = "CONTRADICTS"
source = "frontmatter"
field = "contradicts"

[tags]
inline = false
"#;

// Mirrors cogs-graph/tests/sync_test.rs::aoa_mini — ambiguous slugs,
// path-form links, self-link, resource refs, excluded files.
const FILES: &[(&str, &str)] = &[
    (
        "wiki/concepts/agentic-unit.md",
        "---\ntitle: Agentic Unit\nkind: concept\nstatus: stable\nupdated: 2026-05-01\ntags: [aoa]\nsource_refs:\n  - raw/clips/2026-01-01-au.md\n---\nThe core idea. See [[au-contract]] and [[registry]] and [[agentic-unit]].\n",
    ),
    (
        "wiki/concepts/au-contract.md",
        "---\ntitle: AU Contract\nkind: concept\ntags: [aoa]\n---\nRelates to [[agentic-unit]] and [[agentic-unit]] twice.\n",
    ),
    (
        "wiki/concepts/registry.md",
        "---\ntitle: Registry (concept)\nkind: concept\n---\nContrast [[entities/registry|the product]].\n",
    ),
    (
        "wiki/entities/registry.md",
        "---\ntitle: Registry (entity)\nkind: entity\ncontradicts: [concepts/registry]\n---\nLinks [[agentic-unit]].\n",
    ),
    ("wiki/index.md", "# Index\nExcluded — [[agentic-unit]] must not create edges.\n"),
    (
        "raw/clips/2026-01-01-au.md",
        "---\ntitle: AU article\ncaptured_at: 2026-01-01\nsource_date: 2025-12-30\nurl: https://example.com/au\n---\nOriginal article text.\n",
    ),
    ("raw/README.md", "excluded"),
];

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

type EdgeSet = BTreeSet<(String, String, String)>;

fn native_graph(root: &Path) -> (BTreeSet<String>, EdgeSet) {
    let vault = Vault::discover(root).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();

    let nodes: BTreeSet<String> = db
        .query_json("MATCH (n:Note) RETURN n.id AS id")
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();

    let mut edges: EdgeSet = BTreeSet::new();
    for name in ["CITES", "SOURCE_OF", "CONTRADICTS"] {
        let dst = if name == "SOURCE_OF" { "b.path" } else { "b.id" };
        for row in db
            .query_json(&format!(
                "MATCH (a:Note)-[r:{name}]->(b) RETURN a.id AS s, {dst} AS t"
            ))
            .unwrap()
        {
            edges.insert((
                row["s"].as_str().unwrap().into(),
                row["t"].as_str().unwrap().into(),
                name.into(),
            ));
        }
    }
    (nodes, edges)
}

fn wasm_graph() -> (BTreeSet<String>, EdgeSet) {
    let mut e = Engine::new(CONFIG).unwrap();
    for (rel, content) in FILES {
        if e.is_note(rel) {
            e.upsert(rel, content);
        } else if e.is_resource(rel) {
            e.upsert_resource(rel, content, true);
        }
    }
    e.rebuild_derived();
    let g = e.graph_snapshot(true);
    let nodes = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect();
    let edges = g["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
                e["type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    (nodes, edges)
}

#[test]
fn browser_engine_matches_native_graph() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "cogs.toml", CONFIG);
    for (rel, content) in FILES {
        write(tmp.path(), rel, content);
    }

    let (native_nodes, native_edges) = native_graph(tmp.path());
    let (wasm_nodes, wasm_edges) = wasm_graph();

    assert_eq!(native_nodes, wasm_nodes, "node sets diverge");
    assert_eq!(native_edges, wasm_edges, "edge sets diverge");
    // and the fixture actually exercises the tricky paths:
    assert!(native_edges.iter().any(|(_, _, t)| t == "CONTRADICTS"));
    assert!(native_edges.iter().any(|(_, t, ty)| ty == "SOURCE_OF" && t.starts_with("raw/")));
    assert_eq!(native_edges.iter().filter(|(_, _, t)| t == "CITES").count(), 5);
}
