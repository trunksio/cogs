//! An Open Knowledge Format (OKF) vault — the spec from
//! github.com/ashtonkj/Nameless.TaskList — indexes with the shipped example
//! config and zero cogs code changes: OKF `type:` as kind, its relationship
//! fields as typed edges, `[[type/subtype/slug]]` path links, messages as the
//! immutable resource layer. Uses examples/okf.cogs.toml verbatim so the
//! example stays honest.

use std::fs;
use std::path::Path;

use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine};

const OKF_CONFIG: &str = include_str!("../../../examples/okf.cogs.toml");

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn okf_mini(root: &Path) {
    write(root, "cogs.toml", OKF_CONFIG);
    write(
        root,
        "people/family/ethan.md",
        "---\ntype: Person\ntitle: Ethan\nrole: son\ncontext: [family]\ntags: [family]\n---\nSee [[topics/active/ethan-birthday-party-2026]].\n",
    );
    write(
        root,
        "tasks/pending/book-venue.md",
        "---\ntype: Task\ntitle: Book venue\nstatus: pending\npriority: high\ndue: 2026-07-20\npeople: [people/family/ethan]\ntopic: topics/active/ethan-birthday-party-2026\nsource_message: messages/whatsapp-wife-direct/2026-06-15T14-17-45.md\nproject: projects/birthday-2026\n---\nCall [[locations/party-hall]] about availability.\n",
    );
    write(
        root,
        "topics/active/ethan-birthday-party-2026.md",
        "---\ntype: Topic\ntitle: Ethan birthday party 2026\nstatus: active\nlast_updated: 2026-06-15\npeople: [people/family/ethan]\n---\n## Current understanding\n\nParty planning underway; venue task spawned: [[tasks/pending/book-venue]].\n",
    );
    write(
        root,
        "projects/birthday-2026.md",
        "---\ntype: Project\ntitle: Birthday 2026\nstatus: in-progress\ndeadline: 2026-07-25\n---\nUmbrella for party planning.\n",
    );
    write(
        root,
        "locations/party-hall.md",
        "---\ntype: Location\ntitle: Party Hall\naddress: 1 Main Rd\n---\nVenue candidate.\n",
    );
    write(
        root,
        "commitments/reply-to-caterer.md",
        "---\ntype: Commitment\ntitle: Reply to caterer\nstatus: unresolved\ndue: 2026-07-10\ntask_assigned: tasks/pending/book-venue\nsource_message: messages/whatsapp-wife-direct/2026-06-15T14-17-45.md\n---\nPromised a reply this week.\n",
    );
    // Immutable message layer (resources).
    write(
        root,
        "messages/whatsapp-wife-direct/2026-06-15T14-17-45.md",
        "---\ntitle: Venue reminder\ntimestamp: 2026-06-15T14:17:45+02:00\nsender: wife\n---\n## Raw\n\nDon't forget to book the venue for Ethan's party!\n",
    );
    // _meta is excluded from the graph.
    write(root, "_meta/pipeline-log.md", "# Pipeline log\n");
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
fn okf_vault_indexes_with_the_example_config() {
    let tmp = tempfile::tempdir().unwrap();
    okf_mini(tmp.path());
    // Vault::discover parses + validates the example config as a side effect.
    let vault = Vault::discover(tmp.path()).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    let out = SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();

    // 6 notes (person/task/topic/project/location/commitment), 1 message
    // resource, _meta excluded.
    assert_eq!(out.notes_synced, 6);
    assert_eq!(out.resources_synced, 1);

    // OKF `type:` lands as the note kind.
    assert_eq!(
        count(&db, "MATCH (n:Note {kind: 'Task'}) RETURN count(n)"),
        1
    );

    // Typed edges from OKF relationship fields:
    // INVOLVES: task→ethan, topic→ethan
    assert_eq!(count(&db, "MATCH ()-[r:INVOLVES]->() RETURN count(r)"), 2);
    // ABOUT: task→topic
    assert_eq!(count(&db, "MATCH ()-[r:ABOUT]->() RETURN count(r)"), 1);
    // SOURCE_OF: task→message, commitment→message (provenance to resources)
    assert_eq!(
        count(&db, "MATCH (:Note)-[r:SOURCE_OF]->(:Resource) RETURN count(r)"),
        2
    );
    // ASSIGNED: commitment→task; PART_OF: task→project
    assert_eq!(count(&db, "MATCH ()-[r:ASSIGNED]->() RETURN count(r)"), 1);
    assert_eq!(count(&db, "MATCH ()-[r:PART_OF]->() RETURN count(r)"), 1);

    // Path-form body wikilinks resolve across type dirs:
    // person→topic, task→location, topic→task.
    assert_eq!(count(&db, "MATCH ()-[r:LINKS_TO]->() RETURN count(r)"), 3);
    assert_eq!(
        count(
            &db,
            "MATCH (:Note {id: 'tasks-pending-book-venue'})-[:LINKS_TO]->(l:Note {id: 'locations-party-hall'}) RETURN count(l)"
        ),
        1
    );
}
