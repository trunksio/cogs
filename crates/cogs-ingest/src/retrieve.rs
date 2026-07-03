//! Grounding retrieval: everything the pipeline asks the graph, no LLM here.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use cogs_ask::query::{cypher_id_list, hybrid_note_search};
use cogs_graph::embed::EmbeddingProvider;
use cogs_graph::GraphDb;

/// Minimal metadata for every note in the graph — feeds the link resolver
/// and id → path/title lookups during weaving.
#[derive(Debug, Clone)]
pub struct NoteMeta {
    pub id: String,
    pub slug: String,
    pub dir: String,
    pub path: String,
    pub title: String,
    pub kind: String,
}

pub fn all_notes(db: &GraphDb) -> Result<Vec<NoteMeta>> {
    let rows = db.query_json(
        "MATCH (n:Note) RETURN n.id AS id, n.slug AS slug, n.dir AS dir, n.path AS path, \
         n.title AS title, n.kind AS kind",
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(NoteMeta {
                id: r["id"].as_str()?.to_string(),
                slug: r["slug"].as_str().unwrap_or("").to_string(),
                dir: r["dir"].as_str().unwrap_or("").to_string(),
                path: r["path"].as_str().unwrap_or("").to_string(),
                title: r["title"].as_str().unwrap_or("").to_string(),
                kind: r["kind"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// A page worth showing the weave model as a link/update candidate.
#[derive(Debug, Clone)]
pub struct CandidatePage {
    pub id: String,
    pub title: String,
    pub kind: String,
}

/// Hybrid-search each claim and entity name, RRF-accumulate, and return the
/// top candidates (best-first). Source pages are included — they are valid
/// link/cross-reference targets — but callers exclude them from updates.
pub fn candidate_pages(
    db: &GraphDb,
    embed: Option<&dyn EmbeddingProvider>,
    claims: &[String],
    entity_names: &[String],
    cap: usize,
) -> Result<Vec<CandidatePage>> {
    let mut ranks: HashMap<String, f64> = HashMap::new();
    for claim in claims {
        for (rank, (id, _)) in hybrid_note_search(db, embed, claim, 6)?.iter().enumerate() {
            *ranks.entry(id.clone()).or_insert(0.0) += 1.0 / (60.0 + rank as f64);
        }
    }
    for name in entity_names {
        for (rank, (id, _)) in hybrid_note_search(db, embed, name, 4)?.iter().enumerate() {
            *ranks.entry(id.clone()).or_insert(0.0) += 1.0 / (60.0 + rank as f64);
        }
    }
    let mut ids: Vec<(String, f64)> = ranks.into_iter().collect();
    ids.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ids.truncate(cap);

    let kinds_and_titles = {
        let id_list: Vec<String> = ids.iter().map(|(id, _)| id.clone()).collect();
        if id_list.is_empty() {
            return Ok(vec![]);
        }
        let q = format!(
            "MATCH (n:Note) WHERE list_contains({}, n.id) \
             RETURN n.id AS id, n.title AS title, n.kind AS kind",
            cypher_id_list(&id_list)
        );
        db.query_json(&q).unwrap_or_default()
    };
    let by_id: HashMap<String, (String, String)> = kinds_and_titles
        .into_iter()
        .filter_map(|r| {
            Some((
                r["id"].as_str()?.to_string(),
                (
                    r["title"].as_str().unwrap_or("").to_string(),
                    r["kind"].as_str().unwrap_or("").to_string(),
                ),
            ))
        })
        .collect();
    Ok(ids
        .into_iter()
        .filter_map(|(id, _)| {
            let (title, kind) = by_id.get(&id)?.clone();
            Some(CandidatePage { id, title, kind })
        })
        .collect())
}

/// An existing source page suspiciously close to the incoming capture.
#[derive(Debug, Clone, Serialize)]
pub struct NearDuplicate {
    pub id: String,
    /// "fts" (summary matched) or "vector" (raw body embedding distance).
    pub via: String,
    pub score: f64,
}

/// Vector distance below which an existing source page counts as a
/// near-duplicate. A guess to be tuned on the real vault (folded duplicates
/// exist there as ground truth).
const NEAR_DUP_MAX_DISTANCE: f64 = 0.35;

/// Flag existing source pages the incoming capture probably duplicates:
/// FTS/hybrid on the extracted summary, plus embedding distance from the raw
/// body when embeddings are live. Advisory only — the run continues.
pub fn near_duplicates(
    db: &GraphDb,
    embed: Option<&dyn EmbeddingProvider>,
    summary: &str,
    raw_body: &str,
    embed_char_cap: usize,
) -> Result<Vec<NearDuplicate>> {
    let mut out: Vec<NearDuplicate> = Vec::new();

    let hits = hybrid_note_search(db, None, summary, 8)?;
    let kinds = note_kinds(db, hits.iter().map(|(id, _)| id.clone()).collect())?;
    out.extend(
        hits.into_iter()
            .filter(|(id, _)| kinds.get(id).map(String::as_str) == Some("source"))
            .take(3)
            .map(|(id, score)| NearDuplicate { id, via: "fts".into(), score }),
    );

    if let Some(embed) = embed {
        let body = truncate_chars(raw_body, embed_char_cap);
        if let Ok(vec) = embed.embed_query(body) {
            if let Ok(hits) = db.vector_search("Note", &vec, 8) {
                let kinds = note_kinds(db, hits.iter().map(|(id, _)| id.clone()).collect())?;
                for (id, dist) in hits {
                    if dist < NEAR_DUP_MAX_DISTANCE
                        && kinds.get(&id).map(String::as_str) == Some("source")
                        && !out.iter().any(|d| d.id == id)
                    {
                        out.push(NearDuplicate { id, via: "vector".into(), score: dist });
                    }
                }
            }
        }
    }
    Ok(out)
}

fn note_kinds(db: &GraphDb, ids: Vec<String>) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let q = format!(
        "MATCH (n:Note) WHERE list_contains({}, n.id) RETURN n.id AS id, n.kind AS kind",
        cypher_id_list(&ids)
    );
    let rows = db.query_json(&q).unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some((r["id"].as_str()?.to_string(), r["kind"].as_str().unwrap_or("").to_string()))
        })
        .collect())
}

/// Truncate at a char boundary at or below `cap` bytes.
pub fn truncate_chars(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
