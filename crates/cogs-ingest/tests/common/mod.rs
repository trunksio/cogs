//! Shared fixture + scripted teacher for the ingest/distill e2e tests.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use chrono::NaiveDate;
use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine};
use cogs_ingest::IngestOptions;
use cogs_llm::{ChatProvider, CompletionParams, Message};

pub const CONFIG: &str = r#"
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

pub const CLAIM_1: &str = "Anthropic announced a registry for MCP servers.";
pub const CLAIM_2: &str = "The registry verifies publisher identity before listing.";

/// Routes canned replies by pipeline stage (recognised from the system
/// prompt), so tests stay robust to retrieval-dependent call counts.
/// Extract/links/update replies are strict queues (exhaustion panics);
/// contradiction checks fall back to "no findings" — their count varies with
/// what FTS surfaces.
pub struct ScriptedChat {
    extract: Mutex<VecDeque<String>>,
    links: Mutex<VecDeque<String>>,
    update: Mutex<VecDeque<String>>,
    contradiction: Mutex<VecDeque<String>>,
}

impl ScriptedChat {
    pub fn routed(
        extract: &[&str],
        links: &[String],
        update: &[&str],
        contradiction: &[&str],
    ) -> Self {
        let q = |xs: &[&str]| Mutex::new(xs.iter().map(|s| s.to_string()).collect());
        Self {
            extract: q(extract),
            links: Mutex::new(links.to_vec().into()),
            update: q(update),
            contradiction: q(contradiction),
        }
    }

    /// Extraction plus a links pass that changes nothing.
    pub fn simple() -> Self {
        Self::routed(&[extraction_reply()], &[passthrough_links()], &[], &[])
    }
}

/// A links reply that returns the claims untouched.
pub fn passthrough_links() -> String {
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

pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

pub fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Mini vault: two concepts, one entity, one already-ingested source+raw pair
/// (schema-conformant, so distill can mine it), log.md, git-committed clean.
pub fn mini_vault(root: &Path) {
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
        "---\ntitle: Old article\nkind: source\nstatus: draft\nupdated: 2026-01-01\nsource_refs:\n  - raw/clips/2026-01-01-old.md\ntags: [registry]\nauthor: Jane Doe\nowner: llm\n---\n\n# Old article\n\n## Summary\n\nOld news about agent registries and discovery.\n\n## Key claims\n\n- Registries let [[concepts/agent-registry|agent registries]] be discovered centrally.\n- Discovery predates verification.\n\n## Quotes\n\n> \"registries were inevitable\" — intro\n\n## Cross-references\n\n- [[concepts/agent-registry]]\n",
    );
    write(
        root,
        "raw/clips/2026-01-01-old.md",
        "---\ntitle: Old article\ncaptured_at: 2026-01-01\nurl: https://example.com/old\n---\nOld body text about agent registries. Everyone knew registries were inevitable.\n",
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
pub fn add_capture(root: &Path) -> &'static str {
    write(
        root,
        "raw/clips/2026-07-03-mcp-registry.md",
        "---\ntitle: MCP servers get a registry\ncaptured_at: 2026-07-03\nurl: https://example.com/mcp-reg\n---\nAnthropic announced a registry for MCP servers.\n\nThe registry indexes community servers and \"verifies publisher identity\" before listing.\n",
    );
    // raw/ dirt must not block ingest (the clean check scopes to wiki/)
    "raw/clips/2026-07-03-mcp-registry.md"
}

pub fn extraction_reply() -> &'static str {
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

pub fn setup(root: &Path) -> (Vault, GraphDb) {
    mini_vault(root);
    let vault = Vault::discover(root).unwrap();
    let db = GraphDb::open_rw(&vault, false).unwrap();
    SyncEngine::new(&vault).unwrap().sync(&db, false).unwrap();
    (vault, db)
}

pub fn opts() -> IngestOptions {
    IngestOptions {
        today: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
        ..Default::default()
    }
}
