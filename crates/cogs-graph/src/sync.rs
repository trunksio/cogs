use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use lbug::{Connection, PreparedStatement, Value};
use tracing::info;

use cogs_core::config::{EdgeTarget, Vault};
use cogs_core::note::{ParsedNote, ParsedResource};
use cogs_core::parse::{derive_ids, parse_note, parse_resource};
use cogs_core::resolve::{LinkResolver, Resolution};
use cogs_core::scan::{fingerprint, resource_meta_path, FileState, IndexState, VaultScanner};

use crate::db::{opt_date, opt_string, GraphDb};
use crate::schema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Full,
    Incremental,
}

#[derive(Debug)]
pub struct SyncOutcome {
    pub mode: SyncMode,
    pub notes_synced: usize,
    /// Unchanged notes whose edges were rewritten because link resolution
    /// shifted (a note was added/removed/renamed elsewhere).
    pub notes_relinked: usize,
    pub resources_synced: usize,
    pub deleted: usize,
    pub edges_written: usize,
    pub embeddings_written: usize,
    pub total_notes: usize,
    pub total_resources: usize,
}

pub struct SyncEngine {
    vault: Vault,
    scanner: VaultScanner,
}

impl SyncEngine {
    pub fn new(vault: &Vault) -> Result<Self> {
        Ok(Self { vault: vault.clone(), scanner: VaultScanner::new(vault)? })
    }

    /// Index the vault into the graph DB. `force_all` reprocesses every file
    /// regardless of fingerprints (the DB itself is wiped by GraphDb::open_rw
    /// when rebuilding; this flag just widens the work set).
    pub fn sync(&self, db: &GraphDb, force_all: bool) -> Result<SyncOutcome> {
        self.sync_with(db, force_all, None)
    }

    /// Like `sync`, with an optional embedding provider: nodes whose
    /// `embedded_hash` lags `body_hash` get (re)embedded after the upsert
    /// pass, with the vector indexes dropped around the writes and recreated
    /// afterwards (Ladybug locks the column while an index exists).
    pub fn sync_with(
        &self,
        db: &GraphDb,
        force_all: bool,
        embed: Option<&dyn crate::embed::EmbeddingProvider>,
    ) -> Result<SyncOutcome> {
        let vault = &self.vault;
        let root = &vault.root;
        let strip = &vault.config.vault.id_strip_prefix;

        let (note_paths, resource_paths) = self.scanner.walk(root)?;
        let prev_state = IndexState::load(&vault.index_state_path());
        let full = force_all
            || db.rebuilt
            || prev_state.config_hash != vault.config_hash;
        let mode = if full { SyncMode::Full } else { SyncMode::Incremental };

        // ---- fingerprint pass -------------------------------------------
        let mut new_files: BTreeMap<String, FileState> = BTreeMap::new();
        let mut changed_notes: Vec<&str> = Vec::new();
        for p in &note_paths {
            let fs = fingerprint(&root.join(p), prev_state.files.get(p))
                .with_context(|| format!("fingerprinting {p}"))?;
            let changed = full
                || prev_state.files.get(p).map(|prev| prev.content_hash != fs.content_hash)
                    != Some(false);
            if changed {
                changed_notes.push(p);
            }
            new_files.insert(p.clone(), fs);
        }

        let mut changed_resources: Vec<(String, Option<String>)> = Vec::new();
        for p in &resource_paths {
            let meta_rel = resource_meta_path(root, p);
            let fs = fingerprint(&root.join(p), prev_state.files.get(p))?;
            let mut changed = full
                || prev_state.files.get(p).map(|prev| prev.content_hash != fs.content_hash)
                    != Some(false);
            new_files.insert(p.clone(), fs);
            if let Some(meta_rel) = &meta_rel {
                if meta_rel != p {
                    let mfs = fingerprint(&root.join(meta_rel), prev_state.files.get(meta_rel))?;
                    changed = changed
                        || prev_state
                            .files
                            .get(meta_rel)
                            .map(|prev| prev.content_hash != mfs.content_hash)
                            != Some(false);
                    new_files.insert(meta_rel.clone(), mfs);
                }
            }
            if changed {
                changed_resources.push((p.clone(), meta_rel));
            }
        }

        // ---- deletions ----------------------------------------------------
        let current: HashSet<&String> = new_files.keys().collect();
        let mut deleted_note_ids: Vec<String> = Vec::new();
        let mut deleted_resource_paths: Vec<String> = Vec::new();
        for old_path in prev_state.files.keys() {
            if current.contains(old_path) {
                continue;
            }
            if self.scanner.is_note(old_path) {
                deleted_note_ids.push(derive_ids(old_path, strip).0);
            } else if self.scanner.is_resource(old_path) {
                deleted_resource_paths.push(old_path.clone());
            }
        }

        // ---- resolvers ------------------------------------------------------
        let id_pairs = |paths: &[String]| -> Vec<(String, String)> {
            paths
                .iter()
                .map(|p| {
                    let (id, slug, _) = derive_ids(p, strip);
                    (id, slug)
                })
                .collect()
        };
        let new_pairs = id_pairs(&note_paths);
        let new_resolver =
            LinkResolver::new(new_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())));
        let old_note_paths: Vec<String> = prev_state
            .files
            .keys()
            .filter(|p| self.scanner.is_note(p))
            .cloned()
            .collect();
        let note_set_changed = full
            || !deleted_note_ids.is_empty()
            || note_paths.len() != old_note_paths.len()
            || note_paths.iter().any(|p| !prev_state.files.contains_key(p));
        let old_pairs = id_pairs(&old_note_paths);
        let old_resolver =
            LinkResolver::new(old_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())));

        // ---- parse ----------------------------------------------------------
        let changed_set: HashSet<&str> = changed_notes.iter().copied().collect();
        let mut parsed_changed: Vec<ParsedNote> = Vec::new();
        let mut relink_only: Vec<ParsedNote> = Vec::new();
        for p in &note_paths {
            let is_changed = changed_set.contains(p.as_str());
            // Unchanged notes only matter when the note set shifted and one of
            // their links now resolves differently.
            if !is_changed && !note_set_changed {
                continue;
            }
            let text = std::fs::read_to_string(root.join(p))
                .with_context(|| format!("reading {p}"))?;
            let note = parse_note(p, &text, &vault.config);
            if is_changed {
                parsed_changed.push(note);
            } else if resolution_shifted(&note, &old_resolver, &new_resolver) {
                relink_only.push(note);
            }
        }

        let mut parsed_resources: Vec<ParsedResource> = Vec::new();
        for (p, meta_rel) in &changed_resources {
            let Some(meta_rel) = meta_rel else {
                continue; // no metadata: parity with sync_graph.py (skip silently)
            };
            let text = std::fs::read_to_string(root.join(meta_rel))
                .with_context(|| format!("reading {meta_rel}"))?;
            parsed_resources.push(parse_resource(p, &text, p.ends_with(".md"), &vault.config));
        }

        info!(
            mode = ?mode,
            changed = parsed_changed.len(),
            relinked = relink_only.len(),
            resources = parsed_resources.len(),
            deleted = deleted_note_ids.len() + deleted_resource_paths.len(),
            "sync starting"
        );

        // ---- write ----------------------------------------------------------
        let conn = db.conn()?;
        let mut stmts = SyncStmts::new(&conn, vault)?;
        let in_txn = conn.query("BEGIN TRANSACTION").is_ok();

        for id in &deleted_note_ids {
            conn.execute(&mut stmts.delete_note, vec![("id", Value::String(id.clone()))])?;
        }
        if let Some(stmt) = stmts.delete_resource.as_mut() {
            for path in &deleted_resource_paths {
                conn.execute(stmt, vec![("path", Value::String(path.clone()))])?;
            }
        }

        // Pass 1: pre-create ids so pass-2 edge MATCHes resolve regardless of
        // file order (port of sync_graph.py's two-pass design).
        for note in &parsed_changed {
            conn.execute(&mut stmts.merge_note_id, vec![("id", Value::String(note.id.clone()))])?;
        }

        // Pass 2: properties + edges.
        let mut edges_written = 0usize;
        for note in &parsed_changed {
            write_note_props(&conn, &mut stmts, note)?;
            edges_written += write_note_edges(&conn, &mut stmts, vault, note, &new_resolver)?;
        }
        for note in &relink_only {
            edges_written += write_note_edges(&conn, &mut stmts, vault, note, &new_resolver)?;
        }
        for res in &parsed_resources {
            write_resource(&conn, &mut stmts, res)?;
        }

        if in_txn {
            conn.query("COMMIT")?;
        }

        let anything_changed = !parsed_changed.is_empty()
            || !relink_only.is_empty()
            || !deleted_note_ids.is_empty();
        if anything_changed {
            schema::refresh_fts(&conn);
        }

        let embeddings_written = match embed {
            Some(provider) => self.embed_phase(&conn, provider)?,
            None => 0,
        };

        drop(conn);
        db.checkpoint()?;

        // ---- persist fingerprint state ---------------------------------------
        let state = IndexState { config_hash: vault.config_hash.clone(), files: new_files };
        state.save(&vault.index_state_path())?;

        let conn = db.conn()?;
        let total_notes = count(&conn, "MATCH (n:Note) RETURN count(n)");
        let total_resources = if vault.config.resources.is_some() {
            count(&conn, "MATCH (r:Resource) RETURN count(r)")
        } else {
            0
        };

        Ok(SyncOutcome {
            mode,
            notes_synced: parsed_changed.len(),
            notes_relinked: relink_only.len(),
            resources_synced: parsed_resources.len(),
            deleted: deleted_note_ids.len() + deleted_resource_paths.len(),
            edges_written,
            embeddings_written,
            total_notes,
            total_resources,
        })
    }

    /// Embed every node whose embedded_hash lags its body_hash. Failures are
    /// logged and skipped — the graph stays useful without embeddings, and a
    /// failed note retries next sync because embedded_hash stays stale.
    fn embed_phase(
        &self,
        conn: &Connection,
        provider: &dyn crate::embed::EmbeddingProvider,
    ) -> Result<usize> {
        let cfg = &self.vault.config.embeddings;
        let exclude = &cfg.exclude_kinds;

        let mut work: Vec<(&str, String, String, String)> = Vec::new(); // (table, key, body, body_hash)
        let rows = conn.query(
            "MATCH (n:Note) \
             WHERE n.embedded_hash IS NULL OR n.embedded_hash <> n.body_hash \
             RETURN n.id, n.body_text, n.body_hash, n.kind",
        )?;
        for row in rows {
            let mut it = row.into_iter();
            let (Some(Value::String(id)), body, hash, kind) =
                (it.next(), it.next(), it.next(), it.next())
            else {
                continue;
            };
            if let Some(Value::String(k)) = &kind {
                if exclude.contains(k) {
                    continue;
                }
            }
            let body = match body {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let hash = match hash {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            work.push(("Note", id, body, hash));
        }
        if cfg.embed_resources && self.vault.config.resources.is_some() {
            let rows = conn.query(
                "MATCH (r:Resource) \
                 WHERE r.body_text IS NOT NULL AND r.body_text <> '' \
                   AND (r.embedded_hash IS NULL OR r.embedded_hash <> r.body_hash) \
                 RETURN r.path, r.body_text, r.body_hash",
            )?;
            for row in rows {
                let mut it = row.into_iter();
                let (Some(Value::String(path)), Some(Value::String(body)), Some(Value::String(hash))) =
                    (it.next(), it.next(), it.next())
                else {
                    continue;
                };
                work.push(("Resource", path, body, hash));
            }
        }
        if work.is_empty() {
            return Ok(0);
        }

        info!(pending = work.len(), "embedding changed nodes");
        schema::drop_vector_indices(conn);

        let mut note_stmt = conn.prepare(
            "MATCH (n:Note {id: $key}) SET n.embedding = $vec, n.embedded_hash = $hash",
        )?;
        let mut res_stmt = if self.vault.config.resources.is_some() {
            Some(conn.prepare(
                "MATCH (r:Resource {path: $key}) SET r.embedding = $vec, r.embedded_hash = $hash",
            )?)
        } else {
            None
        };

        let mut written = 0usize;
        let mut failed = 0usize;
        for (table, key, body, hash) in work {
            let vec = match provider.embed(&body) {
                Ok(v) => v,
                Err(e) => {
                    failed += 1;
                    tracing::warn!("embed failed for {key}: {e:#}");
                    continue;
                }
            };
            let value = Value::Array(
                lbug::LogicalType::Float,
                vec.into_iter().map(Value::Float).collect(),
            );
            let stmt = match table {
                "Note" => &mut note_stmt,
                _ => res_stmt.as_mut().expect("resource stmt prepared"),
            };
            conn.execute(
                stmt,
                vec![
                    ("key", Value::String(key)),
                    ("vec", value),
                    ("hash", Value::String(hash)),
                ],
            )?;
            written += 1;
        }
        if failed > 0 {
            tracing::warn!("{failed} embeddings failed (will retry next sync)");
        }

        let any_embeddings = conn
            .query("MATCH (n:Note) WHERE n.embedding IS NOT NULL RETURN n.id LIMIT 1")?
            .next()
            .is_some();
        if any_embeddings {
            schema::create_vector_indices(conn, self.vault.config.resources.is_some());
        }
        Ok(written)
    }
}

fn resolution_shifted(
    note: &ParsedNote,
    old: &LinkResolver,
    new: &LinkResolver,
) -> bool {
    let mut targets: Vec<&str> = note.links.iter().map(|l| l.target.as_str()).collect();
    targets.extend(note.edge_fields.iter().map(|e| e.value.as_str()));
    targets
        .iter()
        .any(|t| old.resolve(t, &note.dir) != new.resolve(t, &note.dir))
}

fn count(conn: &Connection, cypher: &str) -> usize {
    conn.query(cypher)
        .ok()
        .and_then(|mut r| r.next())
        .and_then(|row| row.into_iter().next())
        .map(|v| match v {
            Value::Int64(n) => n as usize,
            Value::UInt64(n) => n as usize,
            _ => 0,
        })
        .unwrap_or(0)
}

/// Prepared statements reused across the batch.
struct SyncStmts {
    merge_note_id: PreparedStatement,
    set_note_props: PreparedStatement,
    delete_note: PreparedStatement,
    /// Only present when the vault declares [resources] (the table exists).
    delete_resource: Option<PreparedStatement>,
    upsert_resource: Option<PreparedStatement>,
    merge_resource_stub: Option<PreparedStatement>,
    merge_tag: PreparedStatement,
    tag_edge: PreparedStatement,
    /// edge name -> (clear stmt, create-to-note stmt or create-to-resource stmt)
    clear_edge: HashMap<String, PreparedStatement>,
    create_edge: HashMap<String, PreparedStatement>,
    clear_tagged: PreparedStatement,
}

impl SyncStmts {
    fn new(conn: &Connection, vault: &Vault) -> Result<Self> {
        let has_resources = vault.config.resources.is_some();
        let mut clear_edge = HashMap::new();
        let mut create_edge = HashMap::new();
        for e in &vault.config.edges {
            clear_edge.insert(
                e.name.clone(),
                conn.prepare(&format!(
                    "MATCH (p:Note {{id: $id}})-[r:{}]->() DELETE r",
                    e.name
                ))?,
            );
            let create = match e.target {
                EdgeTarget::Note => format!(
                    "MATCH (a:Note {{id: $src}}), (b:Note {{id: $dst}}) \
                     MERGE (a)-[r:{}]->(b) ON CREATE SET r.raw_target = $raw",
                    e.name
                ),
                EdgeTarget::Resource => format!(
                    "MATCH (a:Note {{id: $src}}), (b:Resource {{path: $dst}}) \
                     MERGE (a)-[r:{}]->(b) ON CREATE SET r.raw_target = $raw",
                    e.name
                ),
            };
            create_edge.insert(e.name.clone(), conn.prepare(&create)?);
        }
        Ok(Self {
            merge_note_id: conn.prepare("MERGE (p:Note {id: $id})")?,
            set_note_props: conn.prepare(
                "MERGE (p:Note {id: $id})
                 SET p.path = $path,
                     p.slug = $slug,
                     p.dir = $dir,
                     p.title = $title,
                     p.kind = $kind,
                     p.status = $status,
                     p.created = $created,
                     p.updated = $updated,
                     p.tags = $tags,
                     p.body_text = $body_text,
                     p.body_hash = $body_hash,
                     p.frontmatter_json = $frontmatter_json",
            )?,
            delete_note: conn.prepare("MATCH (p:Note {id: $id}) DETACH DELETE p")?,
            delete_resource: has_resources
                .then(|| conn.prepare("MATCH (r:Resource {path: $path}) DETACH DELETE r"))
                .transpose()?,
            upsert_resource: has_resources
                .then(|| {
                    conn.prepare(
                        "MERGE (r:Resource {path: $path})
                         SET r.title = $title,
                             r.captured = $captured,
                             r.source_date = $source_date,
                             r.url = $url,
                             r.body_text = $body_text,
                             r.body_hash = $body_hash",
                    )
                })
                .transpose()?,
            merge_resource_stub: has_resources
                .then(|| conn.prepare("MERGE (r:Resource {path: $path})"))
                .transpose()?,
            merge_tag: conn.prepare("MERGE (:Tag {name: $name})")?,
            tag_edge: conn.prepare(
                "MATCH (p:Note {id: $src}), (t:Tag {name: $dst}) MERGE (p)-[:TAGGED]->(t)",
            )?,
            clear_tagged: conn.prepare("MATCH (p:Note {id: $id})-[r:TAGGED]->() DELETE r")?,
            clear_edge,
            create_edge,
        })
    }
}

fn write_note_props(conn: &Connection, stmts: &mut SyncStmts, note: &ParsedNote) -> Result<()> {
    let tags = Value::List(
        lbug::LogicalType::String,
        note.tags.iter().map(|t| Value::String(t.clone())).collect(),
    );
    conn.execute(
        &mut stmts.set_note_props,
        vec![
            ("id", Value::String(note.id.clone())),
            ("path", Value::String(note.rel_path.clone())),
            ("slug", Value::String(note.slug.clone())),
            ("dir", Value::String(note.dir.clone())),
            ("title", Value::String(note.title.clone())),
            ("kind", opt_string(note.kind.clone())),
            ("status", opt_string(note.status.clone())),
            ("created", opt_date(note.created)),
            ("updated", opt_date(note.updated)),
            ("tags", tags),
            ("body_text", Value::String(note.body_text.clone())),
            ("body_hash", Value::String(note.body_hash.clone())),
            ("frontmatter_json", Value::String(note.frontmatter_json.clone())),
        ],
    )
    .with_context(|| format!("upserting note {}", note.id))?;
    Ok(())
}

fn write_note_edges(
    conn: &Connection,
    stmts: &mut SyncStmts,
    vault: &Vault,
    note: &ParsedNote,
    resolver: &LinkResolver,
) -> Result<usize> {
    // Wipe and re-derive all outgoing edges from current content.
    for clear in stmts.clear_edge.values_mut() {
        conn.execute(clear, vec![("id", Value::String(note.id.clone()))])?;
    }
    conn.execute(&mut stmts.clear_tagged, vec![("id", Value::String(note.id.clone()))])?;

    let mut edges = 0usize;

    // Frontmatter-driven edges.
    for e in vault.config.frontmatter_edges() {
        let field = e.field.as_deref().unwrap_or_default();
        for item in note.edge_fields.iter().filter(|i| i.field == field) {
            match e.target {
                EdgeTarget::Resource => {
                    // Ensure a stub Resource node exists (the metadata upsert
                    // may come later or never — parity with sync_graph.py).
                    let stub = stmts
                        .merge_resource_stub
                        .as_mut()
                        .expect("resource edge requires [resources] (validated)");
                    conn.execute(stub, vec![("path", Value::String(item.value.clone()))])?;
                    let stmt = stmts.create_edge.get_mut(&e.name).unwrap();
                    conn.execute(
                        stmt,
                        vec![
                            ("src", Value::String(note.id.clone())),
                            ("dst", Value::String(item.value.clone())),
                            ("raw", Value::String(item.value.clone())),
                        ],
                    )?;
                    edges += 1;
                }
                EdgeTarget::Note => {
                    if let Resolution::Resolved(target_id) =
                        resolver.resolve(&item.value, &note.dir)
                    {
                        let stmt = stmts.create_edge.get_mut(&e.name).unwrap();
                        conn.execute(
                            stmt,
                            vec![
                                ("src", Value::String(note.id.clone())),
                                ("dst", Value::String(target_id)),
                                ("raw", Value::String(item.value.clone())),
                            ],
                        )?;
                        edges += 1;
                    }
                }
            }
        }
    }

    // TAGGED edges.
    for tag in &note.tags {
        conn.execute(&mut stmts.merge_tag, vec![("name", Value::String(tag.clone()))])?;
        conn.execute(
            &mut stmts.tag_edge,
            vec![
                ("src", Value::String(note.id.clone())),
                ("dst", Value::String(tag.clone())),
            ],
        )?;
        edges += 1;
    }

    // Wikilink edge (CITES / LINKS_TO). Masked (in-code) links are included
    // for parity with sync_graph.py; targets dedup at text level like the
    // Python set-of-targets, self-links skipped.
    if let Some(e) = vault.config.wikilink_edge() {
        let mut seen: HashSet<&str> = HashSet::new();
        for link in &note.links {
            if !seen.insert(link.target.as_str()) {
                continue;
            }
            let Resolution::Resolved(target_id) = resolver.resolve(&link.target, &note.dir)
            else {
                continue;
            };
            if target_id == note.id {
                continue;
            }
            let stmt = stmts.create_edge.get_mut(&e.name).unwrap();
            conn.execute(
                stmt,
                vec![
                    ("src", Value::String(note.id.clone())),
                    ("dst", Value::String(target_id)),
                    ("raw", Value::String(link.target.clone())),
                ],
            )?;
            edges += 1;
        }
    }

    Ok(edges)
}

fn write_resource(
    conn: &Connection,
    stmts: &mut SyncStmts,
    res: &ParsedResource,
) -> Result<()> {
    let stmt = stmts
        .upsert_resource
        .as_mut()
        .expect("resource files only scanned when [resources] configured");
    conn.execute(
        stmt,
        vec![
            ("path", Value::String(res.rel_path.clone())),
            ("title", Value::String(res.title.clone())),
            ("captured", opt_date(res.captured)),
            ("source_date", opt_date(res.source_date)),
            ("url", opt_string(res.url.clone())),
            ("body_text", Value::String(res.body_text.clone())),
            ("body_hash", Value::String(res.body_hash.clone())),
        ],
    )
    .with_context(|| format!("upserting resource {}", res.rel_path))?;
    Ok(())
}
