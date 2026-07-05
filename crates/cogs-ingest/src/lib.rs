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
pub use cogs_llm::training;

use serde::{Deserialize, Serialize};

pub use distill::{distill, DistillOptions, DistillStats};
pub use pipeline::{IngestOptions, IngestReport, Ingester, PlannedWriteView};
pub use retrieve::NearDuplicate;

// ---- typed model outputs (Serialize too: distill re-emits them as targets) --

/// Stage-1 output: everything extracted from one raw capture.
#[derive(Debug, Clone, Serialize)]
pub struct Extraction {
    /// Clean human title for the source page — used when the raw capture's
    /// own frontmatter yields none (filename fallback).
    pub title: Option<String>,
    /// 2-5 sentences, the source page's `## Summary`. Deserialization is
    /// lenient (chunk/merge partials may omit it); the pipeline still fails
    /// an ingest whose FINAL extraction has no summary.
    pub summary: String,
    pub key_claims: Vec<Claim>,
    pub quotes: Vec<Quote>,
    pub entities: Vec<EntityMention>,
    /// Concept-level candidates (things worth wiki pages).
    pub topics: Vec<String>,
    /// kebab-case slug for the source page filename.
    pub suggested_slug: String,
    pub tags: Vec<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
}

// Tolerates the whole extraction arriving as a bare array of claims — the
// teacher sometimes flattens the reply to just key_claims.
impl<'de> Deserialize<'de> for Extraction {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Obj {
            title: Option<String>,
            summary: String,
            #[serde(deserialize_with = "de_claims")]
            key_claims: Vec<Claim>,
            #[serde(deserialize_with = "de_quotes")]
            quotes: Vec<Quote>,
            #[serde(deserialize_with = "de_entities")]
            entities: Vec<EntityMention>,
            #[serde(deserialize_with = "de_string_list")]
            topics: Vec<String>,
            suggested_slug: String,
            #[serde(deserialize_with = "de_string_list")]
            tags: Vec<String>,
            author: Option<String>,
            publisher: Option<String>,
        }
        #[derive(Deserialize)]
        struct ClaimsField(#[serde(deserialize_with = "de_claims")] Vec<Claim>);
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Obj(Obj),
            Claims(ClaimsField),
        }
        Ok(match Wire::deserialize(d)? {
            Wire::Obj(o) => Extraction {
                title: o.title,
                summary: o.summary,
                key_claims: o.key_claims,
                quotes: o.quotes,
                entities: o.entities,
                topics: o.topics,
                suggested_slug: o.suggested_slug,
                tags: o.tags,
                author: o.author,
                publisher: o.publisher,
            },
            Wire::Claims(ClaimsField(key_claims)) => Extraction {
                title: None,
                summary: String::new(),
                key_claims,
                quotes: vec![],
                entities: vec![],
                topics: vec![],
                suggested_slug: String::new(),
                tags: vec![],
                author: None,
                publisher: None,
            },
        })
    }
}

/// A standalone, independently citable factual statement. All fields are
/// deserialization-lenient (truncation repair can produce empty items);
/// validation filters empties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    #[serde(default)]
    pub text: String,
    /// Entity names mentioned by the claim (used for link candidates).
    #[serde(default, deserialize_with = "de_string_list")]
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    /// Must be a verbatim substring of the raw body (whitespace-tolerant);
    /// fabrications are dropped in Rust.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityMention {
    #[serde(default)]
    pub name: String,
    /// "entity" (product/protocol/org/person) or "concept".
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub blurb: String,
}

// Local models under json mode routinely flatten structure: object lists
// become plain strings ("entities": ["Apache Airflow"]) and whole arrays
// become one newline/numbered blob ("linked_claims": "1. …\n2. …"). Accept
// those shapes on the way in; serialization (training targets) stays
// canonical.

/// Strip a leading "- ", "3. " or "3) " list marker.
fn strip_list_marker(line: &str) -> &str {
    let t = line.trim();
    if let Some(r) = t.strip_prefix("- ") {
        return r.trim();
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &t[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return r.trim();
        }
    }
    t
}

/// Split a blob the model should have returned as an array: one item per
/// non-empty line, list markers stripped.
fn split_listish(s: &str) -> Vec<String> {
    s.lines()
        .map(strip_list_marker)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// A list that may arrive as a proper array, a keyed map ({"1": …}), or one
/// string blob.
#[derive(Deserialize)]
#[serde(untagged)]
enum ListOrBlob<T> {
    List(Vec<T>),
    Map(std::collections::BTreeMap<String, T>),
    Blob(String),
}

impl<T> ListOrBlob<T> {
    /// Normalize to a Vec; map keys sort numerically when they are numbers
    /// ("10" after "2"), lexically otherwise. Blobs go through `f`.
    fn into_vec(self, f: impl Fn(String) -> T) -> Vec<T> {
        match self {
            ListOrBlob::List(v) => v,
            ListOrBlob::Map(m) => {
                let mut entries: Vec<(String, T)> = m.into_iter().collect();
                entries.sort_by(|(a, _), (b, _)| match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    _ => a.cmp(b),
                });
                entries.into_iter().map(|(_, v)| v).collect()
            }
            ListOrBlob::Blob(s) => split_listish(&s).into_iter().map(f).collect(),
        }
    }
}

fn de_string_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(ListOrBlob::<String>::deserialize(d)?.into_vec(|s| s))
}

fn de_claims<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Claim>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum X {
        Full(Claim),
        Text(String),
    }
    Ok(ListOrBlob::<X>::deserialize(d)?
        .into_vec(X::Text)
        .into_iter()
        .map(|x| match x {
            X::Full(c) => c,
            X::Text(text) => Claim { text: strip_list_marker(&text).to_string(), entities: vec![] },
        })
        .collect())
}

fn de_quotes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Quote>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum X {
        Full(Quote),
        Text(String),
    }
    Ok(ListOrBlob::<X>::deserialize(d)?
        .into_vec(X::Text)
        .into_iter()
        .map(|x| match x {
            X::Full(q) => q,
            X::Text(text) => Quote { text, location: String::new() },
        })
        .collect())
}

fn de_entities<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<EntityMention>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum X {
        Full(EntityMention),
        Name(String),
    }
    Ok(ListOrBlob::<X>::deserialize(d)?
        .into_vec(X::Name)
        .into_iter()
        .map(|x| match x {
            X::Full(e) => e,
            X::Name(name) => {
                EntityMention { name, kind: "entity".into(), blurb: String::new() }
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_tolerates_flattened_lists() {
        let ex: Extraction = serde_json::from_str(
            r#"{"summary": "s",
                "key_claims": ["plain claim", {"text": "full claim", "entities": ["E"]}],
                "quotes": ["just text"],
                "entities": ["Apache Airflow", {"name": "A2A", "kind": "concept", "blurb": "b"}]}"#,
        )
        .unwrap();
        assert_eq!(ex.key_claims[0].text, "plain claim");
        assert_eq!(ex.key_claims[1].entities, vec!["E"]);
        assert_eq!(ex.quotes[0].text, "just text");
        assert_eq!(ex.entities[0].name, "Apache Airflow");
        assert_eq!(ex.entities[0].kind, "entity");
        assert_eq!(ex.entities[1].kind, "concept");
    }

    #[test]
    fn blob_lists_split_into_items() {
        let ex: Extraction = serde_json::from_str(
            r#"{"summary": "s",
                "key_claims": "1. First claim here.\n2. Second claim.",
                "entities": "- Apache Airflow\n- A2A"}"#,
        )
        .unwrap();
        assert_eq!(ex.key_claims.len(), 2);
        assert_eq!(ex.key_claims[0].text, "First claim here.");
        assert_eq!(ex.entities[1].name, "A2A");

        let plan: LinkPlan = serde_json::from_str(
            r#"{"linked_claims": "1. Anthropic revised its [[terms]].\n2. Second.",
                "update_targets": "concepts/x"}"#,
        )
        .unwrap();
        assert_eq!(plan.linked_claims.len(), 2);
        assert_eq!(plan.linked_claims[0], "Anthropic revised its [[terms]].");
        assert_eq!(plan.update_targets, vec!["concepts/x"]);
    }

    #[test]
    fn extraction_tolerates_bare_claims_array() {
        let ex: Extraction = serde_json::from_str(
            r#"[{"text": "Groq's LPU uses on-chip SRAM.", "entities": ["Groq"]}]"#,
        )
        .unwrap();
        assert_eq!(ex.key_claims.len(), 1);
        assert!(ex.summary.is_empty()); // validation decides what to do with that
    }

    #[test]
    fn map_shaped_lists_normalize_in_numeric_order() {
        let ex: Extraction = serde_json::from_str(
            r#"{"summary": "s",
                "key_claims": {"1": "first", "2": {"text": "second", "entities": []}, "10": "tenth"},
                "entities": {"a": "Airflow"}}"#,
        )
        .unwrap();
        let texts: Vec<&str> = ex.key_claims.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second", "tenth"]);
        assert_eq!(ex.entities[0].name, "Airflow");
    }
}

/// Weave-stage output: claims with wikilinks woven in, plus what to create
/// and update.
#[derive(Debug, Clone, Serialize)]
pub struct LinkPlan {
    /// The input claims, same order, verbatim apart from inserted `[[...]]`
    /// brackets (enforced in Rust — a rewritten claim reverts to the
    /// original).
    pub linked_claims: Vec<String>,
    pub new_pages: Vec<NewPageSpec>,
    /// Existing note ids for the source page's `## Cross-references`.
    pub cross_references: Vec<String>,
    /// Candidate ids whose pages should gain a section from these claims.
    pub update_targets: Vec<String>,
}

// Tolerates the whole plan arriving as a bare array of linked claims — a
// shape local models actually produce.
impl<'de> Deserialize<'de> for LinkPlan {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Obj {
            #[serde(default, deserialize_with = "de_string_list")]
            linked_claims: Vec<String>,
            #[serde(default)]
            new_pages: Vec<NewPageSpec>,
            #[serde(default, deserialize_with = "de_string_list")]
            cross_references: Vec<String>,
            #[serde(default, deserialize_with = "de_string_list")]
            update_targets: Vec<String>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Obj(Obj),
            Claims(Vec<String>),
        }
        Ok(match Wire::deserialize(d)? {
            Wire::Obj(o) => LinkPlan {
                linked_claims: o.linked_claims,
                new_pages: o.new_pages,
                cross_references: o.cross_references,
                update_targets: o.update_targets,
            },
            Wire::Claims(linked_claims) => LinkPlan {
                linked_claims,
                new_pages: vec![],
                cross_references: vec![],
                update_targets: vec![],
            },
        })
    }
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

#[derive(Debug, Clone, Serialize)]
pub struct ContradictionCheck {
    pub findings: Vec<ContradictionFinding>,
}

// Tolerates a bare findings array (models often skip the wrapper object).
impl<'de> Deserialize<'de> for ContradictionCheck {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Obj {
            #[serde(default)]
            findings: Vec<ContradictionFinding>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Obj(Obj),
            Bare(Vec<ContradictionFinding>),
        }
        Ok(match Wire::deserialize(d)? {
            Wire::Obj(o) => ContradictionCheck { findings: o.findings },
            Wire::Bare(findings) => ContradictionCheck { findings },
        })
    }
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
