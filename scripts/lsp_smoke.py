#!/usr/bin/env python3
"""Smoke test: drive `cogs lsp` over stdio with a real vault.

Usage: lsp_smoke.py <cogs-binary> <vault-root> <a-note-rel-path>
"""
import json
import subprocess
import sys
import threading

binary, vault, note_rel = sys.argv[1], sys.argv[2], sys.argv[3]

proc = subprocess.Popen(
    [binary, "--vault", vault, "lsp"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
)

_id = 0
def send(method, params, notify=False):
    global _id
    msg = {"jsonrpc": "2.0", "method": method, "params": params}
    if not notify:
        _id += 1
        msg["id"] = _id
    raw = json.dumps(msg).encode()
    proc.stdin.write(f"Content-Length: {len(raw)}\r\n\r\n".encode() + raw)
    proc.stdin.flush()
    return _id

def read_msg():
    headers = {}
    while True:
        line = proc.stdout.readline().decode()
        if line in ("\r\n", "\n"):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers["content-length"])
    return json.loads(proc.stdout.read(n))

def wait_response(want_id, timeout=60):
    while True:
        msg = read_msg()
        if msg.get("id") == want_id and ("result" in msg or "error" in msg):
            return msg

note_text = open(f"{vault}/{note_rel}").read()
uri = f"file://{vault}/{note_rel}"

rid = send("initialize", {"processId": None, "rootUri": f"file://{vault}",
            "workspaceFolders": [{"uri": f"file://{vault}", "name": "vault"}],
            "capabilities": {}})
resp = wait_response(rid)
caps = resp["result"]["capabilities"]
assert caps["definitionProvider"] and caps["referencesProvider"], caps
print("initialize: ok")

send("initialized", {}, notify=True)
send("textDocument/didOpen", {"textDocument": {
    "uri": uri, "languageId": "markdown", "version": 1, "text": note_text}}, notify=True)

# Find a wikilink in the note to aim at.
import re
m = re.search(r"\[\[([^\]|#\n]+)", note_text)
assert m, "test note has no wikilink"
upto = note_text[: m.start() + 2]
line = upto.count("\n")
col = len(upto) - (upto.rfind("\n") + 1) + 1
pos = {"line": line, "character": col}
print(f"aiming at [[{m.group(1)}]] line={line} col={col}")

rid = send("textDocument/definition", {"textDocument": {"uri": uri}, "position": pos})
resp = wait_response(rid)
assert resp.get("result"), f"definition failed: {resp}"
print(f"definition: ok -> {resp['result']['uri'].split('/')[-1]}")

rid = send("textDocument/hover", {"textDocument": {"uri": uri}, "position": pos})
resp = wait_response(rid)
hover = resp.get("result") or {}
val = hover.get("contents", {}).get("value", "")
assert "backlinks" in val, f"hover missing backlinks: {val[:100]}"
print(f"hover: ok ({val.splitlines()[0][:60]}...)")

rid = send("textDocument/references", {"textDocument": {"uri": uri}, "position": pos,
            "context": {"includeDeclaration": False}})
resp = wait_response(rid)
refs = resp.get("result") or []
print(f"references: ok ({len(refs)} backlinks)")

# Completion inside a fresh [[ at end of file
new_text = note_text + "\n[[agent"
send("textDocument/didChange", {"textDocument": {"uri": uri, "version": 2},
     "contentChanges": [{"text": new_text}]}, notify=True)
lines = new_text.split("\n")
rid = send("textDocument/completion", {"textDocument": {"uri": uri},
    "position": {"line": len(lines) - 1, "character": len(lines[-1])}})
resp = wait_response(rid)
result = resp.get("result")
items = result if isinstance(result, list) else (result or {}).get("items", [])
assert len(items) > 0, "no completions"
print(f"completion: ok ({len(items)} items, first: {items[0]['label']})")

rid = send("workspace/symbol", {"query": "agentic"})
resp = wait_response(rid)
syms = resp.get("result") or []
print(f"workspace/symbol: ok ({len(syms)} hits)")

rid = send("shutdown", {})
wait_response(rid)
send("exit", {}, notify=True)
proc.wait(timeout=10)
print("ALL LSP SMOKE TESTS PASSED")
