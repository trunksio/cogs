//! Pure retrieval-text helpers shared by every engine (native ask/ingest,
//! the server, and the browser build). No graph/DB dependencies — these are
//! the parity-critical primitives that must exist exactly once.

use std::collections::HashMap;

/// Reduce free text to a space-joined bag of alphanumeric words so an FTS
/// query parser can't choke on punctuation/operators.
pub fn sanitize_fts(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reciprocal Rank Fusion accumulation (k=60 is the usual constant).
pub fn rrf_merge<'i>(ranks: &mut HashMap<String, f64>, ids: impl Iterator<Item = &'i str>) {
    for (rank, id) in ids.enumerate() {
        *ranks.entry(id.to_string()).or_insert(0.0) += 1.0 / (60.0 + rank as f64);
    }
}

/// Fuse pre-ranked id lists with RRF and return (id, score) best-first,
/// ties broken by id for determinism, truncated to `k`.
pub fn rrf_fuse(lists: &[Vec<String>], k: usize) -> Vec<(String, f64)> {
    let mut ranks: HashMap<String, f64> = HashMap::new();
    for list in lists {
        rrf_merge(&mut ranks, list.iter().map(String::as_str));
    }
    let mut ids: Vec<(String, f64)> = ranks.into_iter().collect();
    ids.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ids.truncate(k);
    ids
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
    fn fuse_is_deterministic_on_ties() {
        let out = rrf_fuse(&[vec!["b".into(), "a".into()], vec!["a".into(), "b".into()]], 10);
        assert_eq!(out[0].0, "a"); // equal scores → id order
        assert_eq!(out.len(), 2);
    }
}
