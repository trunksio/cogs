//! `cogs ask` — closed-domain, multi-step, cited answering over a vault.
//!
//! Hybrid orchestration: Rust owns the deterministic control flow (retrieval,
//! RRF fusion, typed-edge graph expansion, contradiction detection, citation
//! validation, loop bounding); the LLM owns the reasoning (decompose,
//! synthesize). Every claim must cite a retrieved note; the model is told to
//! answer ONLY from the provided context and to say the wiki is silent
//! otherwise. Citations are validated against the retrieved set in Rust, so a
//! model that invents a reference gets it stripped rather than trusted.

pub mod query;

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use cogs_core::config::Vault;
use cogs_graph::embed::EmbeddingProvider;
use cogs_graph::GraphDb;
use cogs_llm::{complete_json, ChatProvider, CompletionParams, Message};

use query::{cypher_escape, cypher_id_list, hybrid_note_search};

/// A note retrieved as evidence.
#[derive(Debug, Clone)]
struct Evidence {
    id: String,
    title: String,
    path: String,
    body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub id: String,
    pub title: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contradiction {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    pub question: String,
    pub text: String,
    pub citations: Vec<Citation>,
    pub contradictions: Vec<Contradiction>,
    pub notes_considered: usize,
    /// True when the model reported the wiki doesn't cover the question.
    pub abstained: bool,
}

pub struct AskConfig {
    /// Seeds kept per sub-question after RRF fusion.
    pub seeds_per_subq: usize,
    /// Graph-expansion hops from seeds (0 disables).
    pub expand_hops: usize,
    /// Hard cap on notes sent to synthesis.
    pub max_notes: usize,
    /// Chars of each note body included in the synthesis context.
    pub body_cap: usize,
}

impl Default for AskConfig {
    fn default() -> Self {
        Self { seeds_per_subq: 6, expand_hops: 1, max_notes: 14, body_cap: 1600 }
    }
}

pub struct Asker<'a> {
    vault: &'a Vault,
    db: &'a GraphDb,
    chat: &'a dyn ChatProvider,
    /// Query embedder for semantic retrieval; None disables vector search.
    embed: Option<&'a dyn EmbeddingProvider>,
    cfg: AskConfig,
}

impl<'a> Asker<'a> {
    pub fn new(
        vault: &'a Vault,
        db: &'a GraphDb,
        chat: &'a dyn ChatProvider,
        embed: Option<&'a dyn EmbeddingProvider>,
    ) -> Self {
        Self { vault, db, chat, embed, cfg: AskConfig::default() }
    }

    pub fn with_config(mut self, cfg: AskConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn ask(&self, question: &str) -> Result<Answer> {
        let subqs = self.decompose(question)?;
        info!(count = subqs.len(), "decomposed question");

        // Retrieve + fuse per sub-question, union into a working set.
        let mut working: HashMap<String, Evidence> = HashMap::new();
        for sq in &subqs {
            for ev in self.retrieve(sq)? {
                working.entry(ev.id.clone()).or_insert(ev);
            }
        }
        // Graph expansion for coverage beyond top-k (the cogs differentiator).
        if self.cfg.expand_hops > 0 && !working.is_empty() {
            let seeds: Vec<String> = working.keys().cloned().collect();
            for ev in self.expand(&seeds, self.cfg.expand_hops)? {
                if working.len() >= self.cfg.max_notes {
                    break;
                }
                working.entry(ev.id.clone()).or_insert(ev);
            }
        }

        let mut evidence: Vec<Evidence> = working.into_values().collect();
        evidence.sort_by(|a, b| a.id.cmp(&b.id));
        evidence.truncate(self.cfg.max_notes);
        debug!(notes = evidence.len(), "retrieved working set");

        let contradictions = self.contradictions_among(&evidence)?;

        if evidence.is_empty() {
            return Ok(Answer {
                question: question.to_string(),
                text: "The wiki is silent on this — no relevant notes were found.".into(),
                citations: vec![],
                contradictions,
                notes_considered: 0,
                abstained: true,
            });
        }

        let synth = self.synthesize(question, &evidence, &contradictions)?;

        // Validate citations against the retrieved set; drop invented ones.
        // On abstention there's nothing being supported, so report no sources.
        let by_id: HashMap<&str, &Evidence> = evidence.iter().map(|e| (e.id.as_str(), e)).collect();
        let citations: Vec<Citation> = if synth.abstained {
            vec![]
        } else {
            synth
                .citations
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
                .map(|e| Citation { id: e.id.clone(), title: e.title.clone(), path: e.path.clone() })
                .collect()
        };

        Ok(Answer {
            question: question.to_string(),
            text: synth.answer,
            citations,
            contradictions,
            notes_considered: evidence.len(),
            abstained: synth.abstained,
        })
    }

    // ---- stage 1: decompose (LLM) ---------------------------------------

    fn decompose(&self, question: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Decomp {
            subquestions: Vec<String>,
        }
        let msgs = [
            Message::system(
                "You split a user's question into 1-4 focused sub-questions for retrieving \
                 evidence from a knowledge base. If the question is already simple, return it \
                 unchanged as a single sub-question. Reply ONLY as JSON: \
                 {\"subquestions\": [\"...\"]}.",
            ),
            Message::user(question),
        ];
        let params = CompletionParams { temperature: 0.0, max_tokens: 400, json: true };
        match complete_json::<Decomp>(self.chat, &msgs, &params) {
            Ok(d) if !d.subquestions.is_empty() => {
                let mut v = d.subquestions;
                v.truncate(4);
                Ok(v)
            }
            // Decomposition is an optimization; fall back to the raw question.
            _ => Ok(vec![question.to_string()]),
        }
    }

    // ---- stage 2: retrieve (Rust: FTS + vector, RRF-fused) ---------------

    fn retrieve(&self, subq: &str) -> Result<Vec<Evidence>> {
        let ids = hybrid_note_search(self.db, self.embed, subq, self.cfg.seeds_per_subq)?;
        let mut out = Vec::new();
        for (id, _) in ids {
            if let Some(ev) = self.fetch(&id)? {
                out.push(ev);
            }
        }
        Ok(out)
    }

    // ---- graph expansion (Rust) -----------------------------------------

    fn expand(&self, seeds: &[String], hops: usize) -> Result<Vec<Evidence>> {
        let mut frontier = seeds.to_vec();
        let mut seen: std::collections::HashSet<String> = seeds.iter().cloned().collect();
        let mut out = Vec::new();
        for _ in 0..hops {
            if frontier.is_empty() {
                break;
            }
            let list = cypher_id_list(&frontier);
            // Undirected 1-hop over any typed edge between notes.
            let q = format!(
                "MATCH (a:Note)-[]-(b:Note) WHERE list_contains({list}, a.id) \
                 RETURN DISTINCT b.id AS id LIMIT 200"
            );
            let rows = self.db.query_json(&q).unwrap_or_default();
            let mut next = Vec::new();
            for r in rows {
                if let Some(id) = r["id"].as_str() {
                    if seen.insert(id.to_string()) {
                        if let Some(ev) = self.fetch(id)? {
                            out.push(ev);
                            next.push(id.to_string());
                        }
                    }
                }
            }
            frontier = next;
        }
        Ok(out)
    }

    // ---- contradiction detection (Rust, deterministic) ------------------

    fn contradictions_among(&self, evidence: &[Evidence]) -> Result<Vec<Contradiction>> {
        let has_contradicts = self.vault.config.edges.iter().any(|e| e.name == "CONTRADICTS");
        if !has_contradicts || evidence.len() < 2 {
            return Ok(vec![]);
        }
        let list = cypher_id_list(&evidence.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
        let q = format!(
            "MATCH (a:Note)-[:CONTRADICTS]->(b:Note) \
             WHERE list_contains({list}, a.id) AND list_contains({list}, b.id) \
             RETURN a.id AS source, b.id AS target"
        );
        let rows = self.db.query_json(&q).unwrap_or_default();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(Contradiction {
                    source: r["source"].as_str()?.to_string(),
                    target: r["target"].as_str()?.to_string(),
                })
            })
            .collect())
    }

    // ---- stage 3: synthesize (LLM) --------------------------------------

    fn synthesize(
        &self,
        question: &str,
        evidence: &[Evidence],
        contradictions: &[Contradiction],
    ) -> Result<SynthOut> {
        let mut ctx = String::new();
        for e in evidence {
            let body: String = e.body.chars().take(self.cfg.body_cap).collect();
            ctx.push_str(&format!("### [{}] {}\n{}\n\n", e.id, e.title, body.trim()));
        }
        if !contradictions.is_empty() {
            ctx.push_str("### Known contradictions (the wiki marks these as conflicting)\n");
            for c in contradictions {
                ctx.push_str(&format!("- [{}] contradicts [{}]\n", c.source, c.target));
            }
            ctx.push('\n');
        }

        let system = "You answer strictly from the provided wiki notes — never from outside \
            knowledge. Each note is delimited by a heading `### [note-id] Title`. Rules:\n\
            - Use ONLY facts present in the notes. If they don't answer the question, say the \
              wiki is silent and set abstained=true.\n\
            - Cite the note-id for every claim, inline as [note-id].\n\
            - If notes conflict, present both sides and name the contradiction rather than \
              picking one silently.\n\
            - Be comprehensive but do not pad.\n\
            Reply ONLY as JSON: {\"answer\": \"markdown with inline [note-id] citations\", \
            \"citations\": [\"note-id\", ...], \"abstained\": false}.";
        let user = format!("Question: {question}\n\n--- WIKI NOTES ---\n{ctx}");

        let params = CompletionParams {
            temperature: 0.1,
            max_tokens: self.vault.config.llm.max_tokens,
            json: true,
        };
        complete_json::<SynthOut>(
            self.chat,
            &[Message::system(system), Message::user(user)],
            &params,
        )
        .context("synthesis step failed")
    }

    // ---- helpers ---------------------------------------------------------

    fn fetch(&self, id: &str) -> Result<Option<Evidence>> {
        let q = format!(
            "MATCH (n:Note {{id: '{}'}}) \
             RETURN n.id AS id, n.title AS title, n.path AS path, n.body_text AS body",
            cypher_escape(id)
        );
        let rows = self.db.query_json(&q)?;
        Ok(rows.into_iter().next().map(|r| Evidence {
            id: r["id"].as_str().unwrap_or(id).to_string(),
            title: r["title"].as_str().unwrap_or("").to_string(),
            path: r["path"].as_str().unwrap_or("").to_string(),
            body: r["body"].as_str().unwrap_or("").to_string(),
        }))
    }
}

#[derive(Debug, Deserialize)]
struct SynthOut {
    answer: String,
    #[serde(default)]
    citations: Vec<String>,
    #[serde(default)]
    abstained: bool,
}

