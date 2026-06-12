# AGENTS.md — operating manual for this wiki

This vault is a Karpathy-style LLM-native knowledge base. **The LLM owns the
bookkeeping** (summaries, cross-references, contradiction surfacing); **the
human owns sources and editorial direction**. Follow this manual whenever you
work in this repo.

## The three layers

| Layer | Owner | Rules |
|---|---|---|
| `raw/` | human | **Immutable.** Never modify, rename, or delete anything here. |
| `wiki/` | LLM | Synthesis. Regenerable. Every claim traces to `raw/` or is marked opinion. |
| everything else | human | Artefacts (drafts, exports). Only the human edits final versions. |

## Tools available

The `cogs` engine indexes this vault into a graph database (`.cogs/`,
regenerable — never commit it).

**MCP server** (`cogs mcp` — usually already wired into the editor):
- `search(query, k)` — BM25 full-text over titles and bodies
- `semantic_search(query, k)` — embedding search; finds related content without keyword overlap
- `get_note(id)` — full markdown + metadata (ids look like `concepts-my-note`)
- `neighbours(id, edge, direction)` — typed adjacency (CITES, SOURCE_OF, CONTRADICTS, SUPERSEDES, TAGGED)
- `lineage(id, max_depth)` — provenance walk: note → source page → raw file
- `list_notes(kind, status, tag)` — filtered enumeration
- `similar_notes(id, k, exclude_linked)` — embedding neighbours; with `exclude_linked` these are auto-link candidates
- `health_report()` — orphans, contradiction pairs, stale pages

**CLI**: `cogs sync` (reindex; runs automatically while the editor is open),
`cogs status`, `cogs query "<cypher>"`, `cogs viz --toggle` (graph window).

**Editor (LSP)**: `[[link]]` completion, go-to-definition, backlinks via
find-references, hover previews, broken-link diagnostics, rename-across-vault.

## Note format

Every `wiki/` page carries frontmatter:

```yaml
---
title: Human Title
kind: concept        # concept | entity | position | question | source | moc
status: draft        # draft | working | stable
updated: YYYY-MM-DD
source_refs:         # raw/ paths backing this page (required for kind: source)
  - raw/clips/YYYY-MM-DD-some-article.md
tags: []
contradicts: []      # wiki pages this contradicts
supersedes: []       # wiki pages this replaces
opinion: false       # true only for positions/
---
```

Link between pages with `[[bare-slug]]` (or `[[dir/slug]]` when ambiguous).
One idea per file; the page is the sole authority on its topic.

## Ingest workflow (new source arrives)

1. **Capture** the source under `raw/` with a date-prefixed filename
   (`raw/clips/YYYY-MM-DD-slug.md`) and frontmatter: `title`, `captured_at`,
   `source` (URL). Binary files get a sibling `<name>.meta.md`. Once written,
   the file is immutable.
2. **Read it fully**, then summarise key takeaways to the human (3–6 bullets)
   and wait for steer before writing.
3. **Write `wiki/sources/<slug>.md`** (`kind: source`, `source_refs` pointing
   at the raw file): `## Summary`, `## Key claims` (each citable, with
   `[[wikilinks]]`), `## Quotes` (verbatim, with locations).
4. **Update 3–12 concept/entity pages**: weave in the new claims with
   wikilinks back to the source page. Create new pages for concepts mentioned
   repeatedly but unpaged.
5. **Never silently merge contradictions** — when a new claim conflicts with
   an existing page, add a `## Contradictions` section citing both sources
   and set `contradicts:` frontmatter; flag it to the human.
6. **Append one entry to `wiki/log.md`**: `## [YYYY-MM-DD] ingest | <source>`
   listing pages touched.

## Answering questions

1. `search` / `semantic_search` to find candidate pages; read them fully
   with `get_note`.
2. Walk `neighbours`/`lineage` for context and provenance.
3. Cite every factual claim as `[[wiki-page]]`. Say when the wiki is silent —
   never fabricate.
4. If the answer was substantive, offer to file it as `wiki/questions/<slug>.md`.

## Lint workflow (periodic)

Run `health_report()` and review: orphan pages, contradiction pairs, stale
pages (not updated in 180 days), plus broken-link diagnostics in the editor.
Write findings to `wiki/_lint/YYYY-MM-DD.md` grouped by severity. **Do not
auto-fix** — the human approves each action.

## Non-negotiables

1. `raw/` is immutable — never modify, rename, delete, or "fix" anything in it.
2. Every `wiki/` claim traces to `raw/` via `source_refs`, or is marked opinion.
3. Never hard-delete wiki pages; mark `status: archive-candidate` and flag.
4. Wikilinks over paths: `[[my-concept]]`, not `wiki/concepts/my-concept.md`.
5. Append to `wiki/log.md` on every writing session — the audit trail is the contract.
6. `.cogs/` is a regenerable cache: never commit it, never hand-edit it.
