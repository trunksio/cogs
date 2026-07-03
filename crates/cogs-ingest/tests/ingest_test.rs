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

/// Pops canned replies; panics if the pipeline makes an unexpected extra call.
struct ScriptedChat(Mutex<VecDeque<String>>);

impl ScriptedChat {
    fn new(replies: &[&str]) -> Self {
        Self(Mutex::new(replies.iter().map(|s| s.to_string()).collect()))
    }
}

impl ChatProvider for ScriptedChat {
    fn name(&self) -> &str {
        "scripted"
    }
    fn complete(&self, _messages: &[Message], _params: &CompletionParams) -> anyhow::Result<String> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("ScriptedChat exhausted: pipeline made an unexpected extra LLM call"))
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
        "---\ntitle: Agent Registry\nkind: concept\nupdated: 2026-05-01\ntags: [core]\n---\nRegistries index agents. See [[a2a-protocol]].\n",
    );
    write(
        root,
        "wiki/concepts/agent-identity.md",
        "---\ntitle: Agent Identity\nkind: concept\n---\nIdentity semantics for agents.\n",
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
        {"text": "Anthropic announced a registry for MCP servers.", "entities": ["Anthropic", "MCP"]},
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

    let chat = ScriptedChat::new(&[extraction_reply()]);
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

    // Training capture: one record + manifest tying it to the created page.
    assert_eq!(report.training_records, 1);
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
fn dirty_wiki_tree_refuses_unless_forced() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());
    // dirty the note tree
    write(tmp.path(), "wiki/concepts/agent-registry.md", "edited but uncommitted\n");

    let chat = ScriptedChat::new(&[extraction_reply()]);
    let err = Ingester::new(&vault, &db, &chat, None, opts())
        .ingest(Path::new(raw))
        .unwrap_err();
    assert!(err.to_string().contains("uncommitted changes"), "err: {err:#}");

    // --force proceeds (fresh scripted reply; the failed run consumed none).
    let chat = ScriptedChat::new(&[extraction_reply()]);
    let o = IngestOptions { force: true, ..opts() };
    let report = Ingester::new(&vault, &db, &chat, None, o).ingest(Path::new(raw)).unwrap();
    assert!(report.source_page.is_some());
    assert!(report.warnings.iter().any(|w| w.contains("--force")));
}

#[test]
fn already_ingested_returns_early_without_llm_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());

    let chat = ScriptedChat::new(&[]); // any LLM call would panic
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

    let chat = ScriptedChat::new(&[extraction_reply()]);
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

    let chat = ScriptedChat::new(&[extraction_reply()]);
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

    let chat = ScriptedChat::new(&["sorry, no JSON here", extraction_reply()]);
    let report = Ingester::new(&vault, &db, &chat, None, opts()).ingest(Path::new(raw)).unwrap();
    assert!(report.source_page.is_some());
    // both attempts recorded: one parse failure, one success
    assert_eq!(report.training_records, 2);
    let jsonl = std::fs::read_to_string(
        vault
            .state_dir()
            .join("training/runs")
            .join(format!("{}.jsonl", report.run_id)),
    )
    .unwrap();
    let recs: Vec<serde_json::Value> =
        jsonl.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(recs[0]["parsed_ok"], false);
    assert_eq!(recs[1]["parsed_ok"], true);
}
