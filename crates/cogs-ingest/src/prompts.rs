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
- title: a clean human title for the document (its own title if it has one — \
never a filename).\n\
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
Reply ONLY as JSON: {\"title\": \"...\", \"summary\": \"...\", \"key_claims\": [{\"text\": \"...\", \
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

pub const SUGGEST_LINKS_SYSTEM: &str = "\
You weave freshly extracted claims into an existing wiki. You are given the \
claims, a list of existing candidate pages, and the slug of the new source \
page. Return:\n\
- linked_claims: the same claims, in the same order, VERBATIM apart from \
inserted [[wikilink]] brackets. Link mentions of existing pages: use [[id]] \
when the id reads naturally in place, else [[id|existing words]] wrapping the \
words already present. Never reword, reorder, add or drop anything else. Link \
only to the provided candidate ids or to new_pages slugs you propose.\n\
- new_pages: entities or concepts central to the claims that match NO \
candidate page: {\"slug\": \"kebab-case\", \"dir\": \"entities|concepts\", \
\"title\": \"...\", \"kind\": \"entity|concept\", \"blurb\": \"one sentence\"}. \
Only propose a page you would genuinely expect this wiki to keep. \n\
- cross_references: candidate ids genuinely related to this source (they go \
in its Cross-references section).\n\
- update_targets: the subset of candidate ids (never kind source) whose pages \
should gain a new section from these claims.\n\
Reply ONLY as JSON: {\"linked_claims\": [\"...\"], \"new_pages\": [...], \
\"cross_references\": [\"id\"], \"update_targets\": [\"id\"]}";

pub const PAGE_UPDATE_SYSTEM: &str = "\
You update ONE wiki page with material from a just-ingested source. You are \
given the page and the relevant claims (already wikilinked). Write the body \
of a short new section — a heading is added for you, so emit no `#` heading \
lines — weaving in only what is genuinely NEW for this page. Keep the claims' \
wikilinks, and cite the source page at least once as a [[wikilink]] using the \
provided source slug. Draw only on the provided claims — never outside \
knowledge. If nothing here is genuinely new for this page, reply with \
relevant=false. Reply ONLY as JSON: {\"topic\": \"3-6 word section topic\", \
\"section_md\": \"...\", \"relevant\": true}";

pub const CONTRADICTION_SYSTEM: &str = "\
You check ONE wiki page against newly extracted claims for genuine factual \
contradictions — direct conflicts, not additions, refinements, or different \
emphasis. For each conflict: existing_text MUST be an exact verbatim quote \
from the page, and new_claim MUST be one of the provided claims, verbatim. \
Be conservative: an empty findings list is the common, correct answer. Reply \
ONLY as JSON: {\"findings\": [{\"page_id\": \"...\", \"existing_text\": \
\"...\", \"new_claim\": \"...\", \"explanation\": \"...\"}]}";

/// Weave step 1: wikilink the claims against candidate pages.
pub fn suggest_links_messages(
    claims: &[String],
    candidates: &[String],
    source_slug: &str,
) -> Vec<Message> {
    let mut user = String::from("Claims:\n");
    for (i, c) in claims.iter().enumerate() {
        user.push_str(&format!("{}. {c}\n", i + 1));
    }
    user.push_str("\nCandidate pages (id — title (kind)):\n");
    for c in candidates {
        user.push_str(&format!("- {c}\n"));
    }
    user.push_str(&format!("\nNew source page slug: {source_slug}\n"));
    vec![Message::system(SUGGEST_LINKS_SYSTEM), Message::user(user)]
}

/// Weave step 2: draft an appended section for one existing page.
pub fn page_update_messages(
    page_id: &str,
    page_title: &str,
    page_kind: &str,
    page_body: &str,
    claims: &[String],
    source_slug: &str,
) -> Vec<Message> {
    let mut user = format!("Page {page_id} — {page_title} ({page_kind}):\n{page_body}\n");
    user.push_str("\nRelevant claims from the new source:\n");
    for c in claims {
        user.push_str(&format!("- {c}\n"));
    }
    user.push_str(&format!("\nSource page to cite: [[{source_slug}]]\n"));
    vec![Message::system(PAGE_UPDATE_SYSTEM), Message::user(user)]
}

/// Weave step 3: contradiction check for one page.
pub fn contradiction_messages(
    page_id: &str,
    page_title: &str,
    page_body: &str,
    claims: &[String],
) -> Vec<Message> {
    let mut user = format!("Page {page_id} — {page_title}:\n{page_body}\n");
    user.push_str("\nNew claims:\n");
    for c in claims {
        user.push_str(&format!("- {c}\n"));
    }
    vec![Message::system(CONTRADICTION_SYSTEM), Message::user(user)]
}

pub fn weave_params() -> CompletionParams {
    CompletionParams { temperature: 0.0, max_tokens: 1024, json: true }
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
