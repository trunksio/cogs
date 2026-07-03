//! Distill tests: vault mining, deterministic split, and --from-runs
//! acceptance pairing (survivor content becomes the label).

mod common;

use std::path::Path;

use common::*;
use cogs_ingest::{distill, DistillOptions, Ingester};

fn read_pairs(dir: &Path) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for f in ["train.jsonl", "valid.jsonl"] {
        let text = std::fs::read_to_string(dir.join(f)).unwrap_or_default();
        out.extend(text.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()));
    }
    out
}

/// The assistant turn parsed back into JSON, plus the user turn's content.
fn split_pair(pair: &serde_json::Value) -> (String, serde_json::Value) {
    let msgs = pair["messages"].as_array().unwrap();
    let user = msgs[1]["content"].as_str().unwrap().to_string();
    let target: serde_json::Value =
        serde_json::from_str(msgs.last().unwrap()["content"].as_str().unwrap()).unwrap();
    (user, target)
}

#[test]
fn mining_produces_extract_and_links_pairs() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());

    let out = tmp.path().join("dataset");
    let stats = distill(
        &vault,
        &db,
        None,
        &DistillOptions { out: Some(out.clone()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(stats.emitted.get("extract"), Some(&1));
    assert_eq!(stats.emitted.get("suggest_links"), Some(&1));

    let pairs = read_pairs(&out);
    assert_eq!(pairs.len(), 2);

    // extract pair: raw body in, reconstructed accepted Extraction out.
    let extract = pairs
        .iter()
        .find(|p| p["messages"][0]["content"].as_str().unwrap().contains("ingest engine"))
        .unwrap();
    let (user, target) = split_pair(extract);
    assert!(user.contains("title: Old article"));
    assert!(user.contains("url: https://example.com/old"));
    assert!(user.contains("Old body text about agent registries."));
    assert_eq!(target["summary"], "Old news about agent registries and discovery.");
    assert_eq!(
        target["key_claims"][0]["text"],
        "Registries let agent registries be discovered centrally."
    );
    assert_eq!(target["key_claims"][0]["entities"][0], "agent registries");
    assert_eq!(target["key_claims"][1]["text"], "Discovery predates verification.");
    assert_eq!(target["quotes"][0]["text"], "registries were inevitable");
    assert_eq!(target["quotes"][0]["location"], "intro");
    assert_eq!(target["suggested_slug"], "old-article");
    assert_eq!(target["author"], "Jane Doe");
    assert_eq!(target["tags"][0], "registry");

    // suggest_links pair: plain claims + candidates in, accepted links out.
    let links = pairs
        .iter()
        .find(|p| {
            p["messages"][0]["content"].as_str().unwrap().contains("weave freshly extracted")
        })
        .unwrap();
    let (user, target) = split_pair(links);
    // plain (link-stripped) claims as input
    assert!(user.contains("1. Registries let agent registries be discovered centrally."));
    // gold link target force-unioned into the candidate list
    assert!(user.contains("concepts/agent-registry — Agent Registry (concept)"), "user:\n{user}");
    assert!(user.contains("New source page slug: old-article"));
    // accepted wikilinked claims as the label
    assert_eq!(
        target["linked_claims"][0],
        "Registries let [[concepts/agent-registry|agent registries]] be discovered centrally."
    );
    assert_eq!(target["cross_references"][0], "concepts/agent-registry");
}

#[test]
fn split_and_output_are_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());

    let out1 = tmp.path().join("d1");
    let out2 = tmp.path().join("d2");
    for out in [&out1, &out2] {
        distill(
            &vault,
            &db,
            None,
            &DistillOptions { out: Some(out.clone()), ..Default::default() },
        )
        .unwrap();
    }
    for f in ["train.jsonl", "valid.jsonl"] {
        assert_eq!(
            std::fs::read_to_string(out1.join(f)).unwrap(),
            std::fs::read_to_string(out2.join(f)).unwrap(),
            "{f} differs between runs"
        );
    }
}

#[test]
fn from_runs_pairs_track_surviving_content() {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, db) = setup(tmp.path());
    let raw = add_capture(tmp.path());

    // A weave that updates the agent-registry concept page.
    let links_reply = serde_json::json!({
        "linked_claims": [CLAIM_1, CLAIM_2],
        "new_pages": [],
        "cross_references": ["concepts/agent-registry"],
        "update_targets": ["concepts/agent-registry"]
    })
    .to_string();
    let update_reply = serde_json::json!({
        "topic": "Verified listings",
        "section_md": "Registries now verify publishers ([[mcp-registry-announcement]]).",
        "relevant": true
    })
    .to_string();
    let chat =
        ScriptedChat::routed(&[extraction_reply()], &[links_reply], &[&update_reply], &[]);
    let report = Ingester::new(&vault, &db, &chat, None, opts()).ingest(Path::new(raw)).unwrap();
    assert_eq!(report.pages_updated, vec!["concepts-agent-registry"]);

    // Human review edits both the source page summary and the appended section.
    let src_path = tmp.path().join("wiki/sources/mcp-registry-announcement.md");
    let edited = std::fs::read_to_string(&src_path).unwrap().replace(
        "Anthropic launched a registry for MCP servers that indexes community servers and verifies publishers.",
        "Anthropic launched an MCP server registry with verified publishers.",
    );
    std::fs::write(&src_path, edited).unwrap();
    let concept_path = tmp.path().join("wiki/concepts/agent-registry.md");
    let edited = std::fs::read_to_string(&concept_path).unwrap().replace(
        "Registries now verify publishers",
        "Publisher verification is now a registry gate",
    );
    std::fs::write(&concept_path, edited).unwrap();

    let out = tmp.path().join("dataset");
    let stats = distill(
        &vault,
        &db,
        None,
        &DistillOptions { out: Some(out.clone()), from_runs: true, ..Default::default() },
    )
    .unwrap();
    // extract: mined old-article + run-backed new page (dedup keeps 2, not 3)
    assert_eq!(stats.emitted.get("extract"), Some(&2));
    assert_eq!(stats.emitted.get("suggest_links"), Some(&2));
    assert_eq!(stats.emitted.get("page_update"), Some(&1));

    let pairs = read_pairs(&out);

    // The run-backed extract pair carries the HUMAN-EDITED summary as label.
    let edited_extract = pairs
        .iter()
        .map(split_pair)
        .find(|(_, t)| t["suggested_slug"] == "mcp-registry-announcement")
        .expect("run-backed extract pair");
    assert_eq!(
        edited_extract.1["summary"],
        "Anthropic launched an MCP server registry with verified publishers."
    );

    // The page_update pair's label is the surviving (edited) section body.
    let upd = pairs
        .iter()
        .map(split_pair)
        .find(|(_, t)| t.get("section_md").is_some())
        .expect("page_update pair");
    assert_eq!(
        upd.1["section_md"],
        "Publisher verification is now a registry gate ([[mcp-registry-announcement]])."
    );
    assert_eq!(upd.1["topic"], "Verified listings");
    assert_eq!(upd.1["relevant"], true);
    // and its input is the original recorded prompt
    assert!(upd.0.contains("Page concepts-agent-registry"));

    // The run-backed links pair reconstructs update_targets from surviving
    // sections.
    let links = pairs
        .iter()
        .map(split_pair)
        .find(|(u, t)| {
            t.get("linked_claims").is_some() && u.contains("New source page slug: mcp-registry-announcement")
        })
        .expect("run-backed links pair");
    assert_eq!(links.1["update_targets"][0], "concepts/agent-registry");

    // Rejecting the section (deleting it) drops the page_update pair.
    let text = std::fs::read_to_string(&concept_path).unwrap();
    let cut = text.split("\n## Verified listings").next().unwrap().to_string();
    std::fs::write(&concept_path, cut).unwrap();
    let out2 = tmp.path().join("dataset2");
    let stats2 = distill(
        &vault,
        &db,
        None,
        &DistillOptions {
            out: Some(out2.clone()),
            from_runs: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stats2.emitted.get("page_update"), None);
    assert!(stats2
        .skipped
        .keys()
        .any(|k| k.contains("appended section removed")));
}
