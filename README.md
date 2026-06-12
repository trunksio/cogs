# cogs

Graph wiki engine for wikilinked markdown vaults — a Karpathy-style knowledge
store brought to life in [Zed](https://zed.dev): a graph-powered language
server, an embedded [LadybugDB](https://ladybugdb.com) property graph with
full-text (and soon vector) search, an MCP server for AI agents, and a
browser-based knowledge-graph visualization that replaces Obsidian's graph
view.

One binary, four roles:

| Command | Role |
|---|---|
| `cogs sync` | Index the vault into `.cogs/graph.db` (incremental, content-hashed) |
| `cogs lsp` | LSP over stdio: `[[link]]` completion, go-to-definition, backlinks, hover previews, broken-link diagnostics, rename-across-vault, symbols |
| `cogs mcp` | Read-only MCP server: `search`, `get_note`, `neighbours`, `lineage`, `list_notes`, `health_report` |
| `cogs serve` | Graph visualization + JSON API at `http://127.0.0.1:7117` |
| `cogs viz` | The same viz in a native window with show/hide toggle (`--toggle`, `--quit`) — bind it to a key in Zed |

Plus `cogs init` (write a starter config), `cogs status`, `cogs query "<cypher>"`.

## Quick tour

With the Zed extension installed and a vault open:

1. Type `[[` in any note — completion over every page, with title and kind.
2. `cmd-click` a wikilink to follow it; hover for a preview with backlink count.
3. "Find All References" on a link or in a note = backlinks.
4. Misspell a link or a frontmatter `kind` — inline diagnostics.
5. `F2` on a link renames the target note and every reference to it.
6. `cogs viz --toggle` (bind it to a key via a Zed task) — the knowledge
   graph in a native window: filter by kind/tag/edge, flip on **semantic**
   to see embedding-similarity edges and unlinked-but-related notes,
   **health** for orphans/contradictions/stale pages, **time** for recency.
7. In Zed's agent panel, ask anything — the `cogs` MCP server gives the
   agent `search`, `semantic_search`, `get_note`, `neighbours`, `lineage`,
   `similar_notes`, and `health_report` over your vault.

## Starting a new vault

```sh
mkdir my-wiki && cd my-wiki
cogs init --karpathy
```

scaffolds the full three-layer system:

```
my-wiki/
├── cogs.toml          # kinds, typed edges, diagnostics, embeddings
├── AGENTS.md          # operating manual for AI agents: ingest/lint/answer
│                      # workflows + the MCP/CLI/LSP tool inventory
├── raw/               # immutable source layer (clips/, research/, files/)
│   └── README.md      # capture conventions
├── wiki/              # LLM-maintained synthesis
│   ├── concepts/ entities/ positions/ questions/ sources/ _lint/
│   ├── index.md       # master catalogue (excluded from the graph)
│   └── log.md         # append-only audit trail
├── .zed/settings.json # cogs LSP + MCP pre-wired
└── .gitignore         # .cogs/ excluded
```

`AGENTS.md` is the contract: any agent (Claude Code, Zed's agent panel, …)
that reads the repo learns the ingest workflow (capture to `raw/` → source
page → weave into concept pages → log), the note frontmatter schema, the
non-negotiables (raw is immutable, claims trace to sources, contradictions
are surfaced not merged), and exactly which cogs tools it has. The MCP
server's self-description is generated from `cogs.toml` too, so agents that
connect over MCP get matching guidance even without reading AGENTS.md.

Already have an Obsidian-style vault? Plain `cogs init` writes just a
commented `cogs.toml` — or skip init entirely and cogs runs with zero-config
defaults (every `*.md` a note, wikilinks → `LINKS_TO`, tags from frontmatter
and inline `#tags`).

## How it works

- **Zero-config**: point it at any Obsidian-style vault — every `*.md` is a
  note, `[[wikilinks]]` become `LINKS_TO` edges, tags come from frontmatter
  and inline `#tags`.
- **Config-driven**: a `cogs.toml` in the vault root declares note kinds,
  typed edges derived from frontmatter fields (e.g. `source_refs` →
  `SOURCE_OF`, `contradicts` → `CONTRADICTS`), an immutable resource layer,
  per-kind required-field diagnostics, and embedding settings. See
  `examples/aoa-knowledge.cogs.toml` for a full setup.
- **One writer, many readers**: the first process to a vault wins a writer
  lock and keeps the graph DB fresh (file watcher + incremental sync); other
  processes open the DB read-only. The LSP's latency-critical features run
  off an in-memory index, so every process is fully functional.
- **The DB is a cache**: wipe `.cogs/` any time; a config change triggers an
  automatic rebuild. Gitignore `.cogs/`.

## Zed setup

1. Build: `just build` (or `cargo build --release` for an empty viz shell).
2. In Zed: `zed: install dev extension` → select `zed-extension/`.
   (Requires rustup with the `wasm32-wasip2` target.)
3. Point the extension at the binary (until releases exist) in Zed settings:

```jsonc
{
  "lsp": {
    "cogs": {
      "binary": { "path": "/path/to/cogs/target/release/cogs", "arguments": ["lsp"] }
    }
  },
  "languages": {
    "Markdown": { "language_servers": ["cogs", "..."] }
  }
}
```

Put `"cogs"` first: Zed routes go-to-definition/references to the first
language server in the list.

MCP for the agent panel (`.zed/settings.json` in the vault):

```jsonc
{
  "context_servers": {
    "cogs": { "command": "/path/to/cogs", "args": ["mcp"] }
  }
}
```

## The graph view

`cogs serve` (or `cogs lsp --serve`, planned) then open
`http://127.0.0.1:7117`:

- WebGL force-directed graph (sigma.js), community-colored, degree-sized,
  positions stable across filtering.
- Filter rail: kind / status / edge-type / tag facets.
- Full-text search highlighting matching nodes.
- **semantic** overlay: embedding-similarity edges (teal) layered onto the
  link graph with a threshold slider and an "unlinked only" toggle —
  conceptually-close-but-unlinked notes are your auto-link candidates. The
  detail panel lists similar notes with scores.
- **health** overlay: orphans flagged red, stale notes dimmed, contradiction
  edges in red.
- **time** lens: cold→warm color ramp by `updated` date.
- Click a note → detail panel with rendered markdown, backlinks/outlinks
  navigation, and **open in Zed**.

Dev loop: `cogs serve` + `cd web && npm run dev` (Vite proxies `/api`).

### Graph window toggled from Zed

Zed extensions can't render custom panels (yet), so `cogs viz` opens the viz
in a native WebKit window instead. The process keeps running while hidden —
camera, filters, and selection survive across toggles. `cogs viz --toggle`
shows/hides a running window (or launches one); closing the window just hides
it; `cogs viz --quit` really exits. Control is per-vault via
`.cogs/runtime/viz.sock`.

Wire it to a Zed keybinding via a task (user-level `tasks.json`):

```jsonc
{
  "label": "cogs: toggle graph",
  "command": "nohup /path/to/cogs viz --toggle >/dev/null 2>&1 &",
  "cwd": "$ZED_WORKTREE_ROOT",
  "reveal": "never",
  "hide": "always"
}
```

and in `keymap.json`:

```jsonc
{
  "context": "Workspace",
  "bindings": { "cmd-alt-g": ["task::Spawn", { "task_name": "cogs: toggle graph" }] }
}
```

## Development

```sh
cargo test --workspace        # unit + integration tests (fixture vaults)
python3 scripts/lsp_smoke.py ./target/debug/cogs <vault> <note.md>
python3 scripts/mcp_smoke.py ./target/debug/cogs <vault>
```

The C++ build of `lbug` (LadybugDB) makes the first compile slow (~5–10 min);
it's cached afterwards. macOS/Linux binaries embedding lbug must link with
`-rdynamic` (see `build.rs`) or the FTS/VECTOR extensions fail to load.

## Embeddings

Enable in `cogs.toml` (`[embeddings] enabled = true`, Ollama default /
OpenAI via `OPENAI_API_KEY`) or force once with `cogs sync --with-embeddings`.
Embeddings are incremental (`embedded_hash` gates re-embedding), failures
retry on the next sync, and the HNSW index is rebuilt around writes
automatically. They power `/api/similar`, `/api/similarity`, the viz
semantic overlay, and the `semantic_search`/`similar_notes` MCP tools.

## Roadmap

- Lineage mode in the viz (provenance trails note → source → raw file —
  the `/api/lineage/:id` endpoint already exists).
- WebSocket live updates (graph patches as you edit).
- Per-platform release binaries + Zed extension registry submission.
