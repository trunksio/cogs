//! Grounding retrieval: everything the pipeline asks the graph, no LLM here.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use cogs_ask::query::{cypher_id_list, hybrid_note_search};
use cogs_graph::embed::EmbeddingProvider;
use cogs_graph::GraphDb;

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
