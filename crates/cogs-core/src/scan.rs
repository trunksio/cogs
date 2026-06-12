use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::config::Vault;

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("bad glob {p:?}"))?);
    }
    Ok(b.build()?)
}

/// Selects note and resource files under the vault root per the config globs.
pub struct VaultScanner {
    note_include: GlobSet,
    note_exclude: GlobSet,
    resource_include: GlobSet,
    resource_exclude: GlobSet,
    has_resources: bool,
}

impl VaultScanner {
    pub fn new(vault: &Vault) -> Result<Self> {
        let cfg = &vault.config;
        let (res_inc, res_exc, has_resources) = match &cfg.resources {
            Some(r) => (r.paths.clone(), r.exclude.clone(), !r.paths.is_empty()),
            None => (vec![], vec![], false),
        };
        Ok(Self {
            note_include: build_globset(&cfg.vault.notes)?,
            note_exclude: build_globset(&cfg.vault.exclude)?,
            resource_include: build_globset(&res_inc)?,
            resource_exclude: build_globset(&res_exc)?,
            has_resources,
        })
    }

    pub fn is_note(&self, rel_path: &str) -> bool {
        rel_path.ends_with(".md")
            && self.note_include.is_match(rel_path)
            && !self.note_exclude.is_match(rel_path)
    }

    /// `.meta.md` siblings are metadata carriers, not resources themselves.
    pub fn is_resource(&self, rel_path: &str) -> bool {
        self.has_resources
            && !rel_path.ends_with(".meta.md")
            && self.resource_include.is_match(rel_path)
            && !self.resource_exclude.is_match(rel_path)
    }

    /// Walk the vault, returning (note_paths, resource_paths), sorted,
    /// vault-relative with forward slashes.
    pub fn walk(&self, root: &Path) -> Result<(Vec<String>, Vec<String>)> {
        let mut notes = Vec::new();
        let mut resources = Vec::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                name != ".git" && name != ".cogs" && name != "node_modules"
            })
            .build();
        for entry in walker {
            let entry = entry?;
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if self.is_note(&rel) {
                notes.push(rel);
            } else if self.is_resource(&rel) {
                resources.push(rel);
            }
        }
        notes.sort();
        resources.sort();
        Ok((notes, resources))
    }
}

/// Locate the metadata markdown for a resource: the file itself when .md,
/// else `<stem>.meta.md` or `<full>.meta.md` sibling. Port of get_raw_meta_path.
pub fn resource_meta_path(root: &Path, rel_path: &str) -> Option<String> {
    if rel_path.ends_with(".md") {
        return Some(rel_path.to_string());
    }
    let stem_meta = match rel_path.rsplit_once('.') {
        Some((stem, _ext)) => format!("{stem}.meta.md"),
        None => format!("{rel_path}.meta.md"),
    };
    let full_meta = format!("{rel_path}.meta.md");
    for candidate in [stem_meta, full_meta] {
        if root.join(&candidate).is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Persisted per-file fingerprints powering incremental sync.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexState {
    pub config_hash: String,
    /// rel_path -> fingerprint, for notes and resources alike.
    pub files: BTreeMap<String, FileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub mtime_ms: u64,
    pub size: u64,
    /// sha256 of the raw file contents.
    pub content_hash: String,
}

impl IndexState {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Fingerprint a file. `fast_state` short-circuits hashing when (mtime, size)
/// already match the recorded state.
pub fn fingerprint(abs: &Path, fast_state: Option<&FileState>) -> Result<FileState> {
    let meta = std::fs::metadata(abs)?;
    let mtime_ms = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let size = meta.len();
    if let Some(prev) = fast_state {
        if prev.mtime_ms == mtime_ms && prev.size == size {
            return Ok(prev.clone());
        }
    }
    let bytes = std::fs::read(abs)?;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(FileState { mtime_ms, size, content_hash: format!("{:x}", h.finalize()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Vault, VaultConfig};

    #[test]
    fn scanner_classifies_paths() {
        let dir = std::env::temp_dir().join(format!("cogs-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: VaultConfig = toml::from_str(
            r#"
            [vault]
            notes = ["wiki/**/*.md"]
            exclude = ["wiki/index.md", "wiki/_lint/**"]
            id_strip_prefix = "wiki/"
            [resources]
            paths = ["raw/**/*"]
            exclude = ["raw/README.md"]
            "#,
        )
        .unwrap();
        let vault = Vault::from_config(dir.clone(), cfg).unwrap();
        let s = VaultScanner::new(&vault).unwrap();
        assert!(s.is_note("wiki/concepts/x.md"));
        assert!(!s.is_note("wiki/index.md"));
        assert!(!s.is_note("wiki/_lint/2026-01-01.md"));
        assert!(!s.is_note("raw/clips/y.md"));
        assert!(s.is_resource("raw/clips/y.md"));
        assert!(s.is_resource("raw/files/doc.pdf"));
        assert!(!s.is_resource("raw/files/doc.meta.md"));
        assert!(!s.is_resource("raw/README.md"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn meta_path_resolution() {
        let dir = std::env::temp_dir().join(format!("cogs-meta-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        std::fs::write(dir.join("raw/doc.meta.md"), "---\ntitle: t\n---\n").unwrap();
        assert_eq!(
            resource_meta_path(&dir, "raw/doc.pdf").as_deref(),
            Some("raw/doc.meta.md")
        );
        assert_eq!(resource_meta_path(&dir, "raw/note.md").as_deref(), Some("raw/note.md"));
        assert_eq!(resource_meta_path(&dir, "raw/orphan.png"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
