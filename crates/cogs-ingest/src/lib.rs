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

mod distill;
pub mod fm_edit;
pub mod git;
mod pipeline;
pub mod prompts;
mod render;
mod retrieve;
mod text;
pub mod training;

use serde::{Deserialize, Serialize};

pub use distill::{distill, DistillOptions, DistillStats};
pub use pipeline::{IngestOptions, IngestReport, Ingester, PlannedWriteView};
pub use retrieve::NearDuplicate;

// ---- typed model outputs (Serialize too: distill re-emits them as targets) --

/// Stage-1 output: everything extracted from one raw capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extraction {
    /// Clean human title for the source page — used when the raw capture's
    /// own frontmatter yields none (filename fallback).
    #[serde(default)]
    pub title: Option<String>,
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

/// Weave-stage output: claims with wikilinks woven in, plus what to create
/// and update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPlan {
    /// The input claims, same order, verbatim apart from inserted `[[...]]`
    /// brackets (enforced in Rust — a rewritten claim reverts to the
    /// original).
    pub linked_claims: Vec<String>,
    #[serde(default)]
    pub new_pages: Vec<NewPageSpec>,
    /// Existing note ids for the source page's `## Cross-references`.
    #[serde(default)]
    pub cross_references: Vec<String>,
    /// Candidate ids whose pages should gain a section from these claims.
    #[serde(default)]
    pub update_targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPageSpec {
    pub slug: String,
    /// "entities" or "concepts".
    pub dir: String,
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub blurb: String,
}

/// One proposed append-only section for an existing page. The heading is
/// rendered in Rust; the model supplies only the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageUpdate {
    pub topic: String,
    #[serde(default)]
    pub section_md: String,
    /// The model may decline: nothing genuinely new for this page.
    #[serde(default = "default_true")]
    pub relevant: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionCheck {
    #[serde(default)]
    pub findings: Vec<ContradictionFinding>,
}

/// A confirmed conflict between an incoming claim and an existing page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionFinding {
    pub page_id: String,
    /// What the existing page says (validated: must appear in its body).
    pub existing_text: String,
    /// The incoming claim it conflicts with.
    pub new_claim: String,
    pub explanation: String,
}
