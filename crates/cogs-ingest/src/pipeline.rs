//! The ingest stage machine: preflight → extract → (near-dup check) →
//! materialise → sync. Weaving (link suggestion, page updates, contradiction
//! checks) lands in milestone 2.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::json;
use tracing::{info, warn};

use cogs_ask::query::cypher_escape;
use cogs_core::config::{EdgeConfig, EdgeTarget, Vault};
use cogs_core::note::ParsedResource;
use cogs_core::parse::{parse_resource, sha256_hex};
use cogs_core::scan::VaultScanner;
use cogs_graph::embed::EmbeddingProvider;
use cogs_graph::{GraphDb, SyncEngine};
use cogs_llm::ChatProvider;

use crate::retrieve::{self, truncate_chars, NearDuplicate};
use crate::training::{
    RunManifest, TaskKind, Teacher, TrainingRecorder, WriteKind, WriteRecord,
};
use crate::{git, prompts, render, ContradictionFinding, Extraction};

/// Bodies longer than this are chunked at `##` boundaries for extraction.
const CHUNK_CHARS: usize = 28_000;
const MAX_CLAIMS: usize = 12;
const MAX_QUOTES: usize = 6;

pub struct IngestOptions {
    pub force: bool,
    pub dry_run: bool,
    /// Max existing pages to draft updates for (weave stage, milestone 2).
    pub pages_cap: usize,
    /// Capture training records (off with --no-training-capture).
    pub capture: bool,
    /// Override for the training-record dir (default <state_dir>/training).
    pub training_dir: Option<PathBuf>,
    /// Injectable "today" so tests are deterministic.
    pub today: NaiveDate,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            force: false,
            dry_run: false,
            pages_cap: 8,
            capture: true,
            training_dir: None,
            today: chrono::Utc::now().date_naive(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedWriteView {
    pub rel_path: String,
    /// "create" | "append"
    pub action: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct IngestReport {
    pub run_id: String,
    pub raw_path: String,
    /// Set when the raw file already has a source page: nothing was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_ingested: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_page: Option<String>,
    pub pages_updated: Vec<String>,
    pub pages_created: Vec<String>,
    pub contradictions: Vec<ContradictionFinding>,
    pub near_duplicates: Vec<NearDuplicate>,
    pub warnings: Vec<String>,
    pub training_records: u32,
    pub dry_run: bool,
    /// Whether the graph was re-synced after writing (false = a running cogs
    /// process holds the writer and will pick the files up, or sync failed).
    pub synced: bool,
    /// Full planned writes; populated on --dry-run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub planned: Vec<PlannedWriteView>,
}

enum Action {
    /// Target must not exist.
    Create,
    /// Appended to the end (file created if missing).
    Append,
}

struct PlannedWrite {
    rel_path: String,
    action: Action,
    content: String,
    seq: Option<u32>,
    section_heading: Option<String>,
}

pub struct Ingester<'a> {
    vault: &'a Vault,
    db: &'a GraphDb,
    chat: &'a dyn ChatProvider,
    embed: Option<&'a dyn EmbeddingProvider>,
    opts: IngestOptions,
}

impl<'a> Ingester<'a> {
    pub fn new(
        vault: &'a Vault,
        db: &'a GraphDb,
        chat: &'a dyn ChatProvider,
        embed: Option<&'a dyn EmbeddingProvider>,
        opts: IngestOptions,
    ) -> Self {
        Self { vault, db, chat, embed, opts }
    }

    pub fn ingest(&self, raw_file: &Path) -> Result<IngestReport> {
        let mut warnings: Vec<String> = Vec::new();

        // ---- stage 0: preflight ------------------------------------------
        let raw_rel = self.normalise(raw_file)?;
        let source_edge = self.source_edge()?;
        let scanner = VaultScanner::new(self.vault)?;
        if !scanner.is_resource(&raw_rel) {
            bail!(
                "{raw_rel} is not a resource — check [resources].paths/exclude in cogs.toml"
            );
        }
        if !raw_rel.ends_with(".md") {
            bail!("only markdown raw captures are supported for now: {raw_rel}");
        }
        let text = std::fs::read_to_string(self.vault.root.join(&raw_rel))
            .with_context(|| format!("reading {raw_rel}"))?;
        let raw = parse_resource(&raw_rel, &text, true, &self.vault.config);
        if raw.body_text.trim().is_empty() {
            bail!("{raw_rel} has no body text to ingest");
        }

        let prefix = self.vault.config.vault.id_strip_prefix.clone(); // "wiki/" or ""
        let scope = prefix.trim_end_matches('/');
        warnings.extend(git::ensure_clean(
            &self.vault.root,
            (!scope.is_empty()).then_some(scope),
            self.opts.force,
        )?);

        let run_id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            &raw.body_hash[..6.min(raw.body_hash.len())]
        );

        if let Some(existing) = self.already_ingested(&source_edge.name, &raw_rel)? {
            info!("{raw_rel} already ingested: {existing}");
            return Ok(IngestReport {
                run_id,
                raw_path: raw_rel,
                already_ingested: Some(existing),
                source_page: None,
                pages_updated: vec![],
                pages_created: vec![],
                contradictions: vec![],
                near_duplicates: vec![],
                warnings,
                training_records: 0,
                dry_run: self.opts.dry_run,
                synced: false,
                planned: vec![],
            });
        }

        // ---- training capture setup --------------------------------------
        let training_dir = self
            .opts
            .training_dir
            .clone()
            .unwrap_or_else(|| self.vault.state_dir().join("training"));
        let recorder = (self.opts.capture && !self.opts.dry_run).then(|| {
            TrainingRecorder::new(
                training_dir.join("runs"),
                &run_id,
                self.chat.name(),
                &self.vault.config.llm.model,
            )
        });
        let teacher = Teacher::new(self.chat, recorder.as_ref());

        // ---- stage 1: extract (teacher) -----------------------------------
        let (extraction, extract_seq) = self.extract(&teacher, &raw, &raw_rel)?;
        let (extraction, val_warnings) = self.validate_extraction(extraction, &raw, &prefix)?;
        warnings.extend(val_warnings);
        info!(
            claims = extraction.key_claims.len(),
            quotes = extraction.quotes.len(),
            entities = extraction.entities.len(),
            slug = %extraction.suggested_slug,
            "extraction validated"
        );

        // ---- stage 2: near-duplicate check (advisory) ---------------------
        let near_duplicates = retrieve::near_duplicates(
            self.db,
            self.embed,
            &extraction.summary,
            &raw.body_text,
            self.vault.config.embeddings.char_cap as usize,
        )
        .unwrap_or_else(|e| {
            warn!("near-duplicate check failed: {e:#}");
            vec![]
        });
        if !near_duplicates.is_empty() {
            warnings.push(format!(
                "possible duplicate of existing source page(s): {} — consider folding \
                 this capture in manually instead",
                near_duplicates
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // ---- stage 4: materialise -----------------------------------------
        // (stage 3, weaving, lands in milestone 2)
        let slug = &extraction.suggested_slug;
        let source_rel = format!("{prefix}sources/{slug}.md");
        let contradictions: Vec<ContradictionFinding> = vec![];
        let page_md =
            render::source_page(&extraction, &raw, &raw_rel, self.opts.today, &contradictions);
        let log_rel = format!("{prefix}log.md");
        let log_md = render::log_entry(
            self.opts.today,
            &raw.title,
            &raw_rel,
            slug,
            &[],
            &[],
            &contradictions,
            &near_duplicates,
            &run_id,
            &self.vault.config.llm.model,
        );

        let writes = vec![
            PlannedWrite {
                rel_path: source_rel.clone(),
                action: Action::Create,
                content: page_md,
                seq: Some(extract_seq),
                section_heading: None,
            },
            PlannedWrite {
                rel_path: log_rel,
                action: Action::Append,
                content: log_md,
                seq: None,
                section_heading: Some(format!("## [{}] ingest | {}", self.opts.today, raw.title)),
            },
        ];

        if self.opts.dry_run {
            return Ok(IngestReport {
                run_id,
                raw_path: raw_rel,
                already_ingested: None,
                source_page: Some(source_rel),
                pages_updated: vec![],
                pages_created: vec![],
                contradictions,
                near_duplicates,
                warnings,
                training_records: 0,
                dry_run: true,
                synced: false,
                planned: writes
                    .iter()
                    .map(|w| PlannedWriteView {
                        rel_path: w.rel_path.clone(),
                        action: match w.action {
                            Action::Create => "create".into(),
                            Action::Append => "append".into(),
                        },
                        content: w.content.clone(),
                    })
                    .collect(),
            });
        }

        let write_records = self.flush(&writes, &prefix)?;

        // ---- stage 5: reindex + manifest -----------------------------------
        let synced = self.resync();

        if let Some(rec) = &recorder {
            rec.finish(&RunManifest {
                run_id: run_id.clone(),
                created: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                raw_rel_path: raw_rel.clone(),
                raw_body_hash: raw.body_hash.clone(),
                provider: self.chat.name().to_string(),
                model: self.vault.config.llm.model.clone(),
                writes: write_records,
            })?;
        }

        Ok(IngestReport {
            run_id,
            raw_path: raw_rel,
            already_ingested: None,
            source_page: Some(source_rel),
            pages_updated: vec![],
            pages_created: vec![],
            contradictions,
            near_duplicates,
            warnings,
            training_records: recorder.as_ref().map(|r| r.count()).unwrap_or(0),
            dry_run: false,
            synced,
            planned: vec![],
        })
    }

    // ---- preflight helpers ------------------------------------------------

    fn normalise(&self, p: &Path) -> Result<String> {
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            let vault_rel = self.vault.root.join(p);
            if vault_rel.exists() {
                vault_rel
            } else {
                std::env::current_dir()?.join(p)
            }
        };
        let abs = candidate
            .canonicalize()
            .with_context(|| format!("raw file not found: {}", p.display()))?;
        let root = self.vault.root.canonicalize()?;
        let rel = abs
            .strip_prefix(&root)
            .map_err(|_| anyhow::anyhow!("{} is outside the vault", abs.display()))?;
        Ok(rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
    }

    fn source_edge(&self) -> Result<&EdgeConfig> {
        self.vault
            .config
            .frontmatter_edges()
            .find(|e| e.target == EdgeTarget::Resource)
            .context(
                "ingest needs a source layer: configure [resources] and a frontmatter edge \
                 with target = \"resource\" (e.g. source_refs → SOURCE_OF) in cogs.toml",
            )
    }

    fn already_ingested(&self, edge: &str, raw_rel: &str) -> Result<Option<String>> {
        let q = format!(
            "MATCH (n:Note)-[:{edge}]->(r:Resource {{path: '{}'}}) RETURN n.path AS path",
            cypher_escape(raw_rel)
        );
        let rows = self.db.query_json(&q).unwrap_or_default();
        Ok(rows.first().and_then(|r| r["path"].as_str()).map(str::to_string))
    }

    // ---- stage 1 -----------------------------------------------------------

    fn extract(
        &self,
        teacher: &Teacher,
        raw: &ParsedResource,
        raw_rel: &str,
    ) -> Result<(Extraction, u32)> {
        let max_tokens = self.vault.config.llm.max_tokens;
        let params = prompts::extract_params(max_tokens);
        let chunks = chunk_body(&raw.body_text, CHUNK_CHARS);
        if chunks.len() == 1 {
            return teacher.call(
                TaskKind::Extract,
                json!({ "raw_path": raw_rel }),
                &prompts::extract_messages(&raw.title, raw.url.as_deref(), chunks[0]),
                &params,
            );
        }

        info!(chunks = chunks.len(), "long capture: extracting per section, then merging");
        let mut parts = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let (part, _seq): (Extraction, u32) = teacher.call(
                TaskKind::ExtractChunk,
                json!({ "raw_path": raw_rel, "chunk": i + 1, "of": chunks.len() }),
                &prompts::extract_messages(&raw.title, raw.url.as_deref(), chunk),
                &params,
            )?;
            parts.push(serde_json::to_string(&part)?);
        }
        teacher.call(
            TaskKind::ExtractMerge,
            json!({ "raw_path": raw_rel, "parts": parts.len() }),
            &prompts::merge_messages(&parts),
            &params,
        )
    }

    /// Enforce the hard rules on model output. Fails only when unsalvageable
    /// (no summary / no claims); everything else degrades with a warning.
    fn validate_extraction(
        &self,
        mut ex: Extraction,
        raw: &ParsedResource,
        prefix: &str,
    ) -> Result<(Extraction, Vec<String>)> {
        let mut warnings = Vec::new();

        ex.summary = ex.summary.trim().to_string();
        if ex.summary.is_empty() {
            bail!("extraction produced no summary — aborting (nothing usable to write)");
        }

        // Claims: single-line, non-empty, deduped, capped.
        let mut seen = std::collections::HashSet::new();
        ex.key_claims.retain_mut(|c| {
            c.text = c.text.split_whitespace().collect::<Vec<_>>().join(" ");
            !c.text.is_empty() && seen.insert(c.text.to_lowercase())
        });
        if ex.key_claims.is_empty() {
            bail!("extraction produced no key claims — aborting");
        }
        if ex.key_claims.len() > MAX_CLAIMS {
            warnings.push(format!(
                "model produced {} claims; keeping the first {MAX_CLAIMS}",
                ex.key_claims.len()
            ));
            ex.key_claims.truncate(MAX_CLAIMS);
        }

        // Quotes must be verbatim (whitespace-tolerant) substrings of the raw
        // body; the recovered raw slice replaces the model's rendition.
        ex.quotes = ex
            .quotes
            .drain(..)
            .filter_map(|mut q| match find_verbatim(&raw.body_text, &q.text) {
                Some(exact) => {
                    q.text = exact;
                    Some(q)
                }
                None => {
                    warnings.push(format!(
                        "dropped non-verbatim quote: {:?}",
                        truncate_chars(&q.text, 80)
                    ));
                    None
                }
            })
            .collect();
        ex.quotes.truncate(MAX_QUOTES);

        // Tags: lowercase tokens.
        let mut seen_tags = std::collections::HashSet::new();
        ex.tags.retain_mut(|t| {
            *t = t.trim().to_lowercase().replace(' ', "-");
            !t.is_empty() && seen_tags.insert(t.clone())
        });
        ex.tags.truncate(6);

        // Slug: well-formed, non-colliding with existing files.
        let slug_re = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{1,60}$").unwrap();
        if !slug_re.is_match(&ex.suggested_slug) {
            let fallback = slug_from_filename(&raw.rel_path);
            if !ex.suggested_slug.is_empty() {
                warnings.push(format!(
                    "model slug {:?} is malformed; using {:?}",
                    ex.suggested_slug, fallback
                ));
            }
            ex.suggested_slug = fallback;
        }
        let base = ex.suggested_slug.clone();
        let mut n = 1;
        while self.vault.root.join(format!("{prefix}sources/{}.md", ex.suggested_slug)).exists() {
            n += 1;
            ex.suggested_slug = format!("{base}-{n}");
        }
        if n > 1 {
            warnings.push(format!("slug {base:?} taken; using {:?}", ex.suggested_slug));
        }

        Ok((ex, warnings))
    }

    // ---- stage 4/5 ----------------------------------------------------------

    fn flush(&self, writes: &[PlannedWrite], prefix: &str) -> Result<Vec<WriteRecord>> {
        // Invariants: everything under the note tree, never under the
        // resource layer, creates never clobber.
        let scanner = VaultScanner::new(self.vault)?;
        for w in writes {
            if !prefix.is_empty() && !w.rel_path.starts_with(prefix) {
                bail!("refusing to write outside the note tree: {}", w.rel_path);
            }
            if scanner.is_resource(&w.rel_path) {
                bail!("refusing to write into the resource layer: {}", w.rel_path);
            }
            if matches!(w.action, Action::Create)
                && self.vault.root.join(&w.rel_path).exists()
            {
                bail!("refusing to overwrite existing file: {}", w.rel_path);
            }
        }

        let mut records = Vec::new();
        for w in writes {
            let abs = self.vault.root.join(&w.rel_path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match w.action {
                Action::Create => {
                    std::fs::write(&abs, &w.content)
                        .with_context(|| format!("writing {}", w.rel_path))?;
                    info!("created {}", w.rel_path);
                    records.push(WriteRecord {
                        rel_path: w.rel_path.clone(),
                        kind: WriteKind::Created,
                        section_heading: w.section_heading.clone(),
                        content_hash: sha256_hex(&w.content),
                        seq: w.seq,
                    });
                }
                Action::Append => {
                    let existing = std::fs::read_to_string(&abs).unwrap_or_default();
                    let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
                    std::fs::write(&abs, format!("{existing}{sep}{}", w.content))
                        .with_context(|| format!("appending to {}", w.rel_path))?;
                    info!("appended to {}", w.rel_path);
                    records.push(WriteRecord {
                        rel_path: w.rel_path.clone(),
                        kind: WriteKind::Appended,
                        section_heading: w.section_heading.clone(),
                        content_hash: sha256_hex(&w.content),
                        seq: w.seq,
                    });
                }
            }
        }
        Ok(records)
    }

    /// Re-index the files we just wrote when we hold the writer; a running
    /// cogs process (LSP/MCP primary) picks them up via its watcher otherwise.
    fn resync(&self) -> bool {
        if self.db.is_read_only() {
            info!("graph is read-only here — a running cogs process will re-index shortly");
            return false;
        }
        match SyncEngine::new(self.vault)
            .and_then(|e| e.sync_with(self.db, false, self.embed))
        {
            Ok(_) => true,
            Err(e) => {
                warn!("post-ingest sync failed (run `cogs sync`): {e:#}");
                false
            }
        }
    }
}

// ---- pure helpers (unit-tested) --------------------------------------------

/// Split a long body into chunks of at most `cap` bytes, preferring `\n## `
/// section boundaries, hard-splitting oversized sections at char boundaries.
fn chunk_body(body: &str, cap: usize) -> Vec<&str> {
    if body.len() <= cap {
        return vec![body];
    }
    let mut sections: Vec<&str> = Vec::new();
    let mut start = 0;
    for (idx, _) in body.match_indices("\n## ") {
        if idx + 1 > start {
            sections.push(&body[start..idx + 1]);
            start = idx + 1;
        }
    }
    sections.push(&body[start..]);

    // Hard-split any single section that still exceeds the cap.
    let mut pieces: Vec<&str> = Vec::new();
    for s in sections {
        let mut rest = s;
        while rest.len() > cap {
            let mut end = cap;
            while end > 0 && !rest.is_char_boundary(end) {
                end -= 1;
            }
            pieces.push(&rest[..end]);
            rest = &rest[end..];
        }
        if !rest.is_empty() {
            pieces.push(rest);
        }
    }

    // Greedily pack adjacent pieces back up to the cap. Chunks are ranges of
    // the original body, so we can return slices.
    let mut chunks: Vec<&str> = Vec::new();
    let base = body.as_ptr() as usize;
    let mut cur_start: Option<usize> = None;
    let mut cur_end = 0usize;
    for p in pieces {
        let p_start = p.as_ptr() as usize - base;
        let p_end = p_start + p.len();
        match cur_start {
            Some(s) if p_end - s <= cap => cur_end = p_end,
            Some(s) => {
                chunks.push(&body[s..cur_end]);
                cur_start = Some(p_start);
                cur_end = p_end;
            }
            None => {
                cur_start = Some(p_start);
                cur_end = p_end;
            }
        }
    }
    if let Some(s) = cur_start {
        chunks.push(&body[s..cur_end]);
    }
    chunks.retain(|c| !c.trim().is_empty());
    chunks
}

/// Whitespace-collapse `s`, tracking each normalized char's source byte offset.
fn normalized_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut map = Vec::new();
    let mut last_ws = true;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if !last_ws {
                out.push(' ');
                map.push(i);
                last_ws = true;
            }
        } else {
            out.push(ch);
            map.push(i);
            last_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// If `needle` appears in `haystack` up to whitespace differences, return the
/// exact haystack slice (trimmed) — recovering the verbatim quote even when
/// the model mangled line breaks.
fn find_verbatim(haystack: &str, needle: &str) -> Option<String> {
    let (h_norm, map) = normalized_with_map(haystack);
    let (n_norm, _) = normalized_with_map(needle);
    if n_norm.is_empty() {
        return None;
    }
    let pos = h_norm.find(&n_norm)?;
    let start_char = h_norm[..pos].chars().count();
    let end_char = start_char + n_norm.chars().count();
    let src_start = *map.get(start_char)?;
    let src_last = *map.get(end_char - 1)?;
    let last_len = haystack[src_last..].chars().next()?.len_utf8();
    Some(haystack[src_start..src_last + last_len].trim().to_string())
}

/// Fallback slug from the raw filename: strip the date prefix and extension,
/// squash anything non-slug.
fn slug_from_filename(rel_path: &str) -> String {
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .trim_end_matches(".md");
    let date_re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}-").unwrap();
    let stem = date_re.replace(stem, "");
    let mut slug: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.len() < 2 {
        "capture".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_body_is_one_chunk() {
        assert_eq!(chunk_body("hello", 100), vec!["hello"]);
    }

    #[test]
    fn long_body_splits_at_headings_and_packs() {
        let body = format!(
            "intro {}\n## A\n{}\n## B\n{}",
            "x".repeat(30),
            "a".repeat(40),
            "b".repeat(40)
        );
        let chunks = chunk_body(&body, 80);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| c.len() <= 80));
        // nothing lost
        assert_eq!(chunks.concat(), body);
        // section boundary respected: a chunk starts with the heading
        assert!(chunks.iter().any(|c| c.starts_with("## B\n")));
    }

    #[test]
    fn oversized_section_hard_splits() {
        let body = format!("## only\n{}", "y".repeat(300));
        let chunks = chunk_body(&body, 100);
        assert!(chunks.iter().all(|c| c.len() <= 100));
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn find_verbatim_tolerates_whitespace() {
        let hay = "The quick\n  brown   fox\njumps.";
        assert_eq!(
            find_verbatim(hay, "quick brown fox jumps."),
            Some("quick\n  brown   fox\njumps.".to_string())
        );
        assert_eq!(find_verbatim(hay, "quick red fox"), None);
    }

    #[test]
    fn find_verbatim_handles_multibyte() {
        let hay = "héllo wörld — done";
        assert_eq!(find_verbatim(hay, "wörld — done"), Some("wörld — done".to_string()));
    }

    #[test]
    fn slug_fallback_strips_date_and_ext() {
        assert_eq!(
            slug_from_filename("raw/clips/2026-07-03-Some Article!.md"),
            "some-article"
        );
        assert_eq!(slug_from_filename("raw/x.md"), "capture");
    }
}
