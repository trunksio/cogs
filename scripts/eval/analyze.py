#!/usr/bin/env python3
"""Compare student vs teacher eval vaults: success, speed, JSON reliability,
validator-warning profile, and page content stats."""
import glob
import json
import os
import re
import sys

WARN_BUCKETS = [
    ("non-verbatim quote", "quote dropped (fabricated/paraphrased)"),
    ("unwrapped unresolvable link", "hallucinated link unwrapped"),
    ("rewrote claim", "claim rewrite reverted"),
    ("junk claim", "junk claim dropped"),
    ("never cites", "page update rejected (no citation)"),
    ("synthesized one from the top claims", "summary synthesized (fallback)"),
    ("link weaving skipped", "whole link stage degraded"),
    ("claims; keeping the first", "claim overflow truncated"),
    ("already exists", "duplicate new-page proposal"),
    ("malformed slug", "malformed slug"),
]


def analyze(vault: str, runlog_marker: str, runlog: str) -> dict:
    out = {"ok": 0, "fail": 0, "times": [], "warnings": {}, "reports": 0,
           "pages_created": 0, "pages_updated": 0, "contradictions": 0,
           "calls": 0, "parse_fails": 0, "content_retries": 0,
           "claims": [], "quotes": [], "links_in_claims": 0}
    # timing + ok/fail from the run log section
    section = []
    grab = False
    for line in open(runlog):
        if line.startswith("=== TEACHER ==="):
            grab = not grab
            continue
        section.append((grab, line))
    want = runlog_marker == "teacher"
    for grabbed, line in section:
        if grabbed != want or not line.startswith("["):
            continue
        m = re.search(r"\] (OK|FAIL) .* (\d+)s$", line.strip())
        if m:
            out["ok" if m.group(1) == "OK" else "fail"] += 1
            if m.group(1) == "OK":
                out["times"].append(int(m.group(2)))
    # reports
    for rp in glob.glob(f"{vault}/.cogs/eval-reports/*.json"):
        try:
            r = json.load(open(rp))
        except Exception:
            continue
        out["reports"] += 1
        out["pages_created"] += len(r.get("pages_created", []))
        out["pages_updated"] += len(r.get("pages_updated", []))
        out["contradictions"] += len(r.get("contradictions", []))
        for w in r.get("warnings", []):
            for needle, label in WARN_BUCKETS:
                if needle in w:
                    out["warnings"][label] = out["warnings"].get(label, 0) + 1
                    break
            else:
                out["warnings"]["other"] = out["warnings"].get("other", 0) + 1
    # training records → parse reliability
    for jf in glob.glob(f"{vault}/.cogs/training/runs/*.jsonl"):
        for line in open(jf):
            rec = json.loads(line)
            out["calls"] += 1
            if not rec["parsed_ok"]:
                out["parse_fails"] += 1
            if rec.get("meta", {}).get("content_retry"):
                out["content_retries"] += 1
    # page content stats
    for sp in glob.glob(f"{vault}/wiki/sources/*.md"):
        body = open(sp).read()
        m = re.search(r"## Key claims\n(.*?)(\n## |\Z)", body, re.S)
        claims = [l for l in (m.group(1).split("\n") if m else []) if l.startswith("- ")]
        out["claims"].append(len(claims))
        out["links_in_claims"] += sum(c.count("[[") for c in claims)
        m = re.search(r"## Quotes\n(.*?)(\n## |\Z)", body, re.S)
        out["quotes"].append(len([l for l in (m.group(1).split("\n") if m else []) if l.startswith("> ")]))
    return out


def fmt(label, s, t):
    print(f"{label:<44} {s!s:>14} {t!s:>14}")


base = sys.argv[1] if len(sys.argv) > 1 else "."
runlog = sys.argv[2]
S = analyze(f"{base}/eval-student", "student", runlog)
T = analyze(f"{base}/eval-teacher", "teacher", runlog)

avg = lambda xs: round(sum(xs) / len(xs), 1) if xs else 0
fmt("metric", "STUDENT 1.7B", "TEACHER 35B")
fmt("-" * 40, "-" * 12, "-" * 12)
fmt("files ok / fail", f"{S['ok']} / {S['fail']}", f"{T['ok']} / {T['fail']}")
fmt("avg seconds per file", avg(S["times"]), avg(T["times"]))
fmt("LLM calls (recorded)", S["calls"], T["calls"])
pf = lambda o: f"{o['parse_fails']} ({100*o['parse_fails']/max(1,o['calls']):.0f}%)"
fmt("unparseable replies", pf(S), pf(T))
fmt("content retries (no-summary)", S["content_retries"], T["content_retries"])
fmt("pages created / updated", f"{S['pages_created']} / {S['pages_updated']}", f"{T['pages_created']} / {T['pages_updated']}")
fmt("contradictions confirmed", S["contradictions"], T["contradictions"])
fmt("avg claims per source page", avg(S["claims"]), avg(T["claims"]))
fmt("avg verbatim quotes per page", avg(S["quotes"]), avg(T["quotes"]))
fmt("wikilinks inside claims (total)", S["links_in_claims"], T["links_in_claims"])
print("\nvalidator warnings:")
for label in sorted(set(S["warnings"]) | set(T["warnings"])):
    fmt("  " + label, S["warnings"].get(label, 0), T["warnings"].get(label, 0))
