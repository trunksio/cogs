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
        // canonical JSON of the config + schema version; toml round-trip noise
        // (key order) is absorbed by serde_json's stable struct field order.
        let canonical = serde_json::to_string(self).expect("config serializes");
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
}
