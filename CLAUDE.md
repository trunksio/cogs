# cogs — agent notes

Graph wiki engine (Rust workspace + Svelte web app + Zed wasm extension).
Plan: see README.md roadmap; reference implementation being replaced lives at
/Users/lewis/AOA/aoa-knowledge (its scripts/sync_graph.py and
scripts/mcp_server.py define parity semantics — resolver rules, FTS body
stripping, index lifecycle quirks).

## Build & test

- Rust is installed via rustup (homebrew rustup, NOT homebrew rust):
  `export PATH="/opt/homebrew/opt/rustup/bin:$PATH"` if cargo is missing.
- `cargo build` from the workspace ROOT (building from a crate dir leaves the
  root binary stale — this has bitten before).
- `cargo test --workspace`. Web: `cd web && npm run build`. Extension:
  `cd zed-extension && cargo check --target wasm32-wasip2` (excluded from the
  workspace).
- Full binary with embedded viz: `just build`. Debug builds serve `web/dist`
  from disk (rust-embed), so no rebuild needed after `npm run build`.

## Hard-won constraints (do not rediscover)

- Any binary embedding `lbug` MUST emit `cargo:rustc-link-arg=-rdynamic`
  (see build.rs) or `INSTALL FTS` / `LOAD EXTENSION` fail with "function not
  defined". Test binaries get it from lbug's own build script; ours don't.
- Ladybug FTS indexes don't see row updates → drop + recreate after sync
  (schema::refresh_fts). Vector indexes lock their column against SET →
  drop index, write embeddings, recreate (see sync_graph.py:246-297).
- Ladybug is single-writer multi-reader: GraphDb::open_rw first-come (flock in
  cogs-runtime), everyone else open_ro. lbug `Connection` borrows `Database`
  (lifetime) — create connections per scope, never store them in structs.
- Frontmatter YAML must be parsed into `serde_json::Value` (NOT
  `serde_yaml_ng::Value`) — real vault files contain duplicate keys; PyYAML
  semantics are last-wins and serde_yaml_ng's own Value type rejects them.
- Note ids: `wiki/concepts/x.md` → `concepts-x` (strip configured prefix,
  '/'→'-'). Link resolution: lowercase target; path-form → id; bare slug
  unique-or-same-dir-tiebreak. Ported exactly from sync_graph.py — parity
  test against the live aoa vault: see "Parity check" below.
- Kuzu/Ladybug Cypher: ORDER BY must use RETURN aliases when aggregating
  (`ORDER BY id`, not `ORDER BY p.id`).
- rmcp tools can't return `Json<serde_json::Value>` (outputSchema must be a
  typed object) — return JSON as `String` instead.
- `reqwest::blocking` (embedding + LLM providers) deadlocks when created/driven
  on a tokio worker thread — rmcp tool handlers run inside tokio, so the MCP
  server hops embedding calls (embed_query) and the whole `ask` pipeline to a
  plain OS thread (std::thread::scope). Same rule for any future blocking call
  from an MCP tool or axum handler.
- LLM backend is pluggable via [llm] (cogs-llm crate): omlx/ollama/openai are
  OpenAI-compatible (base_url + /chat/completions), anthropic is its own
  adapter. omlx requires a bearer key — set api_key_env in [llm] and export it.
  [llm] and [server] are excluded from the config hash so changing them never
  triggers a graph rebuild.
- ChatProvider must stay object-safe (used as dyn): the JSON helper is a free
  fn `complete_json(&dyn ChatProvider, ...)`, not a trait method.
- `time` is pinned to 0.3.41 in Cargo.lock: newer `time` breaks wry's `cookie`
  dependency (conflicting From impls). Don't blindly `cargo update -p time`.
- Release builds for macOS arm64 need macos-15+ runners — macos-14's older ld
  hard-errors on duplicate symbols from lbug's force-loaded static archives.
- Linux release builds use --no-default-features (no wry viz window): wry
  would dynamically link WebKitGTK and break the binary on headless machines.
- Release asset names (cogs-<tag>-<target>.tar.gz) are load-bearing: the Zed
  extension's downloader constructs them in zed-extension/src/lib.rs.
- `cogs viz` (wry/tao window) must run on the main thread; show/hide control
  is a UDS at `<vault>/.cogs/runtime/viz.sock` (verbs: show/toggle/quit).
  Closing the window hides it; the instance keeps webview state.

## Parity check (M1 exit criterion, keep green)

```sh
./target/debug/cogs --vault /Users/lewis/AOA/aoa-knowledge \
  --config examples/aoa-knowledge.cogs.toml --state-dir /tmp/cogs-aoa-state \
  sync --full
```

Counts must match `scripts/sync_graph.py --full --db /tmp/ref-graph.db` run
from the aoa repo (510 notes / 244 resources / 761 tags / CITES 4282 /
SOURCE_OF 932 / TAGGED 2332 as of 2026-06-12). NEVER write into the aoa repo:
always pass `--state-dir`, and if you run their sync_graph.py, back up and
restore `.wiki/last_sync_sha`.

## Smoke tests

```sh
python3 scripts/lsp_smoke.py ./target/debug/cogs /tmp/cogs-lsp-vault agentic-unit.md
python3 scripts/mcp_smoke.py ./target/debug/cogs <vault> [--config ...] [--state-dir ...]
```
