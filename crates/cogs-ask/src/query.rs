//! Shared retrieval primitives over the graph DB, used by the ask pipeline
//! and by ingest (link/context candidate retrieval, near-duplicate detection).

use std::collections::HashMap;

use anyhow::Result;
use cogs_graph::embed::EmbeddingProvider;
use cogs_graph::GraphDb;

/// Hybrid FTS + vector search over Note, RRF-fused. `embed = None` degrades to
/// FTS only. Returns (note id, fused score) best-first, at most `k`.
pub fn hybrid_note_search(
    db: &GraphDb,
    embed: Option<&dyn EmbeddingProvider>,
    query: &str,
    k: usize,
) -> Result<Vec<(String, f64)>> {
    let mut ranks: HashMap<String, f64> = HashMap::new();

    // BM25 full-text. Sanitize to a bag of words so punctuation can't break
    // the FTS query parser.
    let terms = sanitize_fts(query);
    if !terms.is_empty() {
        let q = format!(
            "CALL QUERY_FTS_INDEX('Note', 'note_fts', '{}') \
             RETURN node.id AS id, score ORDER BY score DESC LIMIT {}",
            cypher_escape(&terms),
            k * 2
        );
        if let Ok(rows) = db.query_json(&q) {
            rrf_merge(&mut ranks, rows.iter().filter_map(|r| r["id"].as_str()));
        }
    }

    // Vector semantic search (skipped if embeddings disabled/unavailable).
    if let Some(embed) = embed {
        if let Ok(vec) = embed.embed_query(query) {
            if let Ok(hits) = db.vector_search("Note", &vec, k * 2) {
                rrf_merge(&mut ranks, hits.iter().map(|(id, _)| id.as_str()));
            }
        }
    }

    let mut ids: Vec<(String, f64)> = ranks.into_iter().collect();
    ids.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ids.truncate(k);
    Ok(ids)
}

pub use cogs_core::textquery::rrf_merge;

pub fn cypher_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

pub fn cypher_id_list(ids: &[String]) -> String {
    let inner: Vec<String> = ids.iter().map(|i| format!("'{}'", cypher_escape(i))).collect();
    format!("[{}]", inner.join(", "))
}

pub use cogs_core::textquery::sanitize_fts;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_list_quotes_and_escapes() {
        assert_eq!(cypher_id_list(&["a".into(), "b'c".into()]), "['a', 'b\\'c']");
    }
}
