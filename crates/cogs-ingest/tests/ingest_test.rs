//! End-to-end ingest tests against a temp fixture vault with a scripted
//! teacher — no network anywhere. Embeddings run as None (the FTS-only path
//! is first-class).

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use chrono::NaiveDate;
use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine};
use cogs_ingest::{IngestOptions, Ingester};
use cogs_llm::{ChatProvider, CompletionParams, Message};

const CONFIG: &str = r#"
[vault]
notes = ["wiki/**/*.md"]
exclude = ["wiki/index.md", "wiki/log.md", "wiki/_lint/**"]
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

const CLAIM_1: &str = "Anthropic announced a registry for MCP servers.";
const CLAIM_2: &str = "The registry verifies publisher identity before listing.";

/// Routes canned replies by pipeline stage (recognised from the system
/// prompt), so tests stay robust to retrieval-dependent call counts.
/// Extract/links/update replies are strict queues (exhaustion panics);
/// contradiction checks fall back to "no findings" — their count varies with
/// what FTS surfaces.
struct ScriptedChat {
    extract: Mutex<VecDeque<String>>,
    links: Mutex<VecDeque<String>>,
    update: Mutex<VecDeque<String>>,
    contradiction: Mutex<VecDeque<String>>,
}

impl ScriptedChat {
    fn routed(extract: &[&str], links: &[String], update: &[&str], contradiction: &[&str]) -> Self {
        let q = |xs: &[&str]| Mutex::new(xs.iter().map(|s| s.to_string()).collect());
        Self {
            extract: q(extract),
            links: Mutex::new(links.to_vec().into()),
            update: q(update),
            contradiction: q(contradiction),
        }
    }

    /// Extraction plus a links pass that changes nothing.
    fn simple() -> Self {
        Self::routed(&[extraction_reply()], &[passthrough_links()], &[], &[])
    }
}

/// A links reply that returns the claims untouched.
fn passthrough_links() -> String {
    serde_json::json!({
        "linked_claims": [CLAIM_1, CLAIM_2],
        "new_pages": [],
        "cross_references": [],
        "update_targets": []
    })
    .to_string()
}

impl ChatProvider for ScriptedChat {
    fn name(&self) -> &str {
        "scripted"
    }
    fn complete(&self, messages: &[Message], _params: &CompletionParams) -> anyhow::Result<String> {
        let system = &messages[0].content;
        let pop = |q: &Mutex<VecDeque<String>>, stage: &str| {
            q.lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("ScriptedChat: unexpected extra {stage} call"))
        };
        if system.contains("ingest engine") || system.contains("merge partial extractions") {
            Ok(pop(&self.extract, "extract"))
        } else if system.contains("weave freshly extracted claims") {
            Ok(pop(&self.links, "suggest_links"))
        } else if system.contains("update ONE wiki page") {
            Ok(pop(&self.update, "page_update"))
        } else if system.contains("check ONE wiki page") {
            Ok(self
                .contradiction
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"findings": []}"#.to_string()))
        } else {
            panic!("ScriptedChat: unrecognised system prompt: {system:.60}");
        }
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Mini vault: two concepts, one entity, one already-ingested source+raw pair,
/// log.md, git-committed clean.
fn mini_vault(root: &Path) {
    write(root, "cogs.toml", CONFIG);
    write(
        root,
        "wiki/concepts/agent-registry.md",
        "---\ntitle: Agent Registry\nkind: concept\nupdated: 2026-05-01\ntags: [core]\n---\nRegistries index agents and MCP servers so they can be discovered. See [[a2a-protocol]].\n",
    );
    write(
        root,
        "wiki/concepts/agent-identity.md",
        "---\ntitle: Agent Identity\nkind: concept\n---\nIdentity semantics for agents and publishers.\n",
    );
    write(
        root,
        "wiki/entities/a2a-protocol.md",
        "---\ntitle: A2A Protocol\nkind: entity\n---\nAn interop protocol. Links [[agent-registry]].\n",
    );
    write(
        root,
        "wiki/sources/old-article.md",
        "---\ntitle: Old article\nkind: source\nsource_refs:\n  - raw/clips/2026-01-01-old.md\n---\n## Summary\n\nOld news about registries.\n",
    );
    write(
        root,
        "raw/clips/2026-01-01-old.md",
        "---\ntitle: Old article\ncaptured_at: 2026-01-01\nurl: https://example.com/old\n---\nOld body text about agent registries.\n",
    );
    write(root, "wiki/log.md", "# Log\n\n## [2026-01-01] bootstrap | vault\n- created\n");
    write(root, "wiki/index.md", "# Index\n");
    write(root, ".gitignore", ".cogs/\n");

    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@test"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fixture"]);
}

/// The capture under test — added after the initial commit, like a fresh clip.
fn add_capture(root: &Path) -> &'static str {
    write(
        root,
        "raw/clips/2026-07-03-mcp-registry.md",
        "---\ntitle: MCP servers get a registry\ncaptured_at: 2026-07-03\nurl: https://example.com/mcp-reg\n---\nAnthropic announced a registry for MCP servers.\n\nThe registry indexes community servers and \"verifies publisher identity\" before listing.\n",
    );
    // raw/ dirt must not block ingest (the clean check scopes to wiki/)
    "raw/clips/2026-07-03-mcp-registry.md"
}

fn extraction_reply() -> &'static str {
    r#"{
      "summary": "Anthropic launched a registry for MCP servers that indexes community servers and verifies publishers.",
      "key_claims": [
        {"text": "Anthropic announced a registry for MCP servers.", "entities": ["Anthropic", "MCP registry"]},
        {"text": "The registry verifies publisher identity before listing.", "entities": ["MCP registry"]}
      ],
      "quotes": [
        {"text": "verifies publisher identity", "location": "para 2"},
        {"text": "this text is fabricated and appears nowhere", "location": "para 9"}
      ],
      "entities": [{"name": "MCP registry", "kind": "entity", "blurb": "Registry of MCP servers."}],
      "topics": ["agent-registry"],
      "suggested_slug": "mcp-registry-announcement",
      "tags": ["mcp", "Registry Stuff"],
      "author": null,
      "publisher": "example.com"
    }"#
}

fn setup(root: &Path) -> (Vault, GraphDb) {
    mini_vault(root);
    let vault = Vault::discover(root).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
    (vault, db)
}

fn opts() -> IngestOptions {
    IngestOptions {
        today: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
        ..Default::default()
    }
}

#[test]
fn happy_path_writes_source_page_log_and_training_records() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());

    let chat = ScriptedChat::simple();
    let report = Ingester::new(&vault, &db, &chat, None, opts())
        .ingest(Path::new(raw))
        .unwrap();

    // Source page written with schema-conformant frontmatter + sections.
    assert_eq!(report.source_page.as_deref(), Some("wiki/sources/mcp-registry-announcement.md"));
    let page =
        std::fs::read_to_string(tmp.path().join("wiki/sources/mcp-registry-announcement.md"))
            .unwrap();
    assert!(page.starts_with("---\ntitle: MCP servers get a registry\nkind: source\nstatus: draft\n"), "page:\n{page}");
    assert!(page.contains("updated: 2026-07-03"));
    assert!(page.contains("captured_at: 2026-07-03"));
    assert!(page.contains("source_refs:\n  - raw/clips/2026-07-03-mcp-registry.md"));
    assert!(page.contains("owner: llm"));
    assert!(page.contains("## Summary"));
    assert!(page.contains("- Anthropic announced a registry for MCP servers."));
    // tags sanitised to lowercase tokens
    assert!(page.contains("tags: [mcp, registry-stuff]"));
    // verbatim quote kept, fabricated quote dropped with a warning
    assert!(page.contains("> \"verifies publisher identity\" — para 2"));
    assert!(!page.contains("fabricated"));
    assert!(report.warnings.iter().any(|w| w.contains("non-verbatim quote")));

    // Log entry appended.
    let log = std::fs::read_to_string(tmp.path().join("wiki/log.md")).unwrap();
    assert!(log.contains("## [2026-07-03] ingest | MCP servers get a registry"));
    assert!(log.contains("- source: raw/clips/2026-07-03-mcp-registry.md"));

    // Graph re-synced: the new SOURCE_OF edge exists.
    assert!(report.synced);
    let rows = db
        .query_json(
            "MATCH (n:Note {id: 'sources-mcp-registry-announcement'})-[:SOURCE_OF]->(r:Resource) \
             RETURN r.path AS p",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["p"].as_str().unwrap(), "raw/clips/2026-07-03-mcp-registry.md");

    // Training capture: extract + suggest_links (+ any contradiction checks).
    assert!(report.training_records >= 2);
    let runs = vault.state_dir().join("training/runs");
    let jsonl = std::fs::read_to_string(runs.join(format!("{}.jsonl", report.run_id))).unwrap();
    let rec: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(rec["task"], "extract");
    assert_eq!(rec["parsed_ok"], true);
    assert!(rec["messages"][1]["content"].as_str().unwrap().contains("Anthropic announced"));
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(runs.join(format!("{}.meta.json", report.run_id))).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["writes"][0]["rel_path"], "wiki/sources/mcp-registry-announcement.md");
    assert_eq!(manifest["writes"][0]["kind"], "created");
    assert_eq!(manifest["writes"][0]["seq"], 1);
}

#[test]
fn weave_links_updates_and_contradictions_land() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());

    let links_reply = serde_json::json!({
        "linked_claims": [
            // valid link (alias form) + an unresolvable one that must unwrap
            "Anthropic announced a [[mcp-registry|registry]] for [[nonsense-page|MCP]] servers.",
            // rewritten text: must fall back to the original claim
            "The registry verifies publisher identity before listing, which is great news."
        ],
        "new_pages": [
            {"slug": "mcp-registry", "dir": "entities", "title": "MCP Registry", "kind": "entity", "blurb": "Anthropic's registry of MCP servers."},
            {"slug": "Bad Slug!", "dir": "entities", "title": "x", "kind": "entity", "blurb": ""},
            {"slug": "a2a-protocol", "dir": "entities", "title": "dup", "kind": "entity", "blurb": ""}
        ],
        "cross_references": ["entities/a2a-protocol", "concepts/agent-identity", "concepts/does-not-exist"],
        "update_targets": ["concepts/agent-registry", "sources/old-article", "concepts/does-not-exist"]
    })
    .to_string();

    let update_reply = serde_json::json!({
        "topic": "MCP server registry",
        "section_md": "A dedicated [[mcp-registry]] now catalogues MCP servers, with publisher verification ([[mcp-registry-announcement]]). Also see [[ghost-page]].",
        "relevant": true
    })
    .to_string();

    let contradiction_reply = serde_json::json!({
        "findings": [
            {
                "page_id": "concepts-agent-registry",
                "existing_text": "Registries index agents and MCP servers so they can be discovered.",
                "new_claim": "The registry verifies publisher identity before listing.",
                "explanation": "Discovery-only vs verification-gated listing."
            },
            {
                "page_id": "concepts-agent-registry",
                "existing_text": "this sentence is not on the page",
                "new_claim": "The registry verifies publisher identity before listing.",
                "explanation": "hallucinated quote must be dropped"
            }
        ]
    })
    .to_string();

    let chat = ScriptedChat::routed(
        &[extraction_reply()],
        &[links_reply],
        &[&update_reply],
        &[&contradiction_reply],
    );
    let report = Ingester::new(&vault, &db, &chat, None, opts())
        .ingest(Path::new(raw))
        .unwrap();

    // New page created (malformed + duplicate specs dropped).
    assert_eq!(report.pages_created, vec!["entities-mcp-registry"]);
    let new_page = std::fs::read_to_string(tmp.path().join("wiki/entities/mcp-registry.md")).unwrap();
    assert!(new_page.contains("title: MCP Registry"));
    assert!(new_page.contains("kind: entity"));
    assert!(new_page.contains("Anthropic's registry of MCP servers."));
    assert!(new_page.contains("Source: [[mcp-registry-announcement]]"));
    assert!(report.warnings.iter().any(|w| w.contains("Bad Slug!") || w.contains("bad slug!")));
    assert!(report.warnings.iter().any(|w| w.contains("entities-a2a-protocol already exists")));

    // Source page: linked claim kept, unresolvable link unwrapped, rewritten
    // claim reverted, cross-references validated.
    let page =
        std::fs::read_to_string(tmp.path().join("wiki/sources/mcp-registry-announcement.md"))
            .unwrap();
    assert!(page.contains("- Anthropic announced a [[mcp-registry|registry]] for MCP servers."), "page:\n{page}");
    assert!(page.contains(&format!("- {CLAIM_2}\n")));
    assert!(page.contains("## Cross-references"));
    assert!(page.contains("- [[entities/a2a-protocol]]"));
    assert!(page.contains("- [[concepts/agent-identity]]"));
    assert!(!page.contains("does-not-exist"));
    assert!(report.warnings.iter().any(|w| w.contains("nonsense-page")));
    assert!(report.warnings.iter().any(|w| w.contains("rewrote claim 2")));

    // Concept page updated: appended dated section, source_refs gained the
    // raw path (missing key inserted), updated bumped, ghost link unwrapped.
    assert_eq!(report.pages_updated, vec!["concepts-agent-registry"]);
    let concept =
        std::fs::read_to_string(tmp.path().join("wiki/concepts/agent-registry.md")).unwrap();
    assert!(concept.contains("## MCP server registry (2026-07-03 ingest)"), "concept:\n{concept}");
    assert!(concept.contains("source_refs:\n  - raw/clips/2026-07-03-mcp-registry.md"));
    assert!(concept.contains("updated: 2026-07-03"));
    assert!(concept.contains("([[mcp-registry-announcement]])"));
    assert!(concept.contains("Also see ghost-page."));
    // original body untouched above the new section
    assert!(concept.contains("Registries index agents and MCP servers"));

    // Contradiction: verbatim finding kept (on the source page, both fm edge
    // and body section); hallucinated finding dropped.
    assert_eq!(report.contradictions.len(), 1);
    assert!(page.contains("contradicts: [concepts-agent-registry]"));
    assert!(page.contains("## Contradictions"));
    assert!(page.contains("Discovery-only vs verification-gated listing."));

    // The updated concept page's manifest entry tracks the appended section.
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            vault
                .state_dir()
                .join("training/runs")
                .join(format!("{}.meta.json", report.run_id)),
        )
        .unwrap(),
    )
    .unwrap();
    let writes = manifest["writes"].as_array().unwrap();
    let upd = writes
        .iter()
        .find(|w| w["rel_path"] == "wiki/concepts/agent-registry.md")
        .expect("concept update in manifest");
    assert_eq!(upd["kind"], "appended");
    assert!(upd["section_heading"]
        .as_str()
        .unwrap()
        .contains("MCP server registry (2026-07-03 ingest)"));

    // Log mentions everything.
    let log = std::fs::read_to_string(tmp.path().join("wiki/log.md")).unwrap();
    assert!(log.contains("- pages updated: concepts-agent-registry"));
    assert!(log.contains("- pages created: entities-mcp-registry"));
    assert!(log.contains("- contradictions raised: 1"));

    // Re-sync picked up the woven edges: concept page now cites the source.
    let rows = db
        .query_json(
            "MATCH (n:Note {id: 'concepts-agent-registry'})-[:CITES]->(m:Note {id: 'sources-mcp-registry-announcement'}) RETURN m.id AS id",
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "concept → source CITES edge after resync");
}

#[test]
fn update_without_source_citation_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());

    let links_reply = serde_json::json!({
        "linked_claims": [CLAIM_1, CLAIM_2],
        "new_pages": [],
        "cross_references": [],
        "update_targets": ["concepts/agent-registry"]
    })
    .to_string();
    // Section that never cites the source page: must be rejected.
    let update_reply = serde_json::json!({
        "topic": "Registry news",
        "section_md": "Something new happened.",
        "relevant": true
    })
    .to_string();

    let chat =
        ScriptedChat::routed(&[extraction_reply()], &[links_reply], &[&update_reply], &[]);
    let report = Ingester::new(&vault, &db, &chat, None, opts())
        .ingest(Path::new(raw))
        .unwrap();

    assert!(report.pages_updated.is_empty());
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("never cites [[mcp-registry-announcement]]")));
    let concept =
        std::fs::read_to_string(tmp.path().join("wiki/concepts/agent-registry.md")).unwrap();
    assert!(!concept.contains("Registry news"));
}

#[test]
fn dirty_wiki_tree_refuses_unless_forced() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());
    // dirty the note tree
    write(tmp.path(), "wiki/concepts/agent-identity.md", "edited but uncommitted\n");

    let chat = ScriptedChat::simple();
    let err = Ingester::new(&vault, &db, &chat, None, opts())
        .ingest(Path::new(raw))
        .unwrap_err();
    assert!(err.to_string().contains("uncommitted changes"), "err: {err:#}");

    // --force proceeds (fresh scripted replies; the failed run consumed none).
    let chat = ScriptedChat::simple();
    let o = IngestOptions { force: true, ..opts() };
    let report = Ingester::new(&vault, &db, &chat, None, o).ingest(Path::new(raw)).unwrap();
    assert!(report.source_page.is_some());
    assert!(report.warnings.iter().any(|w| w.contains("--force")));
}

#[test]
fn already_ingested_returns_early_without_llm_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());

    let chat = ScriptedChat::routed(&[], &[], &[], &[]); // any LLM call would panic
    let report = Ingester::new(&vault, &db, &chat, None, opts())
        .ingest(Path::new("raw/clips/2026-01-01-old.md"))
        .unwrap();
    assert_eq!(report.already_ingested.as_deref(), Some("wiki/sources/old-article.md"));
    assert!(report.source_page.is_none());
}

#[test]
fn dry_run_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());
    let log_before = std::fs::read_to_string(tmp.path().join("wiki/log.md")).unwrap();

    let chat = ScriptedChat::simple();
    let o = IngestOptions { dry_run: true, ..opts() };
    let report = Ingester::new(&vault, &db, &chat, None, o).ingest(Path::new(raw)).unwrap();

    assert!(report.dry_run);
    assert_eq!(report.planned.len(), 2);
    assert!(report.planned[0].content.contains("## Summary"));
    assert!(!tmp.path().join("wiki/sources/mcp-registry-announcement.md").exists());
    assert_eq!(std::fs::read_to_string(tmp.path().join("wiki/log.md")).unwrap(), log_before);
    assert!(!vault.state_dir().join("training").exists());
    assert_eq!(report.training_records, 0);
}

#[test]
fn slug_collision_gets_suffixed() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());
    // occupy the model's suggested slug
    write(
        tmp.path(),
        "wiki/sources/mcp-registry-announcement.md",
        "---\ntitle: Taken\nkind: source\nsource_refs:\n  - raw/clips/2026-01-01-old.md\n---\nbody\n",
    );
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "occupy slug"]);

    let chat = ScriptedChat::simple();
    let report = Ingester::new(&vault, &db, &chat, None, opts()).ingest(Path::new(raw)).unwrap();
    assert_eq!(
        report.source_page.as_deref(),
        Some("wiki/sources/mcp-registry-announcement-2.md")
    );
    assert!(tmp.path().join("wiki/sources/mcp-registry-announcement-2.md").exists());
}

#[test]
fn unparseable_reply_retries_once_then_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());

    let chat = ScriptedChat::routed(
        &["sorry, no JSON here", extraction_reply()],
        &[passthrough_links()],
        &[],
        &[],
    );
    let report = Ingester::new(&vault, &db, &chat, None, opts()).ingest(Path::new(raw)).unwrap();
    assert!(report.source_page.is_some());
    // both extract attempts recorded: one parse failure, one success
    let jsonl = std::fs::read_to_string(
        vault
            .state_dir()
            .join("training/runs")
            .join(format!("{}.jsonl", report.run_id)),
    )
    .unwrap();
    let recs: Vec<serde_json::Value> =
        jsonl.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(recs[0]["task"], "extract");
    assert_eq!(recs[0]["parsed_ok"], false);
    assert_eq!(recs[1]["task"], "extract");
    assert_eq!(recs[1]["parsed_ok"], true);
}
