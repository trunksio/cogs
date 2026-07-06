# cogs

A Karpathy-style, LLM-native knowledge engine for wikilinked markdown vaults:
a graph-powered language server, an embedded [LadybugDB](https://ladybugdb.com)
property graph with full-text **and** vector search, closed-domain question
answering that cites only your own notes, an MCP server for AI agents, and a
browser-based knowledge-graph visualization that replaces Obsidian's graph
view. First-class in [Zed](https://zed.dev) today; the engine is a standalone
binary, so the terminal and any MCP-capable agent work too.

One binary, several roles:

| Command | Role |
|---|---|
| `cogs ask` | Answer a question using **only** the wiki — multi-step retrieval (BM25 + vector + graph) and grounded synthesis with citations; abstains when the wiki is silent |
| `cogs ingest` | Turn a raw capture into a cited source page + concept/entity page updates (LLM-drafted, Rust-validated), written to the working tree for review via `git diff` |
| `cogs distill` | Mine accepted pages and past ingest runs into an SFT dataset (`mlx_lm.lora`-ready) for fine-tuning a local ingest model |
| `cogs lsp` | LSP over stdio: `[[link]]` completion, go-to-definition, backlinks, hover previews, broken-link diagnostics, rename-across-vault, symbols |
| `cogs serve` | Graph visualization + JSON API at `http://127.0.0.1:7117` |
| `cogs viz` | The same viz in a native window with a show/hide toggle (`--toggle`, `--quit`) — bind it to a key in Zed |
| `cogs mcp` | MCP server for agents: `ask`, `search`, `semantic_search`, `get_note`, `neighbours`, `lineage`, `similar_notes`, `list_notes`, `health_report` |
| `cogs sync` | Index the vault into `.cogs/graph.db` (incremental, content-hashed; `--with-embeddings`) |

Plus `cogs init [--karpathy]` (scaffold a vault), `cogs status`, `cogs query "<cypher>"`.

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

## Asking questions (`cogs ask`)

```sh
cogs ask "how does AOA handle agent identity?"
```

Builds a comprehensive, cited answer from **only** what the wiki contains.
A hybrid pipeline: an LLM decomposes the question; cogs retrieves with BM25 +
vector search fused by RRF, then expands over the typed-edge graph for
coverage beyond top-k; the LLM synthesizes with inline `[note-id]` citations;
cogs validates every citation against the retrieved set (invented references
are dropped), surfaces `CONTRADICTS` edges as explicit disputes, and abstains
("the wiki is silent on X") rather than drawing on outside knowledge. Add
`--json` for the structured answer (citations, contradictions). Also available
as the `ask` MCP tool in the agent panel.

The LLM backend is pluggable via `[llm]` in `cogs.toml`, mirroring the
embedding provider: any OpenAI-compatible endpoint — **omlx** (Apple-Silicon
MLX, the local default), Ollama, OpenAI — or Anthropic. Local-first works
with zero data leaving the machine.

## Ingesting captures (`cogs ingest`)

```sh
cogs ingest raw/clips/2026-07-03-some-article.md
git diff   # review; revert anything you disagree with
```

Automates the AGENTS.md ingest workflow with the `[llm]` model, under strict
Rust-side validation — the model drafts, the engine verifies:

- **Extract**: summary, 3–12 independently citable claims, verbatim quotes
  (checked against the raw byte-for-byte, whitespace-tolerantly — fabricated
  quotes are dropped), entities, tags. Long captures are chunked at `##`
  boundaries and merged.
- **Weave**: claims come back wikilinked against retrieval-ranked existing
  pages. A claim may gain `[[brackets]]` but never change text (violations
  revert); links to nonexistent pages are unwrapped; genuinely new
  entities/concepts get stub pages.
- **Update**: affected pages gain an append-only dated section (matching how
  curated vaults accrete) + `source_refs`/`updated` frontmatter edits that
  never re-serialise your YAML. Updates that don't cite the new source page
  are rejected.
- **Contradict**: claims are checked against updated/near-duplicate pages;
  confirmed conflicts land as a `## Contradictions` section + `contradicts:`
  frontmatter (→ `CONTRADICTS` edge) on the new source page, never by
  rewriting the old page.

It refuses to run over a dirty `wiki/` tree (`--force` overrides), detects
already-ingested and near-duplicate captures, appends the `wiki/log.md`
entry, and re-syncs the graph. `--dry-run` prints everything it would write.

## Distilling a training set (`cogs distill`)

Every `cogs ingest` records its model calls under `.cogs/training/` (treat as
precious — it is the one thing in `.cogs/` that can't be regenerated).
`cogs distill` turns the vault into `train.jsonl`/`valid.jsonl` (chat format,
`python3 -m mlx_lm.lora --data <dir>`-compatible):

- **Mined from the vault, today**: every accepted source page paired back to
  its raw capture becomes an extraction + wikilinking example — a mature
  vault is already a training set.
- **`--from-runs`**: recorded ingest inputs paired with the **surviving** file
  content — your post-review edits become the labels; deleted pages and
  removed sections count as rejections and are skipped.

Both use the exact runtime prompt templates, so a LoRA-tuned small model is a
drop-in replacement for the teacher: point `[llm]` at it and ingest runs
fully local.

## How it works

- **Zero-config**: point it at any Obsidian-style vault — every `*.md` is a
  note, `[[wikilinks]]` become `LINKS_TO` edges, tags come from frontmatter
  and inline `#tags`.
- **Config-driven**: a `cogs.toml` in the vault root declares note kinds,
  typed edges derived from frontmatter fields (e.g. `source_refs` →
  `SOURCE_OF`, `contradicts` → `CONTRADICTS`), an immutable resource layer,
  per-kind required-field diagnostics, embedding settings, and (via
  `[ingest]`) where generated pages go and what they're stamped with. Nothing
  about the vault's shape is hardcoded: `examples/aoa-knowledge.cogs.toml`
  drives a Karpathy-style research wiki, and `examples/okf.cogs.toml` drives
  an [Open Knowledge Format](https://github.com/ashtonkj/Nameless.TaskList)
  personal task/message vault — same engine, same features, different
  conventions.
- **One writer, many readers**: the first process to a vault wins a writer
  lock and keeps the graph DB fresh (file watcher + incremental sync); other
  processes open the DB read-only. The LSP's latency-critical features run
  off an in-memory index, so every process is fully functional.
- **The DB is a cache**: wipe `.cogs/` any time; a config change triggers an
  automatic rebuild. Gitignore `.cogs/`.

## Zed setup

Install the extension (from the gallery once published; until then:
`zed: install dev extension` → select `zed-extension/`, which needs rustup
with the `wasm32-wasip2` target). The extension resolves the `cogs` binary
automatically: explicit `lsp.cogs.binary.path` setting → `cogs` on PATH →
**auto-download from GitHub Releases** for your platform. It registers:

- the **language server** for Markdown (completion, backlinks, hover,
  diagnostics, rename),
- the **MCP context server** for the agent panel,
- **`/cogs-init`** — scaffold the open project as a full three-layer wiki
  (the agent writes the files), and **`/cogs-graph`**.

Recommended settings (only needed if you run other Markdown language
servers — Zed routes go-to-definition/references to the first in the list):

```jsonc
{
  "languages": {
    "Markdown": { "language_servers": ["cogs", "..."] }
  }
}
```

For development, pin the binary instead of downloading:

```jsonc
{
  "lsp": {
    "cogs": { "binary": { "path": "/path/to/cogs/target/debug/cogs", "arguments": ["lsp"] } }
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

## Models (`[llm]` and `[embeddings]`)

Both the chat LLM (for `cogs ask` and, soon, ingest) and the embedder are
pluggable, configured in `cogs.toml`:

```toml
[llm]
provider = "omlx"                     # omlx | ollama | openai | anthropic
model = "Qwen3.6-35B-A3B-UD-MLX-4bit"
base_url = "http://127.0.0.1:8000/v1"
api_key_env = "OMLX_API_KEY"          # bearer key kept out of the file

[embeddings]
enabled = true
provider = "omlx"
model = "Qwen3-Embedding-0.6B-8bit"
dim = 1024
endpoint = "http://127.0.0.1:8000/v1"
api_key_env = "OMLX_API_KEY"
query_instruction = "Given a question, retrieve wiki notes that answer it"
```

`omlx`/`ollama`/`openai` are OpenAI-compatible endpoints; `anthropic` is a
native adapter. The local default is **omlx** (Apple Silicon / MLX) so
**nothing leaves the machine**. Embedding is incremental
(`embedded_hash` gates re-embedding), failures retry on the next sync, the
HNSW index is rebuilt around writes, and asymmetric models (Qwen3-Embedding)
get a query-only instruction prefix. Embeddings power `cogs ask`'s semantic
leg, the viz semantic overlay, and the `semantic_search`/`similar_notes` MCP
tools. `cogs ask` works on FTS + graph alone if embeddings are off.

## Status

Shipped and in daily use:

- **Editor UX** — graph-powered Markdown LSP (completion, go-to-def, backlinks,
  hover, broken/ambiguous-link + required-field diagnostics, rename-across-vault,
  symbols), via a Zed extension that auto-downloads the binary.
- **`cogs ask`** — closed-domain, multi-step, cited answering (BM25 + vector +
  graph, RRF-fused; citation validation; contradiction surfacing; abstention).
- **Graph visualization** — WebGL graph with kind/status/tag/edge filters,
  semantic overlay, health overlay, time lens; in the browser or a native
  toggleable window.
- **MCP server** — nine read-only tools for the agent panel.
- **Vault scaffolding** — `cogs init --karpathy` lays down the three-layer
  structure + `AGENTS.md` operating manual; `/cogs-init` does it from the agent.
- **Pluggable local models** — omlx/Ollama/OpenAI/Anthropic for chat and
  embeddings; local-first on Apple Silicon.
- **`cogs ingest`** — LLM-drafted, Rust-validated ingest of raw captures
  (source page, wikilink weaving, append-only page updates, contradiction
  flags) delivered as a reviewable working-tree diff, with every model call
  captured as training data.
- **`cogs distill`** — the vault (and reviewed ingest runs) mined into an
  mlx_lm-ready SFT dataset under the same prompts ingest uses at runtime.
- **Distribution** — per-platform release binaries + extension auto-download.

## Roadmap

- **`/api/ask`** HTTP route so the viz can ask questions; **lineage** viz mode
  (provenance note → source → raw — the `/api/lineage/:id` endpoint exists);
  WebSocket live graph updates.
- **Editor independence** — a `cogs daemon` so many editors share one index;
  a VS Code extension with the graph **inside** a webview panel; Neovim.
- **Controlled vocabularies** — a governed canonical entity layer (SKOS-style
  aliases, entity-linking/resolution, CODEOWNERS + CI approval) to stop entity
  drift ("A2A" vs "Agent2Agent").
- **Finetuned ingest model embedded in cogs** — LoRA-tune a small MLX model on
  the `cogs distill` dataset (entity extraction + summarisation tuned to the
  vault's own conventions), distribute it via `cogs model pull`, and manage it
  as a first-class local provider.
- Zed extension registry submission.

## Development

```sh
cargo test --workspace        # unit + integration tests (fixture vaults)
python3 scripts/lsp_smoke.py ./target/debug/cogs <vault> <note.md>
python3 scripts/mcp_smoke.py ./target/debug/cogs <vault>
```

The C++ build of `lbug` (LadybugDB) makes the first compile slow (~5–10 min);
it's cached afterwards. macOS/Linux binaries embedding lbug must link with
`-rdynamic` (see `build.rs`) or the FTS/VECTOR extensions fail to load. The
crates: `cogs-core` (model/parse/resolve), `cogs-graph` (LadybugDB + embeddings),
`cogs-runtime` (watcher/election), `cogs-lsp`, `cogs-server`, `cogs-llm`
(chat providers), `cogs-ask` (the answering pipeline), `cogs-ingest` (the
ingest pipeline + distill), and the `cogs` binary.
