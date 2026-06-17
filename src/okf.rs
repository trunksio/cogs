//! OKF (Open Knowledge Format) v0.1 interoperability commands.
//!
//! COGS is both an OKF *consumer* (`okf import`, `okf lint`) and an OKF
//! *producer* (`okf export`). OKF represents knowledge as a directory of
//! markdown files: the file path is a concept's identity, plain markdown
//! `[text](path.md)` links form the graph, frontmatter carries the queryable
//! set (`type, title, description, resource, tags, timestamp`), and
//! `index.md` / `log.md` are reserved files.
//!
//! The interop layer is additive: native COGS vaults keep using `[[wikilinks]]`
//! and `kind`. Import applies an OKF-compatibility config profile; export
//! rewrites native conventions into OKF ones.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use cogs_core::config::{Vault, VaultConfig};
use cogs_core::parse::{split_frontmatter, yaml_to_json};
use cogs_graph::{GraphDb, SyncEngine};

/// The OKF-compatibility config profile applied to imported bundles. Mirrors
/// examples/okf.cogs.toml; kept here so `okf import` never needs to write a
/// config into the (possibly read-only / pristine) bundle.
pub const OKF_PROFILE_TOML: &str = include_str!("../templates/okf/cogs.toml");

/// OKF reserved (non-concept) filenames at any directory level.
const RESERVED: &[&str] = &["index.md", "log.md", "README.md"];

fn is_reserved(rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// `cogs okf import <path|git-url|tarball>`: materialize an OKF bundle locally
/// if needed, apply the OKF-compatibility profile, and index it into a graph
/// DB via the existing sync pipeline. The result is answerable via `cogs ask`
/// and browsable via `cogs viz`.
pub fn import(source: &str, state_dir: Option<&Path>) -> Result<()> {
    let materialized = materialize(source)?;
    let root = materialized.path().to_path_buf();

    // Apply the OKF-compat profile rather than whatever (if any) config the
    // bundle ships, so import semantics are consistent across bundles.
    let config: VaultConfig =
        toml::from_str(OKF_PROFILE_TOML).context("parsing the OKF compatibility profile")?;
    let mut vault = Vault::from_config(root.clone(), config)?;
    // Default the state dir outside the bundle so we never write into it.
    let state = state_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_import_state_dir(source, &root));
    vault = vault.with_state_dir(state);

    let engine = SyncEngine::new(&vault)?;
    let db = GraphDb::open_rw(&vault, true).context("opening graph db read-write")?;
    let out = engine.sync(&db, true)?;
    println!(
        "cogs okf import: indexed {} notes, {} edges from {}",
        out.notes_synced, out.edges_written, source
    );
    println!("graph db at {}", db.path().display());
    println!(
        "query it:  cogs --vault {} --config <okf.cogs.toml> --state-dir {} ask \"...\"",
        root.display(),
        vault.state_dir().display()
    );
    // Keep a temp checkout alive until here.
    materialized.keep_if_needed();
    Ok(())
}

/// A materialized bundle root. For directory sources this borrows the path in
/// place; for tarballs / git URLs it owns a temp dir that is cleaned up on
/// drop unless `keep_if_needed` decides otherwise.
enum Materialized {
    InPlace(PathBuf),
    Temp(tempfile::TempDir),
}

impl Materialized {
    fn path(&self) -> &Path {
        match self {
            Materialized::InPlace(p) => p,
            Materialized::Temp(d) => d.path(),
        }
    }
    /// Temp checkouts are dropped (deleted) at end of import — the graph DB in
    /// the state dir is the persistent artifact. Kept as a hook in case we
    /// later want to retain the materialized bundle.
    fn keep_if_needed(self) {}
}

fn materialize(source: &str) -> Result<Materialized> {
    let p = Path::new(source);
    if p.is_dir() {
        return Ok(Materialized::InPlace(p.canonicalize()?));
    }
    if is_tarball(source) {
        let dir = tempfile::tempdir().context("creating temp dir for tarball")?;
        extract_tarball(p, dir.path())?;
        let root = single_root(dir.path())?.unwrap_or_else(|| dir.path().to_path_buf());
        // If the tarball had a single top-level dir, descend into it.
        if root != dir.path() {
            return Ok(Materialized::Temp(reroot(dir, &root)?));
        }
        return Ok(Materialized::Temp(dir));
    }
    if looks_like_git_url(source) {
        let dir = tempfile::tempdir().context("creating temp dir for git clone")?;
        git_clone(source, dir.path())?;
        return Ok(Materialized::Temp(dir));
    }
    bail!(
        "unrecognized OKF source {source:?}: expected a directory, a .tar.gz/.tgz tarball, \
         or a git URL"
    );
}

fn is_tarball(source: &str) -> bool {
    let s = source.to_ascii_lowercase();
    s.ends_with(".tar.gz") || s.ends_with(".tgz") || s.ends_with(".tar")
}

fn looks_like_git_url(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.ends_with(".git")
}

fn extract_tarball(archive: &Path, into: &Path) -> Result<()> {
    if !archive.is_file() {
        bail!("tarball not found: {}", archive.display());
    }
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .context("running `tar` to extract the bundle (is tar installed?)")?;
    if !status.success() {
        bail!("tar failed to extract {}", archive.display());
    }
    Ok(())
}

fn git_clone(url: &str, into: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(into)
        .status()
        .context("running `git clone` (is git installed?)")?;
    if !status.success() {
        bail!("git clone failed for {url}");
    }
    Ok(())
}

/// If `dir` contains exactly one entry and it's a directory, return it (common
/// for tarballs that wrap everything in a top-level folder).
fn single_root(dir: &Path) -> Result<Option<PathBuf>> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.retain(|p| p.file_name().map(|n| n != ".DS_Store").unwrap_or(true));
    if entries.len() == 1 && entries[0].is_dir() {
        Ok(Some(entries.pop().unwrap()))
    } else {
        Ok(None)
    }
}

/// Build a TempDir handle rooted at a subdirectory. We keep the original temp
/// dir alive (so it isn't deleted) by leaking it and returning a fresh handle
/// over the inner path — both get cleaned via the OS temp dir lifetime.
fn reroot(outer: tempfile::TempDir, inner: &Path) -> Result<tempfile::TempDir> {
    // tempfile has no "subdir" handle; move the inner dir to a new temp dir.
    let dest = tempfile::tempdir()?;
    for entry in std::fs::read_dir(inner)? {
        let entry = entry?;
        let target = dest.path().join(entry.file_name());
        std::fs::rename(entry.path(), &target)
            .or_else(|_| copy_tree(&entry.path(), &target))?;
    }
    drop(outer);
    Ok(dest)
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

fn default_import_state_dir(source: &str, root: &Path) -> PathBuf {
    // For in-place dir imports keep state inside the bundle's .cogs; for temp
    // sources, store next to the cwd under .cogs-okf/<name>.
    let p = Path::new(source);
    if p.is_dir() {
        return root.join(".cogs");
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "bundle".into());
    PathBuf::from(".cogs-okf").join(name)
}

// ---------------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sev {
    Error,
    Warning,
}

#[derive(Debug)]
pub struct Finding {
    pub sev: Sev,
    pub file: String,
    pub message: String,
}

/// `cogs okf lint`: validate a vault against OKF v0.1 conformance — every
/// concept has a `type` field, reserved filenames are respected, frontmatter
/// is valid YAML, and cross-link targets resolve. Returns findings grouped by
/// severity; the caller decides exit status.
pub fn lint_vault(vault: &Vault) -> Result<Vec<Finding>> {
    use cogs_core::parse::parse_note;
    use cogs_core::resolve::{LinkResolver, Resolution};
    use cogs_core::scan::VaultScanner;

    let scanner = VaultScanner::new(vault)?;
    let (note_paths, _resources) = scanner.walk(&vault.root)?;

    // Build the resolver over the concept set.
    let strip = &vault.config.vault.id_strip_prefix;
    let pairs: Vec<(String, String)> = note_paths
        .iter()
        .map(|p| {
            let (id, slug, _) = cogs_core::parse::derive_ids(p, strip);
            (id, slug)
        })
        .collect();
    let resolver = LinkResolver::new(pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())));

    let mut findings = Vec::new();
    let type_field = &vault.config.notes.fields.kind;

    for rel in &note_paths {
        let text = std::fs::read_to_string(vault.root.join(rel))
            .with_context(|| format!("reading {rel}"))?;
        let (yaml, _, _, _) = split_frontmatter(&text);

        // Reserved files (index.md/log.md/README.md) are non-concept files and
        // need no `type`; only validate YAML if present.
        let reserved = is_reserved(rel);
        match yaml {
            None if !reserved => findings.push(Finding {
                sev: Sev::Error,
                file: rel.clone(),
                message: "missing YAML frontmatter (OKF concepts require `type`)".into(),
            }),
            None => {}
            Some(y) => {
                let json = yaml_to_json(y);
                if json.is_null() && !y.trim().is_empty() {
                    findings.push(Finding {
                        sev: Sev::Error,
                        file: rel.clone(),
                        message: "frontmatter is not valid YAML".into(),
                    });
                } else if !reserved
                    && json.get(type_field.as_str()).and_then(|v| v.as_str()).is_none()
                {
                    findings.push(Finding {
                        sev: Sev::Error,
                        file: rel.clone(),
                        message: format!("missing required `{type_field}` field"),
                    });
                }
            }
        }

        // Cross-link targets must resolve.
        let note = parse_note(rel, &text, &vault.config);
        let dir = note.dir.clone();
        for link in &note.md_links {
            match resolver.resolve(&link.target, &dir) {
                Resolution::Resolved(_) => {}
                Resolution::Ambiguous(c) => findings.push(Finding {
                    sev: Sev::Warning,
                    file: rel.clone(),
                    message: format!("ambiguous link to `{}` ({} candidates)", link.target, c.len()),
                }),
                Resolution::Broken => findings.push(Finding {
                    sev: Sev::Warning,
                    file: rel.clone(),
                    message: format!("link target does not resolve: `{}`", link.target),
                }),
            }
        }
    }

    Ok(findings)
}

/// Run lint and print a severity-grouped report. Returns true if conformant
/// (no errors).
pub fn lint(vault: &Vault) -> Result<bool> {
    let findings = lint_vault(vault)?;
    let errors: Vec<&Finding> = findings.iter().filter(|f| f.sev == Sev::Error).collect();
    let warnings: Vec<&Finding> = findings.iter().filter(|f| f.sev == Sev::Warning).collect();

    if !errors.is_empty() {
        println!("Errors ({}):", errors.len());
        for f in &errors {
            println!("  {} — {}", f.file, f.message);
        }
    }
    if !warnings.is_empty() {
        println!("Warnings ({}):", warnings.len());
        for f in &warnings {
            println!("  {} — {}", f.file, f.message);
        }
    }
    if errors.is_empty() && warnings.is_empty() {
        println!("OKF v0.1: conformant ✓");
    } else {
        println!(
            "OKF v0.1: {} error(s), {} warning(s)",
            errors.len(),
            warnings.len()
        );
    }
    Ok(errors.is_empty())
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// `cogs okf export [--out <dir|tarball>]`: emit the current native COGS vault
/// as a conformant OKF bundle — rewrite `kind`→`type`, convert each
/// `[[wikilink]]` to a relative markdown link `[label](path.md)`, ensure
/// `index.md`/`log.md`/`README.md` exist, and tar it when the output path ends
/// in `.tar.gz`/`.tgz`.
pub fn export(vault: &Vault, out: &Path) -> Result<()> {
    use cogs_core::parse::derive_ids;
    use cogs_core::resolve::LinkResolver;
    use cogs_core::scan::VaultScanner;

    let scanner = VaultScanner::new(vault)?;
    let (note_paths, _resources) = scanner.walk(&vault.root)?;
    let strip = &vault.config.vault.id_strip_prefix;

    // id -> output relative path (prefix-stripped so OKF identity == path).
    let mut id_to_out: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for rel in &note_paths {
        let (id, slug, _) = derive_ids(rel, strip);
        let out_rel = rel.strip_prefix(strip.as_str()).unwrap_or(rel).to_string();
        id_to_out.insert(id.clone(), out_rel);
        pairs.push((id, slug));
    }
    let resolver = LinkResolver::new(pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())));

    // Decide whether we stage to a temp dir (for tarball) or write directly.
    let tarball = matches!(
        out.extension().and_then(|e| e.to_str()),
        Some("gz") | Some("tgz")
    );
    let staging = tempfile::tempdir()?;
    let dest_dir: PathBuf = if tarball { staging.path().to_path_buf() } else { out.to_path_buf() };
    std::fs::create_dir_all(&dest_dir)?;

    let type_field = &vault.config.notes.fields.kind;
    let mut count = 0usize;
    for rel in &note_paths {
        let text = std::fs::read_to_string(vault.root.join(rel))
            .with_context(|| format!("reading {rel}"))?;
        let (id, _, dir) = derive_ids(rel, strip);
        let out_rel = id_to_out.get(&id).cloned().unwrap_or_else(|| rel.clone());
        let converted = convert_note_to_okf(&text, type_field, &dir, &resolver, &id_to_out);
        let out_path = dest_dir.join(&out_rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&out_path, converted)?;
        count += 1;
    }

    // Ensure reserved files exist.
    ensure_reserved(&dest_dir, &id_to_out)?;

    if tarball {
        make_tarball(&dest_dir, out)?;
        println!("cogs okf export: wrote {count} concepts to {}", out.display());
    } else {
        println!("cogs okf export: wrote {count} concepts to {}/", dest_dir.display());
    }
    Ok(())
}

/// Rewrite one note's text into OKF form: rename the `type`/`kind` frontmatter
/// key and convert `[[wikilinks]]` in the body into relative markdown links.
fn convert_note_to_okf(
    text: &str,
    type_field: &str,
    source_dir: &str,
    resolver: &cogs_core::resolve::LinkResolver,
    id_to_out: &std::collections::HashMap<String, String>,
) -> String {
    let (yaml, _range, body, _off) = split_frontmatter(text);

    // Rewrite the frontmatter line `kind:` -> `type:` when the native field
    // isn't already `type`. Operate line-wise to preserve the rest verbatim.
    let new_yaml = yaml.map(|y| {
        if type_field == "type" {
            return y.to_string();
        }
        y.lines()
            .map(|line| {
                let trimmed = line.trim_start();
                if let Some(rest) = trimmed.strip_prefix(&format!("{type_field}:")) {
                    let indent = &line[..line.len() - trimmed.len()];
                    format!("{indent}type:{rest}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    });

    let new_body = rewrite_wikilinks(body, source_dir, resolver, id_to_out);

    match new_yaml {
        Some(y) => format!("---\n{}\n---\n{}", y.trim_end_matches('\n'), new_body),
        None => new_body,
    }
}

/// Replace `[[target]]` / `[[target|alias]]` / `[[target#anchor]]` with a
/// relative markdown link `[label](relative/path.md)`. Unresolved targets are
/// left as their display text (no dangling links in the OKF output).
fn rewrite_wikilinks(
    body: &str,
    source_dir: &str,
    resolver: &cogs_core::resolve::LinkResolver,
    id_to_out: &std::collections::HashMap<String, String>,
) -> String {
    use cogs_core::resolve::Resolution;
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\[\[([^\]\n]+?)\]\]").unwrap()
    });
    RE.replace_all(body, |caps: &regex::Captures| {
        let chunk = &caps[1];
        let (target_part, alias) = match chunk.split_once('|') {
            Some((t, a)) => (t, Some(a.trim().to_string())),
            None => (chunk, None),
        };
        let target = target_part.split('#').next().unwrap_or(target_part).trim();
        let label = alias.unwrap_or_else(|| {
            target.rsplit('/').next().unwrap_or(target).to_string()
        });
        match resolver.resolve(target, source_dir) {
            Resolution::Resolved(id) => {
                if let Some(dest) = id_to_out.get(&id) {
                    let rel = relative_link(source_dir, dest);
                    format!("[{label}]({rel})")
                } else {
                    label.clone()
                }
            }
            _ => label.clone(),
        }
    })
    .to_string()
}

/// Compute a relative markdown link from a source note's directory to a
/// destination output path (both bundle-relative, forward slashes).
fn relative_link(source_dir: &str, dest: &str) -> String {
    let src_segs: Vec<&str> =
        if source_dir.is_empty() { vec![] } else { source_dir.split('/').collect() };
    let dest_segs: Vec<&str> = dest.split('/').collect();
    // Common prefix.
    let mut i = 0;
    while i < src_segs.len() && i + 1 < dest_segs.len() && src_segs[i] == dest_segs[i] {
        i += 1;
    }
    let ups = src_segs.len() - i;
    let mut parts: Vec<String> = std::iter::repeat("..".to_string()).take(ups).collect();
    parts.extend(dest_segs[i..].iter().map(|s| s.to_string()));
    let joined = parts.join("/");
    if joined.is_empty() {
        dest.to_string()
    } else {
        joined
    }
}

fn ensure_reserved(
    dir: &Path,
    id_to_out: &std::collections::HashMap<String, String>,
) -> Result<()> {
    if !dir.join("index.md").exists() {
        let mut body = String::from(
            "---\ntype: index\ntitle: Index\ndescription: Entry point for this OKF bundle.\n---\n\n# Index\n\n",
        );
        let mut paths: Vec<&String> = id_to_out.values().collect();
        paths.sort();
        for p in paths {
            let label = p.strip_suffix(".md").unwrap_or(p);
            body.push_str(&format!("- [{label}]({p})\n"));
        }
        std::fs::write(dir.join("index.md"), body)?;
    }
    if !dir.join("log.md").exists() {
        std::fs::write(
            dir.join("log.md"),
            "---\ntype: log\ntitle: Log\ndescription: Append-only change history.\n---\n\n# Log\n\n## Exported by cogs okf export\n",
        )?;
    }
    if !dir.join("README.md").exists() {
        std::fs::write(dir.join("README.md"), include_str!("../templates/okf/README.md"))?;
    }
    Ok(())
}

fn make_tarball(dir: &Path, out: &Path) -> Result<()> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let out_abs = if out.is_absolute() {
        out.to_path_buf()
    } else {
        std::env::current_dir()?.join(out)
    };
    // Tar the staging dir's *contents* (so the bundle has no extra top folder).
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&out_abs)
        .arg("-C")
        .arg(dir)
        .arg(".")
        .status()
        .context("running `tar` to create the bundle")?;
    if !status.success() {
        bail!("tar failed to create {}", out.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cogs_core::resolve::LinkResolver;
    use std::collections::HashMap;

    fn resolver_with(ids: &[&str]) -> (LinkResolver, HashMap<String, String>) {
        // slug = last path segment; out path = id-with-slashes + .md
        let pairs: Vec<(String, String)> = ids
            .iter()
            .map(|id| {
                let slug = id.rsplit('-').next().unwrap_or(id).to_string();
                (id.to_string(), slug)
            })
            .collect();
        let resolver = LinkResolver::new(pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())));
        let mut id_to_out = HashMap::new();
        for id in ids {
            id_to_out.insert(id.to_string(), format!("{}.md", id.replace('-', "/")));
        }
        (resolver, id_to_out)
    }

    #[test]
    fn reserved_files_detected() {
        assert!(is_reserved("index.md"));
        assert!(is_reserved("log.md"));
        assert!(is_reserved("README.md"));
        assert!(is_reserved("sub/dir/Index.MD".to_ascii_lowercase().as_str()));
        assert!(!is_reserved("concepts/alpha.md"));
    }

    #[test]
    fn relative_link_climbs_and_descends() {
        // same dir
        assert_eq!(relative_link("concepts", "concepts/beta.md"), "beta.md");
        // up one then into sibling dir
        assert_eq!(relative_link("concepts", "entities/x.md"), "../entities/x.md");
        // root source into subdir
        assert_eq!(relative_link("", "concepts/alpha.md"), "concepts/alpha.md");
    }

    #[test]
    fn wikilinks_rewritten_to_markdown() {
        let (resolver, map) = resolver_with(&["concepts-beta"]);
        let body = "See [[beta]] and [[concepts/beta|the second]].";
        let out = rewrite_wikilinks(body, "concepts", &resolver, &map);
        assert_eq!(out, "See [beta](beta.md) and [the second](beta.md).");
    }

    #[test]
    fn unresolved_wikilink_falls_back_to_label() {
        let (resolver, map) = resolver_with(&["concepts-beta"]);
        let out = rewrite_wikilinks("ref [[ghost|missing]] here", "concepts", &resolver, &map);
        assert_eq!(out, "ref missing here");
    }

    #[test]
    fn convert_renames_kind_to_type() {
        let (resolver, map) = resolver_with(&["concepts-beta"]);
        let text = "---\nkind: concept\ntitle: Alpha\n---\nBody [[beta]].\n";
        let out = convert_note_to_okf(text, "kind", "concepts", &resolver, &map);
        assert!(out.contains("type: concept"));
        assert!(!out.contains("kind: concept"));
        assert!(out.contains("[beta](beta.md)"));
    }

    #[test]
    fn convert_leaves_type_field_untouched() {
        let (resolver, map) = resolver_with(&[]);
        let text = "---\ntype: concept\ntitle: X\n---\nNo links.\n";
        let out = convert_note_to_okf(text, "type", "", &resolver, &map);
        assert!(out.contains("type: concept"));
    }
}
