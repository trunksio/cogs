//! HTTP API for the graph-visualization web app (and anything else local).
//! Binds 127.0.0.1 only. The built web app is embedded via rust-embed.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::info;

use cogs_core::config::Vault;
use cogs_graph::GraphDb;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

pub struct AppState {
    vault: Vault,
    /// Writer handle when this process won the role; otherwise read-only
    /// opens per request (sees the LSP's latest checkpoint).
    primary_db: Option<Mutex<GraphDb>>,
}

impl AppState {
    fn query(&self, cypher: &str) -> anyhow::Result<Vec<Value>> {
        self.with_db(|db| db.query_json(cypher))
    }

    fn with_db<T>(&self, f: impl FnOnce(&GraphDb) -> anyhow::Result<T>) -> anyhow::Result<T> {
        match &self.primary_db {
            Some(db) => f(&db.lock().unwrap()),
            None => f(&GraphDb::open_ro(&self.vault)?),
        }
    }

    fn edge_names(&self) -> Vec<String> {
        self.vault.config.edges.iter().map(|e| e.name.clone()).collect()
    }

    /// Note→note edge pairs (for `linked` flags on similarity results).
    fn linked_pairs(&self) -> anyhow::Result<std::collections::HashSet<(String, String)>> {
        let mut set = std::collections::HashSet::new();
        for e in &self.vault.config.edges {
            if matches!(e.target, cogs_core::config::EdgeTarget::Resource) {
                continue;
            }
            for row in self.query(&format!(
                "MATCH (a:Note)-[:{}]->(b:Note) RETURN a.id AS a, b.id AS b",
                e.name
            ))? {
                if let (Some(a), Some(b)) = (
                    row.get("a").and_then(|v| v.as_str()),
                    row.get("b").and_then(|v| v.as_str()),
                ) {
                    let (x, y) = if a < b { (a, b) } else { (b, a) };
                    set.insert((x.to_string(), y.to_string()));
                }
            }
        }
        Ok(set)
    }
}

type SharedState = Arc<AppState>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

async fn api_meta(State(st): State<SharedState>) -> impl IntoResponse {
    let cfg = &st.vault.config;
    Json(json!({
        "vault_root": st.vault.root.to_string_lossy(),
        "kinds": cfg.kinds.values,
        "edges": st.edge_names(),
        "has_resources": cfg.resources.is_some(),
        "stale_after_days": cfg.diagnostics.stale_after_days,
    }))
}

#[derive(Deserialize)]
struct GraphParams {
    /// "true" to include Resource nodes and note→resource edges.
    #[serde(default)]
    resources: Option<String>,
}

async fn api_graph(
    State(st): State<SharedState>,
    Query(p): Query<GraphParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let include_resources =
        p.resources.as_deref() == Some("true") && st.vault.config.resources.is_some();

    let nodes = st
        .query(
            "MATCH (n:Note) RETURN n.id AS id, n.title AS label, n.kind AS kind, \
             n.status AS status, n.updated AS updated, n.tags AS tags, \
             n.dir AS dir, n.path AS path",
        )
        .map_err(err500)?;

    let mut edges: Vec<Value> = Vec::new();
    for e in &st.vault.config.edges {
        let to_resource = matches!(
            e.target,
            cogs_core::config::EdgeTarget::Resource
        );
        if to_resource && !include_resources {
            continue;
        }
        let dst = if to_resource { "b.path" } else { "b.id" };
        let rows = st
            .query(&format!(
                "MATCH (a:Note)-[r:{}]->(b) RETURN a.id AS source, {dst} AS target",
                e.name
            ))
            .map_err(err500)?;
        for mut row in rows {
            row["type"] = json!(e.name);
            edges.push(row);
        }
    }

    let resources = if include_resources {
        st.query(
            "MATCH (r:Resource) RETURN r.path AS id, r.title AS label, \
             r.captured AS captured, r.source_date AS source_date",
        )
        .map_err(err500)?
    } else {
        vec![]
    };

    Ok(Json(json!({ "nodes": nodes, "edges": edges, "resources": resources })))
}

async fn api_note(
    State(st): State<SharedState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id_esc = esc(&id);
    let rows = st
        .query(&format!(
            "MATCH (n:Note {{id: '{id_esc}'}}) \
             RETURN n.id AS id, n.path AS path, n.title AS title, n.kind AS kind, \
                    n.status AS status, n.updated AS updated, n.tags AS tags, \
                    n.frontmatter_json AS frontmatter"
        ))
        .map_err(err500)?;
    let Some(mut note) = rows.into_iter().next() else {
        return Err((StatusCode::NOT_FOUND, format!("no note {id}")));
    };
    if let Some(path) = note.get("path").and_then(|v| v.as_str()) {
        let abs = st.vault.root.join(path);
        note["abs_path"] = json!(abs.to_string_lossy());
        note["markdown"] = json!(std::fs::read_to_string(&abs).unwrap_or_default());
    }
    let edge_alt = st.edge_names().join("|");
    note["outlinks"] = json!(st
        .query(&format!(
            "MATCH (a:Note {{id: '{id_esc}'}})-[r:{edge_alt}]->(b) \
             RETURN label(r) AS type, coalesce(b.id, b.path) AS id, b.title AS title LIMIT 100"
        ))
        .map_err(err500)?);
    note["backlinks"] = json!(st
        .query(&format!(
            "MATCH (a:Note)-[r:{edge_alt}]->(b:Note {{id: '{id_esc}'}}) \
             RETURN label(r) AS type, a.id AS id, a.title AS title LIMIT 100"
        ))
        .map_err(err500)?);
    Ok(Json(note))
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default)]
    k: Option<u32>,
}

async fn api_search(
    State(st): State<SharedState>,
    Query(p): Query<SearchParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let k = p.k.unwrap_or(20).min(100);
    let rows = st
        .query(&format!(
            "CALL QUERY_FTS_INDEX('Note', 'note_fts', '{}') \
             RETURN node.id AS id, node.title AS title, node.kind AS kind, score \
             ORDER BY score DESC LIMIT {k}",
            esc(&p.q)
        ))
        .map_err(err500)?;
    Ok(Json(json!({ "hits": rows })))
}

async fn api_lineage(
    State(st): State<SharedState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let edge_alt = st.edge_names().join("|");
    let id_esc = esc(&id);
    // Forward (towards sources) and backward (what derives from this).
    let down = st
        .query(&format!(
            "MATCH p = (a:Note {{id: '{id_esc}'}})-[e:{edge_alt}*1..4]->(t) \
             RETURN DISTINCT label(t) AS node_type, coalesce(t.id, t.path) AS id, \
                    t.title AS title, length(e) AS depth ORDER BY depth LIMIT 200"
        ))
        .map_err(err500)?;
    let up = st
        .query(&format!(
            "MATCH p = (s)-[e:{edge_alt}*1..4]->(a:Note {{id: '{id_esc}'}}) \
             RETURN DISTINCT label(s) AS node_type, coalesce(s.id, s.path) AS id, \
                    s.title AS title, length(e) AS depth ORDER BY depth LIMIT 200"
        ))
        .map_err(err500)?;
    Ok(Json(json!({ "root": id, "down": down, "up": up })))
}

#[derive(Deserialize)]
struct SimilarParams {
    note: String,
    #[serde(default)]
    k: Option<usize>,
}

/// Nearest neighbours of one note (detail panel).
async fn api_similar(
    State(st): State<SharedState>,
    Query(p): Query<SimilarParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let k = p.k.unwrap_or(8).min(50);
    let result = st
        .with_db(|db| {
            let Some(vec) = db.note_embedding(&p.note)? else {
                return Ok(None);
            };
            Ok(Some(db.vector_search("Note", &vec, k + 1)?))
        })
        .map_err(err500)?;
    let Some(hits) = result else {
        return Ok(Json(json!({ "note": p.note, "neighbors": [], "no_embedding": true })));
    };
    let linked = st.linked_pairs().map_err(err500)?;
    let neighbors: Vec<Value> = hits
        .into_iter()
        .filter(|(id, _)| id != &p.note)
        .take(k)
        .map(|(id, dist)| {
            let pair = if p.note < id {
                (p.note.clone(), id.clone())
            } else {
                (id.clone(), p.note.clone())
            };
            json!({ "id": id, "score": 1.0 - dist, "linked": linked.contains(&pair) })
        })
        .collect();
    Ok(Json(json!({ "note": p.note, "neighbors": neighbors })))
}

#[derive(Deserialize)]
struct SimilarityParams {
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    min_score: Option<f64>,
    #[serde(default)]
    unlinked_only: Option<bool>,
}

/// Whole-graph kNN pairs (semantic overlay + auto-link suggestions).
async fn api_similarity(
    State(st): State<SharedState>,
    Query(p): Query<SimilarityParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let k = p.k.unwrap_or(5).min(10);
    let min_score = p.min_score.unwrap_or(0.6);
    let unlinked_only = p.unlinked_only.unwrap_or(false);

    let embeddings: Vec<(String, Vec<f32>)> = st
        .with_db(|db| {
            let rows = db.query_json(
                "MATCH (n:Note) WHERE n.embedding IS NOT NULL RETURN n.id AS id, n.embedding AS e",
            )?;
            Ok(rows
                .into_iter()
                .filter_map(|r| {
                    let id = r.get("id")?.as_str()?.to_string();
                    let e = r
                        .get("e")?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                        .collect();
                    Some((id, e))
                })
                .collect())
        })
        .map_err(err500)?;

    if embeddings.is_empty() {
        return Ok(Json(json!({ "pairs": [], "no_embeddings": true })));
    }
    let linked = st.linked_pairs().map_err(err500)?;
    let mut seen = std::collections::HashSet::new();
    let mut pairs: Vec<Value> = Vec::new();
    st.with_db(|db| {
        for (id, vec) in &embeddings {
            for (other, dist) in db.vector_search("Note", vec, k + 1)? {
                if &other == id {
                    continue;
                }
                let score = 1.0 - dist;
                if score < min_score {
                    continue;
                }
                let key = if *id < other {
                    (id.clone(), other.clone())
                } else {
                    (other.clone(), id.clone())
                };
                if !seen.insert(key.clone()) {
                    continue;
                }
                let is_linked = linked.contains(&key);
                if unlinked_only && is_linked {
                    continue;
                }
                pairs.push(json!({
                    "source": key.0, "target": key.1,
                    "score": score, "linked": is_linked,
                }));
            }
        }
        Ok(())
    })
    .map_err(err500)?;
    pairs.sort_by(|a, b| {
        b["score"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["score"].as_f64().unwrap_or(0.0))
    });
    Ok(Json(json!({ "pairs": pairs })))
}

async fn api_health(State(st): State<SharedState>) -> Result<Json<Value>, (StatusCode, String)> {
    let edge_alt = st.edge_names().join("|");
    let orphans = st
        .query(&format!(
            "MATCH (n:Note) \
             WHERE NOT EXISTS {{ MATCH (n)-[:{edge_alt}]->() }} \
               AND NOT EXISTS {{ MATCH ()-[:{edge_alt}]->(n) }} \
             RETURN n.id AS id ORDER BY n.id LIMIT 500"
        ))
        .map_err(err500)?;
    let contradictions = if st.edge_names().iter().any(|e| e == "CONTRADICTS") {
        st.query(
            "MATCH (a:Note)-[:CONTRADICTS]->(b:Note) RETURN a.id AS source, b.id AS target",
        )
        .map_err(err500)?
    } else {
        vec![]
    };
    let stale = match st.vault.config.diagnostics.stale_after_days {
        Some(days) => {
            let cutoff =
                chrono::Local::now().date_naive() - chrono::Duration::days(days as i64);
            st.query(&format!(
                "MATCH (n:Note) WHERE n.updated < date('{cutoff}') \
                 RETURN n.id AS id, n.updated AS updated ORDER BY n.updated LIMIT 500"
            ))
            .map_err(err500)?
        }
        None => vec![],
    };
    Ok(Json(json!({
        "orphans": orphans.into_iter().filter_map(|o| o.get("id").cloned()).collect::<Vec<_>>(),
        "contradictions": contradictions,
        "stale": stale,
    })))
}

#[derive(Deserialize)]
struct OpenParams {
    id: String,
    #[serde(default)]
    line: Option<u32>,
}

/// Open a note in Zed. Loopback-only server + same-origin check + the path is
/// looked up from the DB (never taken from the request), so this can't be
/// used to exec arbitrary input.
async fn api_open(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(p): Json<OpenParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if site != "same-origin" && site != "none" {
            return Err((StatusCode::FORBIDDEN, "cross-origin open rejected".into()));
        }
    }
    let rows = st
        .query(&format!(
            "MATCH (n:Note {{id: '{}'}}) RETURN n.path AS path",
            esc(&p.id)
        ))
        .map_err(err500)?;
    let Some(path) = rows
        .first()
        .and_then(|r| r.get("path"))
        .and_then(|v| v.as_str())
    else {
        return Err((StatusCode::NOT_FOUND, format!("no note {}", p.id)));
    };
    let abs = st.vault.root.join(path);
    if !abs.starts_with(&st.vault.root) || !abs.is_file() {
        return Err((StatusCode::FORBIDDEN, "path outside vault".into()));
    }
    let target = match p.line {
        Some(line) => format!("{}:{line}", abs.to_string_lossy()),
        None => abs.to_string_lossy().into_owned(),
    };
    Command::new("zed")
        .arg(&target)
        .spawn()
        .map_err(|e| err500(format!("could not launch zed: {e}")))?;
    Ok(Json(json!({ "opened": target })))
}

async fn static_assets(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(path).or_else(|| WebAssets::get("index.html")) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string())], content.data).into_response()
        }
        None => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html")],
            "<h1>cogs</h1><p>The viz app isn't embedded in this build. \
             Run <code>just web</code> then rebuild, or use <code>npm run dev</code> \
             in web/ against this server.</p>",
        )
            .into_response(),
    }
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/meta", get(api_meta))
        .route("/api/graph", get(api_graph))
        .route("/api/notes/{id}", get(api_note))
        .route("/api/search", get(api_search))
        .route("/api/similar", get(api_similar))
        .route("/api/similarity", get(api_similarity))
        .route("/api/lineage/{id}", get(api_lineage))
        .route("/api/health", get(api_health))
        .route("/api/open", post(api_open))
        .fallback(static_assets)
        .with_state(state)
}

pub async fn serve(vault: Vault, port: u16) -> Result<()> {
    // Become the writer when free (keeps data fresh standalone); otherwise
    // read per request and let the LSP process own writes.
    let primary_db = match GraphDb::open_rw(&vault, false) {
        Ok(db) => {
            let engine = cogs_graph::SyncEngine::new(&vault)?;
            if let Err(e) = engine.sync(&db, false) {
                tracing::warn!("initial sync failed: {e:#}");
            }
            Some(Mutex::new(db))
        }
        Err(e) => {
            info!("serving read-only ({e})");
            None
        }
    };
    let state: SharedState = Arc::new(AppState { vault, primary_db });
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("cogs viz at http://{addr}/");
    println!("cogs viz at http://{addr}/");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
