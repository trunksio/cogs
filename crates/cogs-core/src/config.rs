use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Schema version baked into the config hash: bump to force DB rebuilds when
/// the generated DDL or sync semantics change incompatibly.
pub const SCHEMA_VERSION: u32 = 2;

pub const CONFIG_FILE_NAMES: &[&str] = &["cogs.toml", ".cogs/config.toml"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VaultConfig {
    pub vault: VaultSection,
    pub resources: Option<ResourcesSection>,
    pub notes: NotesSection,
    pub kinds: KindsSection,
    #[serde(rename = "edges")]
    pub edges: Vec<EdgeConfig>,
    pub tags: TagsSection,
    pub diagnostics: DiagnosticsSection,
    pub embeddings: EmbeddingsSection,
    pub llm: LlmSection,
    pub ingest: IngestSection,
    pub server: ServerSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VaultSection {
    /// Globs (vault-relative) selecting note files.
    pub notes: Vec<String>,
    pub exclude: Vec<String>,
    /// Prefix stripped from the relative path before deriving the note id,
    /// e.g. "wiki/" turns wiki/concepts/x.md into concepts-x.
    pub id_strip_prefix: String,
}

impl Default for VaultSection {
    fn default() -> Self {
        Self {
            notes: vec!["**/*.md".into()],
            exclude: vec![
                ".obsidian/**".into(),
                ".cogs/**".into(),
                ".git/**".into(),
                "node_modules/**".into(),
            ],
            id_strip_prefix: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResourcesSection {
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    /// Frontmatter fields holding dates on resource metadata.
    pub date_fields: ResourceDateFields,
    /// Frontmatter fields checked (in order) for a source URL; first http(s)/file: value wins.
    pub url_fields: Vec<String>,
}

impl Default for ResourcesSection {
    fn default() -> Self {
        Self {
            paths: vec![],
            exclude: vec![],
            date_fields: ResourceDateFields::default(),
            url_fields: vec![
                "source".into(),
                "url".into(),
                "origin".into(),
                "source_ref".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResourceDateFields {
    pub captured: String,
    pub source: String,
}

impl Default for ResourceDateFields {
    fn default() -> Self {
        Self { captured: "captured_at".into(), source: "source_date".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotesSection {
    pub fields: NoteFields,
}

impl Default for NotesSection {
    fn default() -> Self {
        Self { fields: NoteFields::default() }
    }
}

/// Which frontmatter keys map onto the typed Note columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NoteFields {
    pub title: String,
    pub kind: String,
    pub status: String,
    pub created: String,
    pub updated: String,
}

impl Default for NoteFields {
    fn default() -> Self {
        Self {
            title: "title".into(),
            kind: "kind".into(),
            status: "status".into(),
            created: "created".into(),
            updated: "updated".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KindsSection {
    /// Known kind values; empty = kinds unused.
    pub values: Vec<String>,
    pub unknown: Severity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Allow,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    /// REL table name; validated as ^[A-Z][A-Z0-9_]{0,30}$.
    pub name: String,
    pub source: EdgeSource,
    /// Frontmatter field (required when source = "frontmatter").
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub target: EdgeTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeSource {
    Wikilinks,
    Frontmatter,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeTarget {
    #[default]
    Note,
    Resource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TagsSection {
    /// Frontmatter field holding the tag list.
    pub field: String,
    /// Also scan the body for inline #tags.
    pub inline: bool,
}

impl Default for TagsSection {
    fn default() -> Self {
        Self { field: "tags".into(), inline: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiagnosticsSection {
    pub broken_link: Severity,
    pub ambiguous_link: Severity,
    /// kind -> frontmatter fields that must be present.
    pub required_fields: BTreeMap<String, Vec<String>>,
    pub stale_after_days: Option<u32>,
}

impl Default for DiagnosticsSection {
    fn default() -> Self {
        Self {
            broken_link: Severity::Warn,
            ambiguous_link: Severity::Warn,
            required_fields: BTreeMap::new(),
            stale_after_days: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbeddingsSection {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub dim: u32,
    pub endpoint: String,
    pub char_cap: u32,
    pub exclude_kinds: Vec<String>,
    pub embed_resources: bool,
    /// Env var holding the bearer key for the embeddings endpoint (omlx/cloud).
    /// Empty = none.
    pub api_key_env: String,
    /// Asymmetric retrieval instruction prepended to QUERY embeddings only
    /// (documents are embedded bare). Qwen3-Embedding et al. gain 1-5% from
    /// this. Empty = symmetric (query and document embedded identically).
    pub query_instruction: String,
}

impl Default for EmbeddingsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "ollama".into(),
            model: "nomic-embed-text".into(),
            dim: 768,
            endpoint: "http://localhost:11434".into(),
            char_cap: 7000,
            exclude_kinds: vec![],
            embed_resources: true,
            api_key_env: String::new(),
            query_instruction: String::new(),
        }
    }
}

/// Chat/completion LLM backend for `cogs ask` and ingest. Pluggable like the
/// embedding provider: any OpenAI-compatible endpoint (omlx, Ollama, OpenAI,
/// vLLM) plus a native Anthropic adapter. Not part of the schema hash —
/// changing it never triggers a graph rebuild.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmSection {
    /// omlx | ollama | openai | anthropic (anything else is treated as
    /// openai-compatible against `base_url`).
    pub provider: String,
    pub model: String,
    /// Base URL for OpenAI-compatible providers. Empty = provider default
    /// (omlx :8000, ollama :11434, openai api.openai.com).
    pub base_url: String,
    /// Env var holding the API key (cloud providers). Empty = none needed.
    pub api_key_env: String,
    /// Per-request cap; keeps local models bounded.
    pub max_tokens: u32,
    /// HTTP timeout per completion. Local models generating long outputs
    /// (chunked ingest extractions) can legitimately take minutes.
    pub timeout_secs: u64,
    /// Send OpenAI `response_format: json_object` on JSON calls. None =
    /// provider default: off for omlx (its constrained decoding degenerates
    /// into float arrays), on elsewhere. Prompts demand JSON either way.
    pub response_format: Option<bool>,
    /// Extra request-body fields merged into every chat completion — server
    /// escape hatch, e.g. disabling Qwen thinking on omlx/vLLM:
    /// `[llm.extra_body.chat_template_kwargs] enable_thinking = false`.
    pub extra_body: serde_json::Value,
    /// Per-task overlay for `cogs ask` — unset fields inherit from [llm].
    /// Lets a QA-tuned model answer while an ingest-tuned one extracts.
    pub ask: Option<LlmOverride>,
    /// Per-task overlay for `cogs ingest`.
    pub ingest: Option<LlmOverride>,
}

/// Partial [llm] override: every field optional, falling back to the base
/// section. `[llm.ask] model = "..."` swaps just the model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmOverride {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub response_format: Option<bool>,
    pub extra_body: Option<serde_json::Value>,
}

impl LlmSection {
    /// The effective [llm] for a task ("ask" | "ingest"): the base section
    /// with that task's overlay applied.
    pub fn for_task(&self, task: &str) -> LlmSection {
        let overlay = match task {
            "ask" => self.ask.as_ref(),
            "ingest" => self.ingest.as_ref(),
            _ => None,
        };
        let mut out = self.clone();
        out.ask = None;
        out.ingest = None;
        if let Some(o) = overlay {
            if let Some(v) = &o.provider {
                out.provider = v.clone();
            }
            if let Some(v) = &o.model {
                out.model = v.clone();
            }
            if let Some(v) = &o.base_url {
                out.base_url = v.clone();
            }
            if let Some(v) = &o.api_key_env {
                out.api_key_env = v.clone();
            }
            if let Some(v) = o.max_tokens {
                out.max_tokens = v;
            }
            if let Some(v) = o.timeout_secs {
                out.timeout_secs = v;
            }
            if let Some(v) = o.response_format {
                out.response_format = Some(v);
            }
            if let Some(v) = &o.extra_body {
                out.extra_body = v.clone();
            }
        }
        out
    }
}

impl Default for LlmSection {
    fn default() -> Self {
        Self {
            provider: "omlx".into(),
            model: "mlx-community/Qwen2.5-7B-Instruct-4bit".into(),
            base_url: String::new(),
            api_key_env: String::new(),
            max_tokens: 2048,
            timeout_secs: 300,
            response_format: None,
            extra_body: serde_json::Value::Null,
            ask: None,
            ingest: None,
        }
    }
}

/// Vault conventions for `cogs ingest` — where generated pages go and what
/// they're stamped with. Defaults match the karpathy three-layer scaffold;
/// OKF-style or custom vaults override. Runtime-only: excluded from the
/// config hash, so changing it never rebuilds the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IngestSection {
    /// Directory (under the notes root) for generated source pages.
    pub source_dir: String,
    /// `kind` stamped on generated source pages. Also what distill mines.
    pub source_kind: String,
    /// Directories the weave stage may propose new pages into, mapped to the
    /// kind each implies (BTreeMap: deterministic prompt order).
    pub new_pages: BTreeMap<String, String>,
    /// Audit-log file (relative to the notes root); empty disables logging.
    pub log_file: String,
    /// Value for the `owner:` frontmatter field on generated pages; empty
    /// omits the field.
    pub owner: String,
    /// Stamp `ingested_by: <model-id>` on generated pages (file-local
    /// provenance that survives moving the note between vaults).
    pub stamp_model: bool,
}

impl Default for IngestSection {
    fn default() -> Self {
        Self {
            source_dir: "sources".into(),
            source_kind: "source".into(),
            new_pages: BTreeMap::from([
                ("concepts".into(), "concept".into()),
                ("entities".into(), "entity".into()),
            ]),
            log_file: "log.md".into(),
            owner: "llm".into(),
            stamp_model: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    pub port: u16,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self { port: 7117 }
    }
}

impl Default for VaultConfig {
    /// Zero-config defaults for a plain Obsidian-style vault: every .md file
    /// is a note, body wikilinks become LINKS_TO edges, tags come from
    /// frontmatter and inline #tags.
    fn default() -> Self {
        Self {
            vault: VaultSection::default(),
            resources: None,
            notes: NotesSection::default(),
            kinds: KindsSection::default(),
            edges: vec![EdgeConfig {
                name: "LINKS_TO".into(),
                source: EdgeSource::Wikilinks,
                field: None,
                target: EdgeTarget::Note,
            }],
            tags: TagsSection::default(),
            diagnostics: DiagnosticsSection::default(),
            embeddings: EmbeddingsSection::default(),
            llm: LlmSection::default(),
            ingest: IngestSection::default(),
            server: ServerSection::default(),
        }
    }
}

/// A located, validated config plus the vault root it applies to.
#[derive(Debug, Clone)]
pub struct Vault {
    pub root: PathBuf,
    pub config: VaultConfig,
    /// Hash of (SCHEMA_VERSION, canonicalized config) — DB rebuild trigger.
    pub config_hash: String,
    /// Where derived state (graph.db, index-state.json, runtime/) lives.
    /// Defaults to <root>/.cogs; overridable so tooling can index a vault
    /// without writing into it.
    state_dir: PathBuf,
}

impl Vault {
    /// Walk up from `start` looking for cogs.toml / .cogs/config.toml.
    /// Falls back to zero-config defaults rooted at `start` itself.
    pub fn discover(start: &Path) -> Result<Self> {
        let start = start
            .canonicalize()
            .with_context(|| format!("vault path does not exist: {}", start.display()))?;
        for dir in start.ancestors() {
            for name in CONFIG_FILE_NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Self::load(dir, &candidate);
                }
            }
        }
        Ok(Self::from_config(start, VaultConfig::default())?)
    }

    pub fn load(root: &Path, config_path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        let config: VaultConfig = toml::from_str(&text)
            .with_context(|| format!("parsing {}", config_path.display()))?;
        Self::from_config(root.to_path_buf(), config)
    }

    pub fn from_config(root: PathBuf, config: VaultConfig) -> Result<Self> {
        config.validate()?;
        let config_hash = config.hash();
        let state_dir = root.join(".cogs");
        Ok(Self { root, config, config_hash, state_dir })
    }

    pub fn with_state_dir(mut self, dir: PathBuf) -> Self {
        self.state_dir = dir;
        self
    }

    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir().join("graph.db")
    }

    pub fn index_state_path(&self) -> PathBuf {
        self.state_dir().join("index-state.json")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.state_dir().join("runtime")
    }
}

impl VaultConfig {
    pub fn validate(&self) -> Result<()> {
        let name_re = regex::Regex::new(r"^[A-Z][A-Z0-9_]{0,30}$").unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut wikilink_edges = 0;
        for e in &self.edges {
            if !name_re.is_match(&e.name) {
                bail!("edge name {:?} must match ^[A-Z][A-Z0-9_]{{0,30}}$", e.name);
            }
            if e.name == "TAGGED" {
                bail!("edge name TAGGED is reserved for the tag relation");
            }
            if !seen.insert(&e.name) {
                bail!("duplicate edge name {:?}", e.name);
            }
            match e.source {
                EdgeSource::Wikilinks => {
                    wikilink_edges += 1;
                    if e.field.is_some() {
                        bail!("edge {:?}: 'field' is not valid with source=\"wikilinks\"", e.name);
                    }
                    if e.target == EdgeTarget::Resource {
                        bail!("edge {:?}: wikilink edges must target notes", e.name);
                    }
                }
                EdgeSource::Frontmatter => {
                    if e.field.as_deref().unwrap_or("").is_empty() {
                        bail!("edge {:?}: source=\"frontmatter\" requires 'field'", e.name);
                    }
                }
            }
        }
        if wikilink_edges > 1 {
            bail!("at most one edge may have source=\"wikilinks\"");
        }
        if self
            .edges
            .iter()
            .any(|e| e.target == EdgeTarget::Resource)
            && self.resources.is_none()
        {
            bail!("an edge targets resources but no [resources] section is configured");
        }
        Ok(())
    }

    /// The edge derived from body wikilinks (CITES in aoa-knowledge, LINKS_TO by default).
    pub fn wikilink_edge(&self) -> Option<&EdgeConfig> {
        self.edges.iter().find(|e| e.source == EdgeSource::Wikilinks)
    }

    pub fn frontmatter_edges(&self) -> impl Iterator<Item = &EdgeConfig> {
        self.edges.iter().filter(|e| e.source == EdgeSource::Frontmatter)
    }

    pub fn hash(&self) -> String {
        // Hash only schema-affecting config. Runtime-only sections (server
        // port, llm backend) are REMOVED from the hashed JSON — not reset to
        // defaults, which would still shift the hash whenever those structs
        // gain fields (learned the hard way: adding [llm].timeout_secs
        // rebuilt every vault). toml key-order noise is absorbed by
        // serde_json's stable struct field order.
        let mut v = serde_json::to_value(self).expect("config serializes");
        if let Some(o) = v.as_object_mut() {
            o.remove("llm");
            o.remove("ingest");
            o.remove("server");
        }
        let canonical = serde_json::to_string(&v).expect("config serializes");
        let mut h = Sha256::new();
        h.update(SCHEMA_VERSION.to_le_bytes());
        h.update(canonical.as_bytes());
        format!("{:x}", h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        VaultConfig::default().validate().unwrap();
    }

    #[test]
    fn default_has_linksto_wikilink_edge() {
        let cfg = VaultConfig::default();
        assert_eq!(cfg.wikilink_edge().unwrap().name, "LINKS_TO");
    }

    #[test]
    fn aoa_style_config_parses() {
        let cfg: VaultConfig = toml::from_str(
            r#"
            [vault]
            notes = ["wiki/**/*.md"]
            exclude = ["wiki/index.md", "wiki/log.md", "wiki/README.md", "wiki/_lint/**"]
            id_strip_prefix = "wiki/"

            [resources]
            paths = ["raw/**/*"]
            exclude = ["raw/README.md"]

            [kinds]
            values = ["concept", "entity", "position", "question", "source", "moc"]

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
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.wikilink_edge().unwrap().name, "CITES");
        assert_eq!(cfg.frontmatter_edges().count(), 2);
        assert_eq!(cfg.vault.id_strip_prefix, "wiki/");
    }

    #[test]
    fn rejects_bad_edge_names() {
        let mut cfg = VaultConfig::default();
        cfg.edges[0].name = "lower".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_two_wikilink_edges() {
        let mut cfg = VaultConfig::default();
        cfg.edges.push(EdgeConfig {
            name: "ALSO_LINKS".into(),
            source: EdgeSource::Wikilinks,
            field: None,
            target: EdgeTarget::Note,
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_hash_stable_and_sensitive() {
        let a = VaultConfig::default().hash();
        let b = VaultConfig::default().hash();
        assert_eq!(a, b);
        let mut cfg = VaultConfig::default();
        cfg.edges[0].name = "REFERS_TO".into();
        assert_ne!(a, cfg.hash());
    }

    #[test]
    fn llm_task_overlay_inherits_base() {
        let cfg: LlmSection = toml::from_str(
            r#"
            provider = "omlx"
            model = "base-model"
            base_url = "http://localhost:8000/v1"
            api_key_env = "OMLX_API_KEY"

            [ingest]
            model = "ingest-model"

            [ingest.extra_body]
            repetition_penalty = 1.1

            [ask]
            model = "ask-model"
            max_tokens = 4096
            "#,
        )
        .unwrap();
        let ingest = cfg.for_task("ingest");
        assert_eq!(ingest.model, "ingest-model");
        assert_eq!(ingest.base_url, "http://localhost:8000/v1"); // inherited
        assert_eq!(ingest.extra_body["repetition_penalty"], 1.1);
        let ask = cfg.for_task("ask");
        assert_eq!(ask.model, "ask-model");
        assert_eq!(ask.max_tokens, 4096);
        assert_eq!(ask.api_key_env, "OMLX_API_KEY"); // inherited
        // no overlay for other tasks → base
        assert_eq!(cfg.for_task("other").model, "base-model");
    }

    #[test]
    fn config_hash_ignores_llm_and_server_entirely() {
        let a = VaultConfig::default().hash();
        let mut cfg = VaultConfig::default();
        cfg.llm.model = "some-other-model".into();
        cfg.llm.timeout_secs = 999;
        cfg.llm.extra_body = serde_json::json!({ "chat_template_kwargs": { "enable_thinking": false } });
        cfg.server.port = 9999;
        cfg.ingest.source_dir = "captures".into();
        cfg.ingest.new_pages.insert("topics".into(), "Topic".into());
        assert_eq!(a, cfg.hash(), "runtime-only sections must never affect the hash");
    }
}
