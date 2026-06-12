//! Read-only MCP server over the graph DB (replaces aoa-knowledge's
//! scripts/mcp_server.py). Tools mirror the Python six plus health_report;
//! semantic tools land with the embeddings milestone.

use std::sync::Mutex;

use anyhow::Result as AnyResult;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use cogs_core::config::Vault;
use cogs_core::parse::derive_ids;
use cogs_graph::GraphDb;

pub struct CogsMcp {
    vault: Vault,
    /// Present when this process won the writer role at startup (it then also
    /// keeps the DB fresh on its own syncs). Otherwise each call opens
    /// read-only so it sees the LSP's latest checkpoint.
    primary_db: Option<Mutex<GraphDb>>,
    /// Lazily-built embedding provider (only needed for semantic_search).
    embedder: std::sync::OnceLock<Option<Box<dyn cogs_graph::EmbeddingProvider>>>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn jtext(v: Value) -> Result<String, ErrorData> {
    serde_json::to_string_pretty(&v).map_err(internal)
}

fn cypher_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

impl CogsMcp {
    pub fn new(vault: Vault) -> AnyResult<Self> {
        // Try to become the writer (keeps data fresh when no LSP is running);
        // fall back to read-only-per-call when another process holds the DB.
        let primary_db = match GraphDb::open_rw(&vault, false) {
            Ok(db) => {
                let engine = cogs_graph::SyncEngine::new(&vault)?;
                if let Err(e) = engine.sync(&db, false) {
                    tracing::warn!("initial sync failed: {e:#}");
                }
                Some(Mutex::new(db))
            }
            Err(e) => {
                tracing::info!("running as reader ({e})");
                None
            }
        };
        Ok(Self {
            vault,
            primary_db,
            embedder: std::sync::OnceLock::new(),
            tool_router: Self::tool_router(),
        })
    }

    fn query(&self, cypher: &str) -> Result<Vec<Value>, ErrorData> {
        match &self.primary_db {
            Some(db) => db.lock().unwrap().query_json(cypher).map_err(internal),
            None => {
                let db = GraphDb::open_ro(&self.vault).map_err(internal)?;
                db.query_json(cypher).map_err(internal)
            }
        }
    }

    /// Port of mcp_server.py's _resolve_page_id: exact id, path-derived id,
    /// then unique bare slug.
    fn resolve_id(&self, raw: &str) -> Result<String, ErrorData> {
        let strip = &self.vault.config.vault.id_strip_prefix;
        let candidates = [
            raw.to_string(),
            derive_ids(raw, strip).0,
            raw.trim_end_matches(".md").replace('/', "-"),
        ];
        for c in &candidates {
            let rows = self.query(&format!(
                "MATCH (n:Note {{id: '{}'}}) RETURN n.id",
                cypher_escape(c)
            ))?;
            if !rows.is_empty() {
                return Ok(c.clone());
            }
        }
        // Unique bare slug?
        let rows = self.query(&format!(
            "MATCH (n:Note {{slug: '{}'}}) RETURN n.id",
            cypher_escape(&raw.to_lowercase())
        ))?;
        if rows.len() == 1 {
            if let Some(id) = rows[0].get("n.id").and_then(|v| v.as_str()) {
                return Ok(id.to_string());
            }
        }
        Err(ErrorData::invalid_params(
            format!("note not found: {raw:?} (try the `search` tool to locate it)"),
            None,
        ))
    }

    fn edge_names(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.vault.config.edges.iter().map(|e| e.name.clone()).collect();
        names.push("TAGGED".into());
        names
    }

    fn with_db<T>(
        &self,
        f: impl FnOnce(&GraphDb) -> anyhow::Result<T>,
    ) -> Result<T, ErrorData> {
        match &self.primary_db {
            Some(db) => f(&db.lock().unwrap()).map_err(internal),
            None => {
                let db = GraphDb::open_ro(&self.vault).map_err(internal)?;
                f(&db).map_err(internal)
            }
        }
    }

    /// Embed a query string. The provider uses reqwest::blocking, which must
    /// not be created or driven on a tokio worker thread (rmcp runs tool
    /// handlers inside its runtime) — hop to a plain OS thread.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, ErrorData> {
        std::thread::scope(|s| {
            s.spawn(|| {
                let provider = self
                    .embedder
                    .get_or_init(|| {
                        cogs_graph::make_provider(&self.vault.config.embeddings)
                            .map_err(|e| {
                                tracing::warn!("embedding provider unavailable: {e:#}")
                            })
                            .ok()
                    })
                    .as_deref()
                    .ok_or_else(|| {
                        ErrorData::internal_error(
                            "embedding provider unavailable (check [embeddings] config and \
                             that the provider endpoint is reachable)",
                            None,
                        )
                    })?;
                provider.embed(text).map_err(internal)
            })
            .join()
            .map_err(|_| ErrorData::internal_error("embedding thread panicked", None))?
        })
    }

    fn describe(&self, ids: &[(String, f64)]) -> Result<Vec<Value>, ErrorData> {
        ids.iter()
            .map(|(id, dist)| {
                let rows = self.query(&format!(
                    "MATCH (n:Note {{id: '{}'}}) RETURN n.id AS id, n.title AS title, \
                     n.kind AS kind",
                    cypher_escape(id)
                ))?;
                let mut v = rows.into_iter().next().unwrap_or(json!({ "id": id }));
                v["score"] = json!(1.0 - dist);
                Ok(v)
            })
            .collect()
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Search terms (BM25 over note titles and bodies)
    query: String,
    /// Max results (default 10)
    k: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetNoteParams {
    /// Note id (e.g. "concepts-agentic-unit"), path, or unique slug
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NeighboursParams {
    /// Note id, path, or unique slug
    id: String,
    /// Edge type to follow, or "*" for all
    edge: Option<String>,
    /// "out", "in", or "both" (default "both")
    direction: Option<String>,
    /// Max results (default 25)
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LineageParams {
    /// Note id, path, or unique slug
    id: String,
    /// Max traversal depth (default 3)
    max_depth: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SimilarNotesParams {
    /// Note id, path, or unique slug
    id: String,
    /// Max results (default 10)
    k: Option<u32>,
    /// Drop notes already linked to this one (surface auto-link candidates)
    exclude_linked: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListNotesParams {
    /// Filter by kind (e.g. "concept")
    kind: Option<String>,
    /// Filter by status (e.g. "stable")
    status: Option<String>,
    /// Filter by tag
    tag: Option<String>,
    /// Max results (default 50)
    limit: Option<u32>,
}

#[tool_router]
impl CogsMcp {
    #[tool(description = "Full-text (BM25) search over note titles and bodies")]
    fn search(&self, Parameters(p): Parameters<SearchParams>) -> Result<String, ErrorData> {
        let k = p.k.unwrap_or(10).min(50);
        let rows = self.query(&format!(
            "CALL QUERY_FTS_INDEX('Note', 'note_fts', '{}') \
             RETURN node.id AS id, node.title AS title, node.kind AS kind, \
                    node.status AS status, score \
             ORDER BY score DESC LIMIT {k}",
            cypher_escape(&p.query)
        ))?;
        jtext(json!({ "hits": rows }))
    }

    #[tool(description = "Fetch a note's metadata and full markdown body (disk-fresh)")]
    fn get_note(&self, Parameters(p): Parameters<GetNoteParams>) -> Result<String, ErrorData> {
        let id = self.resolve_id(&p.id)?;
        let rows = self.query(&format!(
            "MATCH (n:Note {{id: '{}'}}) \
             RETURN n.id AS id, n.path AS path, n.title AS title, n.kind AS kind, \
                    n.status AS status, n.updated AS updated, n.tags AS tags, \
                    n.frontmatter_json AS frontmatter",
            cypher_escape(&id)
        ))?;
        let mut note = rows.into_iter().next().unwrap_or(json!({}));
        // Body always read from disk so unsynced edits are visible.
        if let Some(path) = note.get("path").and_then(|v| v.as_str()) {
            let body = std::fs::read_to_string(self.vault.root.join(path)).unwrap_or_default();
            note["markdown"] = json!(body);
        }
        jtext(note)
    }

    #[tool(description = "Typed graph adjacency: which notes/resources/tags connect to this note")]
    fn neighbours(
        &self,
        Parameters(p): Parameters<NeighboursParams>,
    ) -> Result<String, ErrorData> {
        let id = self.resolve_id(&p.id)?;
        let limit = p.limit.unwrap_or(25).min(200);
        let edges = match p.edge.as_deref() {
            None | Some("*") => self.edge_names().join("|"),
            Some(e) => {
                let e = e.to_uppercase();
                if !self.edge_names().contains(&e) {
                    return Err(ErrorData::invalid_params(
                        format!("unknown edge {e:?}; available: {}", self.edge_names().join(", ")),
                        None,
                    ));
                }
                e
            }
        };
        let id_esc = cypher_escape(&id);
        let mut out = json!({ "id": id, "out": [], "in": [] });
        let direction = p.direction.as_deref().unwrap_or("both");
        if direction == "out" || direction == "both" {
            out["out"] = json!(self.query(&format!(
                "MATCH (a:Note {{id: '{id_esc}'}})-[r:{edges}]->(b) \
                 RETURN label(r) AS edge, label(b) AS node_type, \
                        coalesce(b.id, b.path, b.name) AS id, b.title AS title \
                 LIMIT {limit}"
            ))?);
        }
        if direction == "in" || direction == "both" {
            out["in"] = json!(self.query(&format!(
                "MATCH (a)-[r:{edges}]->(b:Note {{id: '{id_esc}'}}) \
                 RETURN label(r) AS edge, label(a) AS node_type, \
                        coalesce(a.id, a.path, a.name) AS id, a.title AS title \
                 LIMIT {limit}"
            ))?);
        }
        jtext(out)
    }

    #[tool(description = "Multi-hop provenance walk from a note towards its sources/resources")]
    fn lineage(&self, Parameters(p): Parameters<LineageParams>) -> Result<String, ErrorData> {
        let id = self.resolve_id(&p.id)?;
        let depth = p.max_depth.unwrap_or(3).clamp(1, 5);
        let edges: Vec<String> = self
            .vault
            .config
            .edges
            .iter()
            .map(|e| e.name.clone())
            .collect();
        let rows = self.query(&format!(
            "MATCH (a:Note {{id: '{}'}})-[e:{}*1..{depth}]->(t) \
             RETURN DISTINCT label(t) AS node_type, \
                    coalesce(t.id, t.path) AS id, t.title AS title, \
                    length(e) AS depth \
             ORDER BY depth, id LIMIT 100",
            cypher_escape(&id),
            edges.join("|"),
        ))?;
        jtext(json!({ "root": id, "reachable": rows }))
    }

    #[tool(description = "List notes filtered by kind/status/tag")]
    fn list_notes(
        &self,
        Parameters(p): Parameters<ListNotesParams>,
    ) -> Result<String, ErrorData> {
        let limit = p.limit.unwrap_or(50).min(500);
        let mut conditions = vec![];
        if let Some(k) = &p.kind {
            conditions.push(format!("n.kind = '{}'", cypher_escape(k)));
        }
        if let Some(s) = &p.status {
            conditions.push(format!("n.status = '{}'", cypher_escape(s)));
        }
        if let Some(t) = &p.tag {
            conditions.push(format!("list_contains(n.tags, '{}')", cypher_escape(t)));
        }
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let rows = self.query(&format!(
            "MATCH (n:Note) {where_clause} \
             RETURN n.id AS id, n.title AS title, n.kind AS kind, n.status AS status, \
                    n.updated AS updated \
             ORDER BY n.id LIMIT {limit}"
        ))?;
        jtext(json!({ "notes": rows }))
    }

    #[tool(description = "Semantic (embedding) search over notes — finds conceptually related \
                          content even without keyword overlap")]
    fn semantic_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<String, ErrorData> {
        let k = p.k.unwrap_or(10).min(50) as usize;
        let vec = self.embed_query(&p.query)?;
        let hits = self.with_db(|db| db.vector_search("Note", &vec, k))?;
        if hits.is_empty() {
            return jtext(json!({
                "hits": [],
                "note": "no embeddings in the graph yet — run `cogs sync --with-embeddings`",
            }));
        }
        jtext(json!({ "hits": self.describe(&hits)? }))
    }

    #[tool(description = "Notes semantically similar to a given note (by stored embedding); \
                          surfaces conceptually-close-but-unlinked notes")]
    fn similar_notes(
        &self,
        Parameters(p): Parameters<SimilarNotesParams>,
    ) -> Result<String, ErrorData> {
        let id = self.resolve_id(&p.id)?;
        let k = p.k.unwrap_or(10).min(50) as usize;
        let vec = self
            .with_db(|db| db.note_embedding(&id))?
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("note {id:?} has no embedding (excluded kind, or embeddings not synced)"),
                    None,
                )
            })?;
        let hits: Vec<(String, f64)> = self
            .with_db(|db| db.vector_search("Note", &vec, k + 1))?
            .into_iter()
            .filter(|(other, _)| other != &id)
            .take(k)
            .collect();
        let mut described = self.describe(&hits)?;
        if p.exclude_linked.unwrap_or(false) {
            let edge_alt = self.edge_names().join("|");
            let linked: std::collections::HashSet<String> = self
                .query(&format!(
                    "MATCH (a:Note {{id: '{}'}})-[:{edge_alt}]-(b:Note) RETURN b.id AS id",
                    cypher_escape(&id)
                ))?
                .into_iter()
                .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(String::from))
                .collect();
            described.retain(|d| {
                d.get("id")
                    .and_then(|v| v.as_str())
                    .map(|i| !linked.contains(i))
                    .unwrap_or(true)
            });
        }
        jtext(json!({ "note": id, "similar": described }))
    }

    #[tool(description = "Vault health: orphans, contradiction pairs, stale notes, counts")]
    fn health_report(&self) -> Result<String, ErrorData> {
        let edges: Vec<String> = self
            .vault
            .config
            .edges
            .iter()
            .map(|e| e.name.clone())
            .collect();
        let edge_alt = edges.join("|");
        let orphans = self.query(&format!(
            "MATCH (n:Note) \
             WHERE NOT EXISTS {{ MATCH (n)-[:{edge_alt}]->() }} \
               AND NOT EXISTS {{ MATCH ()-[:{edge_alt}]->(n) }} \
             RETURN n.id AS id, n.title AS title ORDER BY n.id LIMIT 100"
        ))?;
        let contradictions = if edges.iter().any(|e| e == "CONTRADICTS") {
            self.query(
                "MATCH (a:Note)-[:CONTRADICTS]->(b:Note) \
                 RETURN a.id AS source, b.id AS target",
            )?
        } else {
            vec![]
        };
        let stale = match self.vault.config.diagnostics.stale_after_days {
            Some(days) => {
                let cutoff = chrono::Local::now().date_naive()
                    - chrono::Duration::days(days as i64);
                self.query(&format!(
                    "MATCH (n:Note) WHERE n.updated < date('{cutoff}') \
                     RETURN n.id AS id, n.updated AS updated ORDER BY n.updated LIMIT 100"
                ))?
            }
            None => vec![],
        };
        let counts = self.query("MATCH (n:Note) RETURN count(n) AS notes")?;
        jtext(json!({
            "orphans": orphans,
            "contradictions": contradictions,
            "stale": stale,
            "counts": counts.first().cloned().unwrap_or(json!({})),
        }))
    }
}

#[tool_handler]
impl ServerHandler for CogsMcp {
    fn get_info(&self) -> ServerInfo {
        let cfg = &self.vault.config;
        let kinds = if cfg.kinds.values.is_empty() {
            "none declared".to_string()
        } else {
            cfg.kinds.values.join(", ")
        };
        let instructions = format!(
            "Read-only query interface over the cogs knowledge graph for the vault at {}.\n\
             Note ids look like 'dir-slug' (path with '{}' stripped, '/'→'-', no .md).\n\
             Note kinds: {kinds}. Edge types: {}.\n\
             Workflow: `search` to locate notes; `get_note` for full content; \
             `neighbours`/`lineage` to walk the graph; `list_notes` to enumerate; \
             `health_report` for orphans/contradictions/stale pages.",
            self.vault.root.display(),
            cfg.vault.id_strip_prefix,
            self.edge_names().join(", "),
        );
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(instructions);
        info
    }
}

pub async fn run_stdio(vault: Vault) -> AnyResult<()> {
    use rmcp::ServiceExt;
    let server = CogsMcp::new(vault)?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
