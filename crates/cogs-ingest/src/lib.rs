//! `cogs ingest` — teacher-LLM-driven ingest of a raw capture into the wiki,
//! following the AGENTS.md workflow: extract → ground → weave → materialise.
//!
//! Division of labour mirrors cogs-ask: Rust owns deterministic control flow
//! and validates every model claim (quotes must be verbatim substrings of the
//! raw body, wikilink targets must resolve against the graph, claims may gain
//! links but never be rewritten); the LLM owns extraction and synthesis.
//! Writes go straight into the working tree — the user reviews via git diff —
//! so the command refuses to run over a dirty note tree.
//!
//! Every teacher call is captured as a training record (JSONL) so the vault's
//! own ingest history becomes the SFT dataset for a small local model
//! (`cogs distill`).

pub mod git;
mod pipeline;
pub mod prompts;
mod render;
mod retrieve;
pub mod training;

use serde::{Deserialize, Serialize};

pub use pipeline::{IngestOptions, IngestReport, Ingester, PlannedWriteView};
pub use retrieve::NearDuplicate;

// ---- typed model outputs (Serialize too: distill re-emits them as targets) --

/// Stage-1 output: everything extracted from one raw capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extraction {
    /// 2-5 sentences, the source page's `## Summary`.
    pub summary: String,
    #[serde(default)]
    pub key_claims: Vec<Claim>,
    #[serde(default)]
    pub quotes: Vec<Quote>,
    #[serde(default)]
    pub entities: Vec<EntityMention>,
    /// Concept-level candidates (things worth wiki pages).
    #[serde(default)]
    pub topics: Vec<String>,
    /// kebab-case slug for the source page filename.
    #[serde(default)]
    pub suggested_slug: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
}

/// A standalone, independently citable factual statement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub text: String,
    /// Entity names mentioned by the claim (used for link candidates).
    #[serde(default)]
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    /// Must be a verbatim substring of the raw body (whitespace-tolerant);
    /// fabrications are dropped in Rust.
    pub text: String,
    #[serde(default)]
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityMention {
    pub name: String,
    /// "entity" (product/protocol/org/person) or "concept".
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub blurb: String,
}

/// A confirmed conflict between an incoming claim and an existing page.
/// Populated by the weave stage (milestone 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionFinding {
    pub page_id: String,
    /// What the existing page says (validated: must appear in its body).
    pub existing_text: String,
    /// The incoming claim it conflicts with.
    pub new_claim: String,
    pub explanation: String,
}
