//! Conformance: cogs consumes a Google Open Knowledge Format (OKF) v0.1
//! bundle — github.com/GoogleCloudPlatform/knowledge-catalog, okf/SPEC.md —
//! with the shipped examples/okf-google.cogs.toml and zero code special
//! cases. The vault is the vendored GA4 sample bundle subset (Apache-2.0,
//! see fixtures/okf-google-ga4/ATTRIBUTION.md) plus two test-authored files
//! covering what GA4 doesn't: bundle-root-absolute links, wikilink
//! coexistence, a log.md, and an unknown frontmatter key.
//!
//! Spec rules exercised: reserved index.md/log.md are not concepts (§3.1);
//! `type` → kind (§4.1); unknown frontmatter keys preserved (§4.1); markdown
//! links in both absolute and relative forms become untyped directed edges
//! (§5); broken links tolerated — no edge, no error (§5.3, §9); links inside
//! fenced code are not links.

use std::fs;
use std::path::Path;

use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine};

const OKF_GOOGLE_CONFIG: &str = include_str!("../../../examples/okf-google.cogs.toml");

/// Test-authored root concept: the GA4 files only use relative links, so
/// this adds the spec's absolute form, wikilink coexistence, a tolerated
/// broken link, ignored external/non-.md links, code-fence immunity, and a
/// producer-defined frontmatter key.
const OVERVIEW: &str = "---\n\
type: Playbook\n\
title: Analytics onboarding\n\
description: Where to start with the GA4 export data.\n\
owner_team: analytics\n\
tags: [onboarding]\n\
timestamp: 2026-06-01T00:00:00Z\n\
---\n\
\n\
Start with the [events table](/tables/events_.md), then the\n\
[event count metric](./references/metrics/event_count.md).\n\
Wikilinks coexist: [[datasets/ga4_obfuscated_sample_ecommerce]].\n\
\n\
Not-yet-written knowledge is fine: [churn model](/models/churn.md).\n\
Ignored: [GA4 docs](https://developers.google.com/analytics),\n\
[export guide](/guides/export.pdf), [team](mailto:analytics@example.com).\n\
\n\
```sql\n\
-- in-code links never become edges: [join](/references/joins/events___ads_clickstats.md)\n\
SELECT 1;\n\
```\n";

const LOG: &str = "# Bundle Update Log\n\
\n\
## 2026-06-01\n\
* **Creation**: Added the [onboarding playbook](/overview.md).\n\
\n\
## 2026-05-28\n\
* **Initialization**: Imported the GA4 sample bundle.\n";

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), &to).unwrap();
        }
    }
}

fn count(db: &GraphDb, cypher: &str) -> i64 {
    db.query_json(cypher).unwrap()[0]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .as_i64()
        .unwrap()
}

#[test]
fn google_okf_bundle_indexes_with_the_example_config() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/okf-google-ga4/bundle");
    copy_tree(&fixture, tmp.path());
    fs::write(tmp.path().join("cogs.toml"), OKF_GOOGLE_CONFIG).unwrap();
    fs::write(tmp.path().join("overview.md"), OVERVIEW).unwrap();
    fs::write(tmp.path().join("log.md"), LOG).unwrap();

    let vault = Vault::discover(tmp.path()).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();

    // §3.1: 8 vendored concepts + overview.md = 9 nodes. The 6 index.md
    // files (root and nested) and log.md are reserved, not concepts.
    assert_eq!(out.notes_synced, 9);
    assert_eq!(out.resources_synced, 0);
    assert_eq!(count(&db, "MATCH (n:Note) WHERE n.id CONTAINS 'index' RETURN count(n)"), 0);
    assert_eq!(count(&db, "MATCH (n:Note {id: 'log'}) RETURN count(n)"), 0);

    // §4.1: OKF `type` lands as the note kind, values open-ended.
    assert_eq!(count(&db, "MATCH (n:Note {kind: 'BigQuery Table'}) RETURN count(n)"), 1);
    assert_eq!(count(&db, "MATCH (n:Note {kind: 'BigQuery Dataset'}) RETURN count(n)"), 1);
    assert_eq!(count(&db, "MATCH (n:Note {kind: 'Reference'}) RETURN count(n)"), 6);
    assert_eq!(count(&db, "MATCH (n:Note {kind: 'Playbook'}) RETURN count(n)"), 1);

    // §4.1: `timestamp` maps to updated; unknown keys (`resource`,
    // `description`, `owner_team`) are preserved in the note payload.
    assert_eq!(
        count(
            &db,
            "MATCH (n:Note {id: 'tables-events_'}) WHERE n.updated = date('2026-05-28') RETURN count(n)"
        ),
        1
    );
    let fm = db
        .query_json("MATCH (n:Note {id: 'overview'}) RETURN n.frontmatter_json AS fm")
        .unwrap()[0]["fm"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(fm.contains("\"owner_team\":\"analytics\""), "{fm}");
    let fm = db
        .query_json("MATCH (n:Note {id: 'tables-events_'}) RETURN n.frontmatter_json AS fm")
        .unwrap()[0]["fm"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(fm.contains("\"resource\"") && fm.contains("\"description\""), "{fm}");

    // §5: edges from BOTH markdown link forms plus the coexisting wikilink.
    // events_.md (relative ../ links): 6 resolve — 5 metrics + the join;
    // its other 3 metric links point at files not vendored → broken,
    // tolerated (§5.3). overview.md: absolute → events_, relative ./ →
    // event_count, wikilink → the dataset; /models/churn.md broken;
    // external / non-.md / in-code ignored. Total 6 + 3 = 9.
    assert_eq!(count(&db, "MATCH ()-[r:LINKS_TO]->() RETURN count(r)"), 9);
    assert_eq!(
        count(&db, "MATCH (:Note {id: 'tables-events_'})-[r:LINKS_TO]->() RETURN count(r)"),
        6
    );
    for target in [
        "tables-events_",                          // absolute form
        "references-metrics-event_count",          // relative ./ form
        "datasets-ga4_obfuscated_sample_ecommerce", // wikilink coexistence
    ] {
        assert_eq!(
            count(
                &db,
                &format!(
                    "MATCH (:Note {{id: 'overview'}})-[r:LINKS_TO]->(:Note {{id: '{target}'}}) RETURN count(r)"
                )
            ),
            1,
            "missing overview -> {target}"
        );
    }
    // relative ../ traversal: events_ → the join reference, two dirs over.
    assert_eq!(
        count(
            &db,
            "MATCH (:Note {id: 'tables-events_'})-[r:LINKS_TO]->(:Note {id: 'references-joins-events___ads_clickstats'}) RETURN count(r)"
        ),
        1
    );
}
