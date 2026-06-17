use std::fs;
use std::path::Path;

use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine, SyncMode};
use lbug::Value;

const AOA_MINI_CONFIG: &str = r#"
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

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn aoa_mini(root: &Path) {
    write(root, "cogs.toml", AOA_MINI_CONFIG);
    write(
        root,
        "wiki/concepts/agentic-unit.md",
        "---\ntitle: Agentic Unit\nkind: concept\nstatus: stable\nupdated: 2026-05-01\ntags: [aoa]\nsource_refs:\n  - raw/clips/2026-01-01-au.md\n---\nThe core idea. See [[au-contract]] and [[registry]].\n",
    );
    write(
        root,
        "wiki/concepts/au-contract.md",
        "---\ntitle: AU Contract\nkind: concept\ntags: [aoa]\n---\nRelates to [[agentic-unit]].\n",
    );
    write(
        root,
        "wiki/concepts/registry.md",
        "---\ntitle: Registry (concept)\nkind: concept\n---\nConcept-side registry. Contrast [[entities/registry|the product]].\n",
    );
    write(
        root,
        "wiki/entities/registry.md",
        "---\ntitle: Registry (entity)\nkind: entity\ncontradicts: [concepts/registry]\n---\nProduct registry page linking [[agentic-unit]].\n",
    );
    write(
        root,
        "wiki/index.md",
        "# Index\nExcluded — [[agentic-unit]] link here must not create edges.\n",
    );
    write(
        root,
        "raw/clips/2026-01-01-au.md",
        "---\ntitle: AU article\ncaptured_at: 2026-01-01\nsource_date: 2025-12-30\nurl: https://example.com/au\n---\nOriginal article text.\n",
    );
    write(root, "raw/files/paper.pdf", "%PDF-fake");
    write(
        root,
        "raw/files/paper.meta.md",
        "---\ntitle: A paper\ncaptured_at: 2026-02-02\n---\nAbout the paper.\n",
    );
    write(root, "raw/README.md", "excluded");
}

fn q1(db: &GraphDb, cypher: &str) -> i64 {
    let conn = db.conn().unwrap();
    let row = conn.query(cypher).unwrap().next().unwrap();
    match row.into_iter().next().unwrap() {
        Value::Int64(n) => n,
        Value::UInt64(n) => n as i64,
        other => panic!("expected count, got {other:?}"),
    }
}

#[test]
fn full_sync_builds_expected_graph() {
    let tmp = tempfile::tempdir().unwrap();
    aoa_mini(tmp.path());
    let vault = Vault::discover(tmp.path()).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    let engine = SyncEngine::new(&vault).unwrap();
    let out = engine.sync(&db, false).unwrap();

    assert_eq!(out.mode, SyncMode::Full);
    assert_eq!(out.notes_synced, 4);
    assert_eq!(out.resources_synced, 2);
    assert_eq!(q1(&db, "MATCH (n:Note) RETURN count(n)"), 4);
    assert_eq!(q1(&db, "MATCH (r:Resource) RETURN count(r)"), 2);
    // CITES: au->au-contract, au->registry(ambiguous, same-dir tiebreak->concepts),
    // au-contract->au, concepts/registry->entities-registry (path form),
    // entities/registry->au = 5
    assert_eq!(q1(&db, "MATCH (:Note)-[r:CITES]->(:Note) RETURN count(r)"), 5);
    assert_eq!(q1(&db, "MATCH (:Note)-[r:SOURCE_OF]->(:Resource) RETURN count(r)"), 1);
    assert_eq!(q1(&db, "MATCH (:Note)-[r:CONTRADICTS]->(:Note) RETURN count(r)"), 1);
    assert_eq!(q1(&db, "MATCH (:Note)-[r:TAGGED]->(:Tag) RETURN count(r)"), 2);
    assert_eq!(q1(&db, "MATCH (t:Tag) RETURN count(t)"), 1);
    // ambiguous tiebreak resolved to the same-dir concept page
    assert_eq!(
        q1(
            &db,
            "MATCH (:Note {id: 'concepts-agentic-unit'})-[:CITES]->(b:Note {id: 'concepts-registry'}) RETURN count(b)"
        ),
        1
    );
}

#[test]
fn incremental_sync_handles_edit_add_delete() {
    let tmp = tempfile::tempdir().unwrap();
    aoa_mini(tmp.path());
    let vault = Vault::discover(tmp.path()).unwrap();

    {
        let db = GraphDb::open_rw(&vault, false).unwrap();
        SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
    }

    // No changes -> incremental no-op.
    {
        let db = GraphDb::open_rw(&vault, false).unwrap();
        let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
        assert_eq!(out.mode, SyncMode::Incremental);
        assert_eq!(out.notes_synced, 0);
        assert_eq!(out.notes_relinked, 0);
        assert_eq!(out.deleted, 0);
    }

    // Edit one note: add a broken link.
    write(
        tmp.path(),
        "wiki/concepts/au-contract.md",
        "---\ntitle: AU Contract\nkind: concept\ntags: [aoa]\n---\nRelates to [[agentic-unit]] and [[future-note]].\n",
    );
    {
        let db = GraphDb::open_rw(&vault, false).unwrap();
        let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
        assert_eq!(out.notes_synced, 1);
        assert_eq!(q1(&db, "MATCH (:Note)-[r:CITES]->() RETURN count(r)"), 5);
    }

    // Add the missing note: au-contract must be re-linked without being edited.
    write(
        tmp.path(),
        "wiki/concepts/future-note.md",
        "---\ntitle: Future\nkind: concept\n---\nNow exists.\n",
    );
    {
        let db = GraphDb::open_rw(&vault, false).unwrap();
        let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
        assert_eq!(out.notes_synced, 1); // future-note
        assert_eq!(out.notes_relinked, 1); // au-contract picked up the fixed link
        assert_eq!(q1(&db, "MATCH (:Note)-[r:CITES]->() RETURN count(r)"), 6);
        assert_eq!(q1(&db, "MATCH (n:Note) RETURN count(n)"), 5);
    }

    // Delete it again: node gone, edge gone, au-contract relinked back.
    fs::remove_file(tmp.path().join("wiki/concepts/future-note.md")).unwrap();
    {
        let db = GraphDb::open_rw(&vault, false).unwrap();
        let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
        assert_eq!(out.deleted, 1);
        assert_eq!(q1(&db, "MATCH (n:Note) RETURN count(n)"), 4);
        assert_eq!(q1(&db, "MATCH (:Note)-[r:CITES]->() RETURN count(r)"), 5);
    }
}

#[test]
fn config_change_triggers_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    aoa_mini(tmp.path());
    let vault = Vault::discover(tmp.path()).unwrap();
    {
        let db = GraphDb::open_rw(&vault, false).unwrap();
        SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
    }

    // Change the config (rename the wikilink edge) -> hash mismatch -> rebuild.
    let new_config = AOA_MINI_CONFIG.replace("name = \"CITES\"", "name = \"REFERS_TO\"");
    write(tmp.path(), "cogs.toml", &new_config);
    let vault2 = Vault::discover(tmp.path()).unwrap();
    assert_ne!(vault.config_hash, vault2.config_hash);

    let db = GraphDb::open_rw(&vault2, false).unwrap();
    assert!(db.rebuilt);
    let out = SyncEngine::new(&vault2).unwrap().sync(&db, false).unwrap();
    assert_eq!(out.mode, SyncMode::Full);
    assert_eq!(q1(&db, "MATCH (:Note)-[r:REFERS_TO]->() RETURN count(r)"), 5);
}

#[test]
fn zero_config_vault_works() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "a.md", "Links to [[b]] with #atag inline.\n");
    write(tmp.path(), "b.md", "---\ntags: [yaml-tag]\n---\nBack to [[a]].\n");
    let vault = Vault::discover(tmp.path()).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
    assert_eq!(out.notes_synced, 2);
    assert_eq!(q1(&db, "MATCH (:Note)-[r:LINKS_TO]->() RETURN count(r)"), 2);
    assert_eq!(q1(&db, "MATCH (t:Tag) RETURN count(t)"), 2);
}

const OKF_CONFIG: &str = r#"
[notes.fields]
kind = "type"
description = "description"
resource = "resource"
timestamp = "timestamp"

[kinds]
unknown = "allow"

[[edges]]
name = "LINKS_TO"
source = "markdown_links"

[tags]
inline = false
"#;

#[test]
fn okf_markdown_links_build_graph() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "cogs.toml", OKF_CONFIG);
    write(
        tmp.path(),
        "concepts/agentic-unit.md",
        "---\ntype: concept\ndescription: The core unit\ntimestamp: 2026-06-01\n---\nSee [contract](au-contract.md) and [registry](../entities/registry.md).\n",
    );
    write(
        tmp.path(),
        "concepts/au-contract.md",
        "---\ntype: concept\n---\nRelates to [the unit](agentic-unit.md).\n",
    );
    write(
        tmp.path(),
        "entities/registry.md",
        "---\ntype: entity\n---\nProduct page, links to [unit](../concepts/agentic-unit.md).\n",
    );
    let vault = Vault::discover(tmp.path()).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
    assert_eq!(out.notes_synced, 3);
    // au->au-contract, au->entities-registry, au-contract->au, registry->au = 4
    assert_eq!(q1(&db, "MATCH (:Note)-[r:LINKS_TO]->(:Note) RETURN count(r)"), 4);
    // OKF queryable columns land on the node.
    assert_eq!(
        q1(
            &db,
            "MATCH (n:Note {id: 'concepts-agentic-unit'}) WHERE n.description = 'The core unit' AND n.kind = 'concept' RETURN count(n)"
        ),
        1
    );
}

#[test]
fn fts_search_works_after_sync() {
    let tmp = tempfile::tempdir().unwrap();
    aoa_mini(tmp.path());
    let vault = Vault::discover(tmp.path()).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();

    let conn = db.conn().unwrap();
    let result = conn.query(
        "CALL QUERY_FTS_INDEX('Note', 'note_fts', 'contract') RETURN node.id, score ORDER BY score DESC",
    );
    match result {
        Ok(rows) => {
            let hits: Vec<String> = rows
                .map(|r| match r.into_iter().next().unwrap() {
                    Value::String(s) => s,
                    other => panic!("unexpected {other:?}"),
                })
                .collect();
            assert!(
                hits.contains(&"concepts-au-contract".to_string()),
                "expected au-contract in FTS hits, got {hits:?}"
            );
        }
        Err(e) => {
            // FTS extension may be unavailable offline; don't fail the suite.
            eprintln!("FTS unavailable in this environment: {e}");
        }
    }
}
