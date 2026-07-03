//! `cogs distill` — mine the vault and captured ingest runs into an SFT
//! dataset for the small local ingest model.
//!
//! Two sources of pairs, emitted under the SAME prompt templates the runtime
//! pipeline uses (prompts.rs), so the fine-tuned student is a drop-in
//! replacement for the teacher:
//!
//! - **Mined from the vault**: every accepted source page whose single
//!   `source_refs` raw file still exists yields an `extract` pair (raw body →
//!   reconstructed Extraction) and a `suggest_links` pair (plain claims +
//!   rebuilt candidates → the accepted wikilinked claims). This works today,
//!   with zero recorded runs.
//! - **`--from-runs`**: recorded teacher calls paired with the SURVIVING file
//!   content — human post-review edits become the labels; deleted pages and
//!   removed sections mean rejection and are skipped. These pairs carry the
//!   true original inputs (candidates as seen at ingest time) and win over
//!   mined pairs for the same page.
//!
//! Output: `train.jsonl` / `valid.jsonl` in chat format
//! (`{"messages": [...]}`), the directory layout `mlx_lm.lora --data` expects.
//! The split is deterministic by hash of a per-pair key.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use cogs_core::config::{EdgeTarget, Vault};
use cogs_core::parse::{
    derive_ids, parse_note, parse_resource, scan_wikilinks, split_frontmatter,
    strip_wikilinks_for_fts, yaml_to_json,
};
use cogs_core::scan::VaultScanner;
use cogs_graph::embed::EmbeddingProvider;
use cogs_graph::GraphDb;
use cogs_llm::Message;

use crate::retrieve::{self, NoteMeta};
use crate::text::{bullet_items, find_verbatim, normalize, section};
use crate::training::{RunManifest, TaskKind, TrainingRecord, WriteKind};
use crate::{prompts, Claim, ContradictionCheck, EntityMention, Extraction, LinkPlan, Quote};

pub struct DistillOptions {
    /// Output dir (default `<training_dir>/dataset`).
    pub out: Option<PathBuf>,
    /// Validation fraction (deterministic by pair key).
    pub split: f64,
    /// Task filter: extract | suggest_links | page_update | contradiction.
    pub tasks: Option<Vec<String>>,
    /// Also mine captured ingest runs.
    pub from_runs: bool,
    /// Where run records live (default `<state_dir>/training`).
    pub training_dir: Option<PathBuf>,
}

impl Default for DistillOptions {
    fn default() -> Self {
        Self { out: None, split: 0.1, tasks: None, from_runs: false, training_dir: None }
    }
}

#[derive(Debug, Serialize)]
pub struct DistillStats {
    pub emitted: BTreeMap<String, usize>,
    pub skipped: BTreeMap<String, usize>,
    pub train: usize,
    pub valid: usize,
    pub out_dir: PathBuf,
}

struct Pair {
    /// "extract" | "suggest_links" | "page_update" | "contradiction"
    task: &'static str,
    /// Dedup + split key (task-scoped).
    key: String,
    messages: Vec<Message>,
    target_json: String,
}

pub fn distill(
    vault: &Vault,
    db: &GraphDb,
    embed: Option<&dyn EmbeddingProvider>,
    opts: &DistillOptions,
) -> Result<DistillStats> {
    let training_dir =
        opts.training_dir.clone().unwrap_or_else(|| vault.state_dir().join("training"));
    let out_dir = opts.out.clone().unwrap_or_else(|| training_dir.join("dataset"));
    let task_enabled = |t: &str| {
        opts.tasks.as_ref().map(|ts| ts.iter().any(|x| x == t)).unwrap_or(true)
    };

    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut skip = |reason: &str| *skipped.entry(reason.to_string()).or_insert(0) += 1;

    let mut pairs: Vec<Pair> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();

    // Run-captured pairs first: they carry the true original inputs and win
    // the dedup against mined pairs for the same page.
    if opts.from_runs {
        for pair in run_pairs(vault, &training_dir, &task_enabled, &mut skip)? {
            seen.insert((pair.task.to_string(), pair.key.clone()));
            pairs.push(pair);
        }
    }

    for pair in mined_pairs(vault, db, embed, &task_enabled, &mut skip)? {
        if seen.contains(&(pair.task.to_string(), pair.key.clone())) {
            skip("mined pair superseded by run pair");
            continue;
        }
        pairs.push(pair);
    }

    // Deterministic split + write.
    std::fs::create_dir_all(&out_dir)?;
    let mut train = std::io::BufWriter::new(std::fs::File::create(out_dir.join("train.jsonl"))?);
    let mut valid = std::io::BufWriter::new(std::fs::File::create(out_dir.join("valid.jsonl"))?);
    let mut emitted: BTreeMap<String, usize> = BTreeMap::new();
    let (mut n_train, mut n_valid) = (0usize, 0usize);
    for p in &pairs {
        let mut msgs: Vec<serde_json::Value> = p
            .messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect();
        msgs.push(json!({ "role": "assistant", "content": p.target_json }));
        let line = serde_json::to_string(&json!({ "messages": msgs }))?;
        if is_valid_split(&p.key, opts.split) {
            writeln!(valid, "{line}")?;
            n_valid += 1;
        } else {
            writeln!(train, "{line}")?;
            n_train += 1;
        }
        *emitted.entry(p.task.to_string()).or_insert(0) += 1;
    }
    train.flush()?;
    valid.flush()?;
    info!(train = n_train, valid = n_valid, "dataset written to {}", out_dir.display());

    Ok(DistillStats { emitted, skipped, train: n_train, valid: n_valid, out_dir })
}

/// sha256(key) → [0,1) fraction; below `split` lands in valid.jsonl.
fn is_valid_split(key: &str, split: f64) -> bool {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let digest = h.finalize();
    let x = u64::from_be_bytes(digest[..8].try_into().unwrap());
    (x as f64 / u64::MAX as f64) < split
}

// ---- vault mining ------------------------------------------------------------

fn mined_pairs(
    vault: &Vault,
    db: &GraphDb,
    embed: Option<&dyn EmbeddingProvider>,
    task_enabled: &dyn Fn(&str) -> bool,
    skip: &mut dyn FnMut(&str),
) -> Result<Vec<Pair>> {
    if !task_enabled("extract") && !task_enabled("suggest_links") {
        return Ok(vec![]);
    }
    let cfg = &vault.config;
    let source_field = cfg
        .frontmatter_edges()
        .find(|e| e.target == EdgeTarget::Resource)
        .and_then(|e| e.field.clone())
        .context("distill needs a frontmatter edge with target = \"resource\" (source layer)")?;

    let scanner = VaultScanner::new(vault)?;
    let (note_paths, _) = scanner.walk(&vault.root)?;
    let notes_meta = retrieve::all_notes(db).unwrap_or_default();
    let by_id: BTreeMap<&str, &NoteMeta> = notes_meta.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut pairs = Vec::new();
    for rel in &note_paths {
        let Ok(text) = std::fs::read_to_string(vault.root.join(rel)) else {
            skip("unreadable note");
            continue;
        };
        let note = parse_note(rel, &text, cfg);
        if note.kind.as_deref() != Some("source") {
            continue;
        }
        let refs: Vec<&str> = note
            .edge_fields
            .iter()
            .filter(|f| f.field == source_field)
            .map(|f| f.value.as_str())
            .collect();
        if refs.len() != 1 {
            skip("source page without exactly one source ref");
            continue;
        }
        let raw_rel = refs[0];
        if !raw_rel.ends_with(".md") || !vault.root.join(raw_rel).exists() {
            skip("raw file missing or not markdown");
            continue;
        }
        let Ok(raw_text) = std::fs::read_to_string(vault.root.join(raw_rel)) else {
            skip("unreadable raw file");
            continue;
        };
        let raw = parse_resource(raw_rel, &raw_text, true, cfg);
        if raw.body_text.trim().is_empty() {
            skip("raw file has no body");
            continue;
        }
        if raw.body_text.len() > 28_000 {
            // Runtime would chunk; a single-shot pair would misrepresent the task.
            skip("raw file longer than one extraction window");
            continue;
        }

        let (_, _, page_body, _) = split_frontmatter(&text);
        let Some(recon) = reconstruct(&text, page_body, &note.slug, &raw) else {
            skip("source page missing Summary/Key claims sections");
            continue;
        };

        if task_enabled("extract") {
            pairs.push(Pair {
                task: "extract",
                key: note.id.clone(),
                messages: prompts::extract_messages(&raw.title, raw.url.as_deref(), &raw.body_text),
                target_json: serde_json::to_string(&recon.extraction)?,
            });
        }

        if task_enabled("suggest_links") {
            // Rebuild candidates against today's graph, then force-union the
            // gold link targets so the accepted answer is always reachable.
            let plain: Vec<String> =
                recon.extraction.key_claims.iter().map(|c| c.text.clone()).collect();
            let entity_names: Vec<String> =
                recon.extraction.entities.iter().map(|e| e.name.clone()).collect();
            let candidates =
                retrieve::candidate_pages(db, embed, &plain, &entity_names, 16).unwrap_or_default();
            let mut lines: Vec<String> = Vec::new();
            let mut seen_ids: std::collections::HashSet<String> = Default::default();
            for c in &candidates {
                if let Some(meta) = by_id.get(c.id.as_str()) {
                    if seen_ids.insert(c.id.clone()) {
                        lines.push(candidate_line(meta, &cfg.vault.id_strip_prefix));
                    }
                }
            }
            for target in &recon.gold_link_ids {
                if let Some(meta) = by_id.get(target.as_str()) {
                    if seen_ids.insert(target.clone()) {
                        lines.push(candidate_line(meta, &cfg.vault.id_strip_prefix));
                    }
                }
            }
            let plan = LinkPlan {
                linked_claims: recon.linked_claims.clone(),
                new_pages: vec![],
                cross_references: recon.cross_references.clone(),
                update_targets: vec![],
            };
            pairs.push(Pair {
                task: "suggest_links",
                key: note.id.clone(),
                messages: prompts::suggest_links_messages(&plain, &lines, &note.slug),
                target_json: serde_json::to_string(&plan)?,
            });
        }
    }
    debug!(pairs = pairs.len(), "mined from vault");
    Ok(pairs)
}

fn candidate_line(meta: &NoteMeta, prefix: &str) -> String {
    let target = meta.path.strip_prefix(prefix).unwrap_or(&meta.path).trim_end_matches(".md");
    format!("{target} — {} ({})", meta.title, meta.kind)
}

struct Reconstruction {
    extraction: Extraction,
    /// Accepted claim bullets, wikilinks intact.
    linked_claims: Vec<String>,
    /// Cross-reference link targets exactly as written on the page.
    cross_references: Vec<String>,
    /// Note ids the accepted claims link to (for candidate force-union).
    gold_link_ids: Vec<String>,
}

/// Rebuild the Extraction a model would have needed to produce this accepted
/// source page. Returns None when the page doesn't follow the schema.
fn reconstruct(
    page_text: &str,
    page_body: &str,
    page_slug: &str,
    raw: &cogs_core::note::ParsedResource,
) -> Option<Reconstruction> {
    let summary = normalize(section(page_body, "Summary")?);
    let linked_claims = bullet_items(section(page_body, "Key claims")?);
    if summary.is_empty() || linked_claims.is_empty() {
        return None;
    }

    let (yaml, _, _, _) = split_frontmatter(page_text);
    let fm = yaml.map(yaml_to_json).unwrap_or(serde_json::Value::Null);
    let fm_str = |k: &str| fm.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let fm_list = |k: &str| -> Vec<String> {
        fm.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };

    let mut gold_link_ids = Vec::new();
    let key_claims: Vec<Claim> = linked_claims
        .iter()
        .map(|c| {
            let links = scan_wikilinks(c, 0);
            let entities: Vec<String> = links
                .iter()
                .map(|l| {
                    gold_link_ids.push(l.target.trim().to_lowercase().replace('/', "-"));
                    l.alias
                        .clone()
                        .unwrap_or_else(|| l.target.rsplit('/').next().unwrap_or("").to_string())
                })
                .collect();
            Claim { text: normalize(&strip_wikilinks_for_fts(c)), entities }
        })
        .collect();

    // Quotes: only those still verbatim in the raw survive as labels.
    let quotes: Vec<Quote> = section(page_body, "Quotes")
        .map(|s| {
            s.lines()
                .filter_map(|l| l.strip_prefix("> "))
                .filter_map(|l| {
                    let (text, location) = match l.rsplit_once(" — ") {
                        Some((t, loc)) => (t, loc.to_string()),
                        None => (l, String::new()),
                    };
                    let text = text.trim().trim_matches('"');
                    find_verbatim(&raw.body_text, text).map(|exact| Quote { text: exact, location })
                })
                .collect()
        })
        .unwrap_or_default();

    // Entities: what the page links to under entities/ and concepts/.
    let mut entities: Vec<EntityMention> = Vec::new();
    let mut seen_e: std::collections::HashSet<String> = Default::default();
    for l in scan_wikilinks(page_body, 0) {
        let t = l.target.trim().to_lowercase();
        let kind = if t.starts_with("entities/") {
            "entity"
        } else if t.starts_with("concepts/") {
            "concept"
        } else {
            continue;
        };
        let name = l
            .alias
            .clone()
            .unwrap_or_else(|| l.target.rsplit('/').next().unwrap_or("").to_string());
        if seen_e.insert(name.to_lowercase()) {
            entities.push(EntityMention { name, kind: kind.into(), blurb: String::new() });
        }
    }

    let cross_references: Vec<String> = section(page_body, "Cross-references")
        .map(|s| scan_wikilinks(s, 0).iter().map(|l| l.target.trim().to_string()).collect())
        .unwrap_or_default();

    let tags = fm_list("tags");
    Some(Reconstruction {
        extraction: Extraction {
            title: fm_str("title"),
            summary,
            key_claims,
            quotes,
            entities,
            topics: tags.clone(),
            suggested_slug: page_slug.to_string(),
            tags,
            author: fm_str("author"),
            publisher: fm_str("publisher"),
        },
        linked_claims,
        cross_references,
        gold_link_ids,
    })
}

// ---- run mining ---------------------------------------------------------------

fn run_pairs(
    vault: &Vault,
    training_dir: &std::path::Path,
    task_enabled: &dyn Fn(&str) -> bool,
    skip: &mut dyn FnMut(&str),
) -> Result<Vec<Pair>> {
    let runs_dir = training_dir.join("runs");
    let mut pairs = Vec::new();
    let Ok(entries) = std::fs::read_dir(&runs_dir) else {
        info!("no runs directory at {} — nothing captured yet", runs_dir.display());
        return Ok(pairs);
    };
    let mut manifests: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".meta.json"))
        .collect();
    manifests.sort();

    for mpath in manifests {
        let Ok(manifest) = std::fs::read_to_string(&mpath)
            .map_err(anyhow::Error::from)
            .and_then(|s| serde_json::from_str::<RunManifest>(&s).map_err(Into::into))
        else {
            skip("unreadable run manifest");
            continue;
        };
        let jsonl_path = runs_dir.join(format!("{}.jsonl", manifest.run_id));
        let Ok(jsonl) = std::fs::read_to_string(&jsonl_path) else {
            skip("run records missing");
            continue;
        };
        let records: Vec<TrainingRecord> =
            jsonl.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        // The created source page anchors extract/suggest_links acceptance.
        let source_write = manifest.writes.iter().find(|w| {
            w.kind == WriteKind::Created && w.rel_path.contains("/sources/")
        });

        for rec in &records {
            if !rec.parsed_ok {
                continue;
            }
            match rec.task {
                TaskKind::Extract | TaskKind::ExtractMerge if task_enabled("extract") => {
                    let Some(w) = source_write.filter(|w| w.seq == Some(rec.seq)) else {
                        skip("extract record without a surviving page write");
                        continue;
                    };
                    let Some(pair) =
                        page_backed_pair(vault, &manifest, rec, &w.rel_path, "extract", skip)
                    else {
                        continue;
                    };
                    pairs.push(pair);
                }
                TaskKind::ExtractChunk => skip("chunked extract records not mined"),
                TaskKind::SuggestLinks if task_enabled("suggest_links") => {
                    let Some(w) = source_write else {
                        skip("links record without a surviving page write");
                        continue;
                    };
                    let Some(pair) = links_run_pair(vault, &manifest, rec, &w.rel_path, skip)
                    else {
                        continue;
                    };
                    pairs.push(pair);
                }
                TaskKind::PageUpdate if task_enabled("page_update") => {
                    let Some(w) =
                        manifest.writes.iter().find(|w| w.seq == Some(rec.seq)).cloned()
                    else {
                        skip("update record without a write");
                        continue;
                    };
                    let Some(heading) = w.section_heading.clone() else { continue };
                    let Ok(text) = std::fs::read_to_string(vault.root.join(&w.rel_path)) else {
                        skip("updated page deleted (rejected)");
                        continue;
                    };
                    let (_, _, body, _) = split_frontmatter(&text);
                    let h = heading.trim_start_matches("## ");
                    let Some(sec) = section(body, h) else {
                        skip("appended section removed (rejected)");
                        continue;
                    };
                    let topic = h.split(" (").next().unwrap_or(h).to_string();
                    let upd = crate::PageUpdate {
                        topic,
                        section_md: sec.trim().to_string(),
                        relevant: true,
                    };
                    pairs.push(Pair {
                        task: "page_update",
                        key: format!("{}#{}", w.rel_path, heading),
                        messages: rec.messages.clone(),
                        target_json: serde_json::to_string(&upd)?,
                    });
                }
                TaskKind::Contradiction if task_enabled("contradiction") => {
                    let Ok(check) = serde_json::from_str::<ContradictionCheck>(
                        cogs_llm::extract_json(&rec.raw_output).unwrap_or("{}"),
                    ) else {
                        continue;
                    };
                    // Empty-findings checks are valid conservatism labels; found
                    // ones must still be present on the surviving source page.
                    let surviving = if check.findings.is_empty() {
                        check
                    } else {
                        let Some(w) = source_write else { continue };
                        let Ok(text) = std::fs::read_to_string(vault.root.join(&w.rel_path))
                        else {
                            skip("source page deleted (rejected)");
                            continue;
                        };
                        let kept: Vec<_> = check
                            .findings
                            .into_iter()
                            .filter(|f| {
                                section(split_frontmatter(&text).2, "Contradictions")
                                    .map(|s| s.contains(&f.page_id))
                                    .unwrap_or(false)
                            })
                            .collect();
                        if kept.is_empty() {
                            skip("contradiction findings removed (rejected)");
                            continue;
                        }
                        ContradictionCheck { findings: kept }
                    };
                    pairs.push(Pair {
                        task: "contradiction",
                        key: format!("{}:{}", manifest.run_id, rec.seq),
                        messages: rec.messages.clone(),
                        target_json: serde_json::to_string(&surviving)?,
                    });
                }
                _ => {}
            }
        }
    }
    debug!(pairs = pairs.len(), "mined from runs");
    Ok(pairs)
}

/// Extract-family run pair: original recorded inputs + target reconstructed
/// from the page as it survives on disk today.
fn page_backed_pair(
    vault: &Vault,
    manifest: &RunManifest,
    rec: &TrainingRecord,
    page_rel: &str,
    task: &'static str,
    skip: &mut dyn FnMut(&str),
) -> Option<Pair> {
    let Ok(text) = std::fs::read_to_string(vault.root.join(page_rel)) else {
        skip("created page deleted (rejected)");
        return None;
    };
    let (_, _, body, _) = split_frontmatter(&text);
    let (_, slug, _) = derive_ids(page_rel, &vault.config.vault.id_strip_prefix);
    let raw_text =
        std::fs::read_to_string(vault.root.join(&manifest.raw_rel_path)).unwrap_or_default();
    let raw = parse_resource(&manifest.raw_rel_path, &raw_text, true, &vault.config);
    let recon = reconstruct(&text, body, &slug, &raw)?;
    Some(Pair {
        task,
        key: derive_ids(page_rel, &vault.config.vault.id_strip_prefix).0,
        messages: rec.messages.clone(),
        target_json: serde_json::to_string(&recon.extraction).ok()?,
    })
}

/// suggest_links run pair: recorded inputs + plan reconstructed from what
/// survives (linked claims from the page, update targets from sections that
/// are still present, new pages from created files that still exist).
fn links_run_pair(
    vault: &Vault,
    manifest: &RunManifest,
    rec: &TrainingRecord,
    page_rel: &str,
    skip: &mut dyn FnMut(&str),
) -> Option<Pair> {
    let Ok(text) = std::fs::read_to_string(vault.root.join(page_rel)) else {
        skip("created page deleted (rejected)");
        return None;
    };
    let (_, _, body, _) = split_frontmatter(&text);
    let linked_claims = bullet_items(section(body, "Key claims")?);
    if linked_claims.is_empty() {
        skip("source page missing Key claims");
        return None;
    }
    let cross_references: Vec<String> = section(body, "Cross-references")
        .map(|s| scan_wikilinks(s, 0).iter().map(|l| l.target.trim().to_string()).collect())
        .unwrap_or_default();

    let prefix = &vault.config.vault.id_strip_prefix;
    let mut new_pages = Vec::new();
    let mut update_targets = Vec::new();
    for w in &manifest.writes {
        let rel = &w.rel_path;
        if rel == page_rel || rel.ends_with("log.md") {
            continue;
        }
        let target = rel.strip_prefix(prefix).unwrap_or(rel).trim_end_matches(".md").to_string();
        match w.kind {
            WriteKind::Created => {
                let Ok(t) = std::fs::read_to_string(vault.root.join(rel)) else { continue };
                let note = parse_note(rel, &t, &vault.config);
                let (dir, slug) = match target.split_once('/') {
                    Some((d, s)) => (d.to_string(), s.to_string()),
                    None => continue,
                };
                new_pages.push(crate::NewPageSpec {
                    slug,
                    dir,
                    title: note.title,
                    kind: note.kind.unwrap_or_default(),
                    blurb: String::new(),
                });
            }
            WriteKind::Appended | WriteKind::FmEdited => {
                let survives = std::fs::read_to_string(vault.root.join(rel))
                    .ok()
                    .zip(w.section_heading.as_ref())
                    .map(|(t, h)| t.contains(h.as_str()))
                    .unwrap_or(false);
                if survives {
                    update_targets.push(target);
                }
            }
        }
    }

    let plan =
        LinkPlan { linked_claims, new_pages, cross_references, update_targets };
    Some(Pair {
        task: "suggest_links",
        key: derive_ids(page_rel, prefix).0,
        messages: rec.messages.clone(),
        target_json: serde_json::to_string(&plan).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_is_deterministic_and_bounded() {
        assert_eq!(is_valid_split("some-id", 0.1), is_valid_split("some-id", 0.1));
        assert!(!is_valid_split("anything", 0.0));
        assert!(is_valid_split("anything", 1.1));
        // roughly a tenth of keys land in valid
        let n = (0..1000).filter(|i| is_valid_split(&format!("k{i}"), 0.1)).count();
        assert!((50..200).contains(&n), "got {n}");
    }
}
