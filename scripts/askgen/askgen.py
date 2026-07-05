#!/usr/bin/env python3
"""Build the ask-adapter SFT dataset: teacher-generated questions over a
vault, answered through the REAL `cogs ask` pipeline with training capture,
filtered by citation acceptance.

Phases (resumable; state lives in --out):
  questions  — sample wiki pages, have the teacher write grounded questions
               (plus a slice of deliberately unanswerable probes, so the
               student learns to abstain)
  answer     — `cogs ask --json --capture-training` per question
  dataset    — recorded decompose/synthesize calls -> train/valid chat JSONL;
               synthesize pairs kept only if citations survived validation
               (or the question was a probe and the model abstained)

Teacher API is any OpenAI-compatible endpoint (built for Ollama Cloud):
  ASKGEN_BASE_URL (default https://ollama.com/v1)
  ASKGEN_MODEL    (default deepseek-v4-flash)
  ASKGEN_API_KEY_ENV names the env var holding the key (default OLLAMA_API_KEY)

Usage:
  askgen.py questions --vault <vault> --out <dir> --count 100
  askgen.py answer    --vault <vault> --config <cogs.toml> --out <dir> [--cogs <bin>]
  askgen.py dataset   --vault <vault> --out <dir> [--split 0.1]
"""
import argparse
import glob
import hashlib
import json
import os
import random
import re
import subprocess
import sys
import urllib.request

BASE_URL = os.environ.get("ASKGEN_BASE_URL", "https://ollama.com/v1")
MODEL = os.environ.get("ASKGEN_MODEL", "deepseek-v4-flash")
KEY_ENV = os.environ.get("ASKGEN_API_KEY_ENV", "OLLAMA_API_KEY")

QUESTION_SYS = (
    "You write evaluation questions for a private wiki. Given excerpts from "
    "wiki pages, produce questions a curious reader would ask that ARE "
    "answerable from the excerpts — mix factual lookups, comparisons, and "
    "how/why questions; vary phrasing and length; never reference 'the wiki', "
    "'the excerpt', or page names as such. Reply ONLY as JSON: "
    '{"questions": ["..."]}'
)
PROBE_SYS = (
    "You write plausible-sounding questions that a knowledge base about the "
    "given topics could NOT answer: each must hinge on a specific fact from "
    "OUTSIDE the topic area — a different industry, a product/event/person "
    "the topics would never cover, or precise figures (prices, dates, "
    "versions) that no document about these topics would contain. Do not ask "
    "about the topics themselves. Reply ONLY as JSON: "
    '{"questions": ["..."]}'
)


def chat(system: str, user: str, max_tokens: int = 1200) -> str:
    key = os.environ.get(KEY_ENV, "")
    req = urllib.request.Request(
        f"{BASE_URL}/chat/completions",
        data=json.dumps({
            "model": MODEL,
            "temperature": 0.8,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        }).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {key}"},
    )
    with urllib.request.urlopen(req, timeout=180) as r:
        return json.load(r)["choices"][0]["message"]["content"] or ""


def json_block(s: str):
    m = re.search(r"\{.*\}", s, re.S)
    return json.loads(m.group(0)) if m else {}


def sample_pages(vault: str, n_batches: int, per_batch: int = 3, seed: int = 7):
    pages = sorted(glob.glob(f"{vault}/wiki/*/*.md"))
    pages = [p for p in pages if "/_lint/" not in p]
    rng = random.Random(seed)
    rng.shuffle(pages)
    batches, i = [], 0
    while len(batches) < n_batches and i + per_batch <= len(pages):
        batches.append(pages[i:i + per_batch])
        i += per_batch
    return batches


def cmd_questions(a):
    os.makedirs(a.out, exist_ok=True)
    path = f"{a.out}/questions.json"
    qs = json.load(open(path)) if os.path.exists(path) else []
    have = {q["question"] for q in qs}
    n_probe = max(1, a.count // 10)
    n_real = a.count - n_probe

    per_call = 4
    for batch in sample_pages(a.vault, (n_real + per_call - 1) // per_call):
        if sum(1 for q in qs if not q["probe"]) >= n_real:
            break
        excerpts = []
        for p in batch:
            body = open(p, errors="replace").read()
            body = re.sub(r"^---.*?---", "", body, flags=re.S)
            excerpts.append(body.strip()[:2500])
        try:
            out = json_block(chat(QUESTION_SYS, "\n\n----\n\n".join(excerpts)))
        except Exception as e:
            print(f"question call failed: {e}", file=sys.stderr)
            continue
        for q in (out.get("questions") or [])[:per_call]:
            if isinstance(q, str) and q.strip() and q not in have:
                qs.append({"question": q.strip(), "probe": False, "answered": False})
                have.add(q)
        json.dump(qs, open(path, "w"), indent=1)
        print(f"questions: {len(qs)}")

    topics = ", ".join(os.path.basename(p)[:-3] for b in sample_pages(a.vault, 4) for p in b)
    while sum(1 for q in qs if q["probe"]) < n_probe:
        try:
            out = json_block(chat(PROBE_SYS, f"Topics: {topics}"))
        except Exception as e:
            print(f"probe call failed: {e}", file=sys.stderr)
            break
        for q in (out.get("questions") or [])[:n_probe]:
            if isinstance(q, str) and q.strip() and q not in have:
                qs.append({"question": q.strip(), "probe": True, "answered": False})
                have.add(q)
        json.dump(qs, open(path, "w"), indent=1)
    print(f"total questions: {len(qs)} ({sum(1 for q in qs if q['probe'])} probes)")


def cmd_answer(a):
    path = f"{a.out}/questions.json"
    qs = json.load(open(path))
    cogs = a.cogs or os.path.join(os.path.dirname(__file__), "../../target/debug/cogs")
    tdir = f"{a.out}/capture"
    done = fail = 0
    for q in qs:
        if q.get("answered"):
            continue
        r = subprocess.run(
            [cogs, "--vault", a.vault, "--config", a.config, "ask", q["question"],
             "--json", "--capture-training", "--training-dir", tdir],
            capture_output=True, text=True, timeout=1200,
        )
        if r.returncode == 0:
            try:
                ans = json.loads(r.stdout)
                q["answered"] = True
                q["abstained"] = ans.get("abstained", False)
                q["citations"] = len(ans.get("citations", []))
                done += 1
            except Exception:
                fail += 1
        else:
            fail += 1
            print(f"ask failed: {q['question'][:60]} :: {r.stderr[-160:]}", file=sys.stderr)
        json.dump(qs, open(path, "w"), indent=1)
        print(f"[{done} ok / {fail} fail] {q['question'][:70]}")
    print(f"answer phase done: {done} ok, {fail} fail")


def is_valid_split(key: str, frac: float) -> bool:
    x = int.from_bytes(hashlib.sha256(key.encode()).digest()[:8], "big")
    return x / 2**64 < frac


def cmd_dataset(a):
    qmeta = {q["question"]: q for q in json.load(open(f"{a.out}/questions.json"))}
    runs = sorted(glob.glob(f"{a.out}/capture/runs/*.jsonl"))
    os.makedirs(f"{a.out}/dataset", exist_ok=True)
    train = open(f"{a.out}/dataset/train.jsonl", "w")
    valid = open(f"{a.out}/dataset/valid.jsonl", "w")
    kept = dropped = 0
    for rf in runs:
        # acceptance record written by `cogs ask --capture-training`
        ask_json = rf.replace(".jsonl", ".ask.json")
        answer = json.load(open(ask_json)) if os.path.exists(ask_json) else None
        for line in open(rf):
            rec = json.loads(line)
            if not rec["parsed_ok"]:
                continue
            q = rec.get("meta", {}).get("question", "")
            meta = qmeta.get(q, {})
            if rec["task"] == "synthesize":
                if answer is None:
                    dropped += 1
                    continue
                ok_answer = not answer["abstained"] and len(answer["citations"]) > 0
                ok_abstain = answer["abstained"] and meta.get("probe", False)
                if not (ok_answer or ok_abstain):
                    dropped += 1
                    continue
            msgs = rec["messages"] + [{"role": "assistant", "content": rec["raw_output"]}]
            line_out = json.dumps({"messages": msgs})
            (valid if is_valid_split(q or rf, a.split) else train).write(line_out + "\n")
            kept += 1
    train.close(); valid.close()
    print(f"dataset: {kept} pairs kept, {dropped} dropped (rejected answers)")


p = argparse.ArgumentParser()
sub = p.add_subparsers(dest="cmd", required=True)
q = sub.add_parser("questions"); q.add_argument("--vault", required=True); q.add_argument("--out", required=True); q.add_argument("--count", type=int, default=100)
ans = sub.add_parser("answer"); ans.add_argument("--vault", required=True); ans.add_argument("--config", required=True); ans.add_argument("--out", required=True); ans.add_argument("--cogs")
d = sub.add_parser("dataset"); d.add_argument("--vault", required=True); d.add_argument("--out", required=True); d.add_argument("--split", type=float, default=0.1)
args = p.parse_args()
{"questions": cmd_questions, "answer": cmd_answer, "dataset": cmd_dataset}[args.cmd](args)
