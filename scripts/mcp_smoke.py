#!/usr/bin/env python3
"""Smoke test: drive `cogs mcp` over stdio (MCP JSON-RPC, newline-delimited)."""
import json
import subprocess
import sys

binary, vault = sys.argv[1], sys.argv[2]
extra = sys.argv[3:]

proc = subprocess.Popen(
    [binary, "--vault", vault, *extra, "mcp"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
)

_id = 0
def rpc(method, params=None, notify=False):
    global _id
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not notify:
        _id += 1
        msg["id"] = _id
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()
    if notify:
        return None
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("server closed")
        resp = json.loads(line)
        if resp.get("id") == _id:
            if "error" in resp:
                raise RuntimeError(f"{method}: {resp['error']}")
            return resp["result"]

def call_tool(name, args):
    r = rpc("tools/call", {"name": name, "arguments": args})
    assert not r.get("isError"), f"{name} errored: {r}"
    return r

init = rpc("initialize", {"protocolVersion": "2025-06-18",
    "capabilities": {}, "clientInfo": {"name": "smoke", "version": "0"}})
assert "tools" in init["capabilities"], init
print(f"initialize: ok (instructions: {init.get('instructions', '')[:60]}...)")
rpc("notifications/initialized", {}, notify=True)

tools = rpc("tools/list")["tools"]
names = sorted(t["name"] for t in tools)
print(f"tools/list: {names}")
assert {"search", "get_note", "neighbours", "lineage", "list_notes", "health_report"} <= set(names)

r = call_tool("search", {"query": "agentic unit", "k": 3})
hits = json.loads(r["content"][0]["text"])["hits"] if r.get("content") else r.get("structuredContent", {}).get("hits")
print(f"search: ok ({len(hits)} hits, top: {hits[0]['id']})")

r = call_tool("get_note", {"id": hits[0]["id"]})
note = r.get("structuredContent") or json.loads(r["content"][0]["text"])
assert note.get("markdown"), "no body"
print(f"get_note: ok ({note['id']}, {len(note['markdown'])} chars)")

r = call_tool("neighbours", {"id": note["id"], "limit": 5})
nb = r.get("structuredContent") or json.loads(r["content"][0]["text"])
print(f"neighbours: ok (out={len(nb['out'])} in={len(nb['in'])})")

r = call_tool("lineage", {"id": note["id"], "max_depth": 2})
lin = r.get("structuredContent") or json.loads(r["content"][0]["text"])
print(f"lineage: ok ({len(lin['reachable'])} reachable)")

r = call_tool("list_notes", {"kind": "concept", "limit": 5})
ln = r.get("structuredContent") or json.loads(r["content"][0]["text"])
print(f"list_notes: ok ({len(ln['notes'])} concepts)")

r = call_tool("health_report", {})
h = r.get("structuredContent") or json.loads(r["content"][0]["text"])
print(f"health_report: ok (orphans={len(h['orphans'])} stale={len(h['stale'])} notes={h['counts']})")

proc.terminate()
print("ALL MCP SMOKE TESTS PASSED")
