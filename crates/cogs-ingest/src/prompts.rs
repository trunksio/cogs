//! Every prompt template the ingest pipeline sends to the teacher LLM.
//!
//! Single source of truth shared with `cogs distill`: mined/captured training
//! pairs are emitted under these exact templates, so a model fine-tuned on the
//! dataset is a drop-in replacement for the teacher at runtime. Keep changes
//! deliberate — editing a template invalidates comparability with previously
//! distilled datasets.

use cogs_llm::{CompletionParams, Message};

pub const EXTRACT_SYSTEM: &str = "\
You are the ingest engine for a Karpathy-style wiki (immutable raw captures \
distilled into a curated, wikilinked synthesis layer). Given one raw captured \
document, extract:\n\
- summary: 2-5 sentences, in your own words, dense and factual.\n\
- key_claims: 3-12 standalone, independently citable factual statements. Each \
must be understandable without the others and without the document. List the \
entity/concept names each claim mentions.\n\
- quotes: up to 6 short verbatim quotes worth preserving. Each `text` MUST be \
an exact substring of the document — do not paraphrase, fix typos, or adjust \
punctuation. `location` is a short hint (section name or position).\n\
- entities: named things (products, protocols, organisations, people, papers) \
with kind \"entity\", plus recurring ideas with kind \"concept\". Include a \
one-sentence blurb for each.\n\
- topics: broader concept-level themes the document contributes to.\n\
- suggested_slug: a short kebab-case filename slug for the source page.\n\
- tags: 2-6 lowercase topic tags.\n\
- author / publisher: from the document if evident, else null.\n\
Reply ONLY as JSON: {\"summary\": \"...\", \"key_claims\": [{\"text\": \"...\", \
\"entities\": [\"...\"]}], \"quotes\": [{\"text\": \"...\", \"location\": \
\"...\"}], \"entities\": [{\"name\": \"...\", \"kind\": \"entity|concept\", \
\"blurb\": \"...\"}], \"topics\": [\"...\"], \"suggested_slug\": \"kebab-case\", \
\"tags\": [\"...\"], \"author\": null, \"publisher\": null}";

pub const MERGE_SYSTEM: &str = "\
You merge partial extractions of ONE long document (it was extracted \
section-by-section) into a single coherent extraction. Unify the summary, \
dedupe claims/entities/topics/tags, keep the strongest at most 12 key_claims \
and 6 quotes, and pick the best suggested_slug. Do not invent anything absent \
from the parts; quotes must be carried over unchanged. Reply ONLY as JSON with \
the same schema as the parts.";

/// Stage-1 extraction over one raw document (or one chunk of it).
pub fn extract_messages(title: &str, url: Option<&str>, body: &str) -> Vec<Message> {
    let mut header = format!("title: {title}\n");
    if let Some(url) = url {
        header.push_str(&format!("url: {url}\n"));
    }
    vec![
        Message::system(EXTRACT_SYSTEM),
        Message::user(format!("{header}\n{body}")),
    ]
}

/// Merge per-chunk extractions of a long document into one.
pub fn merge_messages(part_jsons: &[String]) -> Vec<Message> {
    let mut user = String::from("Partial extractions, in document order:\n");
    for (i, p) in part_jsons.iter().enumerate() {
        user.push_str(&format!("\n--- part {} ---\n{p}\n", i + 1));
    }
    vec![Message::system(MERGE_SYSTEM), Message::user(user)]
}

pub fn extract_params(max_tokens: u32) -> CompletionParams {
    CompletionParams { temperature: 0.0, max_tokens, json: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prompt_carries_schema_and_inputs() {
        let msgs = extract_messages("T", Some("https://x"), "BODY");
        assert!(msgs[0].content.contains("\"key_claims\""));
        assert!(msgs[0].content.contains("suggested_slug"));
        assert!(msgs[1].content.contains("title: T"));
        assert!(msgs[1].content.contains("url: https://x"));
        assert!(msgs[1].content.ends_with("BODY"));
    }

    #[test]
    fn merge_prompt_numbers_parts() {
        let msgs = merge_messages(&["{\"a\":1}".into(), "{\"b\":2}".into()]);
        assert!(msgs[1].content.contains("--- part 1 ---"));
        assert!(msgs[1].content.contains("--- part 2 ---"));
    }
}
