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
//! The typed model outputs (tolerant deserialization) and the hard validators
//! live in `cogs-ingest-core` — a pure, wasm32-safe crate — so browser-side
//! ingest (Cogitarium via cogs-wasm) runs the SAME quality gate. This crate
//! re-exports them; external callers keep using `cogs_ingest::Extraction` etc.
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
pub use cogs_llm::training;

pub use cogs_ingest_core::validators;
pub use cogs_ingest_core::{
    Claim, ContradictionCheck, ContradictionFinding, EntityMention, Extraction, LinkPlan,
    NewPageSpec, PageUpdate, Quote,
};
pub(crate) use cogs_ingest_core::text;

pub use distill::{distill, DistillOptions, DistillStats};
pub use pipeline::{IngestOptions, IngestReport, Ingester, PlannedWriteView};
pub use retrieve::NearDuplicate;
