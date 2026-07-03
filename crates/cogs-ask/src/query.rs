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

/// Reciprocal Rank Fusion accumulation (k=60 is the usual constant).
pub fn rrf_merge<'i>(ranks: &mut HashMap<String, f64>, ids: impl Iterator<Item = &'i str>) {
    for (rank, id) in ids.enumerate() {
        *ranks.entry(id.to_string()).or_insert(0.0) += 1.0 / (60.0 + rank as f64);
    }
}

pub fn cypher_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

pub fn cypher_id_list(ids: &[String]) -> String {
    let inner: Vec<String> = ids.iter().map(|i| format!("'{}'", cypher_escape(i))).collect();
    format!("[{}]", inner.join(", "))
}

/// Reduce free text to a space-joined bag of alphanumeric words so the FTS
/// parser can't choke on punctuation/operators.
pub fn sanitize_fts(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rewards_top_ranks() {
        let mut r = HashMap::new();
        rrf_merge(&mut r, ["a", "b", "c"].into_iter());
        rrf_merge(&mut r, ["b", "a"].into_iter());
        assert!(r["b"] > r["c"]);
        assert!(r["a"] > r["c"]);
    }

    #[test]
    fn fts_sanitize_strips_punctuation() {
        assert_eq!(sanitize_fts("How does A2A's contract work?!"), "How does A2A contract work");
    }

    #[test]
    fn id_list_quotes_and_escapes() {
        assert_eq!(cypher_id_list(&["a".into(), "b'c".into()]), "['a', 'b\\'c']");
    }
}
