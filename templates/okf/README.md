# Open Knowledge Format (OKF) bundle

This directory is an **OKF v0.1** knowledge bundle: a portable, plain-text
representation of knowledge as a graph of markdown files.

## Conventions

- **One concept per file.** A concept's identity is its path relative to the
  bundle root (e.g. `concepts/example.md`).
- **YAML frontmatter.** Each concept opens with a `---` fenced YAML block. The
  only required field is `type`. The full queryable field set is:
  `type`, `title`, `description`, `resource`, `tags`, `timestamp`.
- **Markdown links form the graph.** Reference another concept with a standard
  relative markdown link: `[label](other/concept.md)`. These links are the
  edges of the knowledge graph.
- **Reserved files.** `index.md` is the entry point / progressive-disclosure
  map; `log.md` is an append-only history. `README.md` (this file) documents
  the format. None of these are concepts.

## Working with this bundle

This bundle was produced by / is consumable by [COGS](https://github.com/trunksio/cogs),
a graph wiki engine that treats OKF as an import/export interchange format:

```sh
cogs okf import <path|git-url|tarball>   # index any OKF bundle into a graph
cogs okf lint                            # check OKF v0.1 conformance
cogs okf export --out bundle.tar.gz      # emit a conformant OKF bundle
cogs ask "..."                           # query the bundle with citations
cogs mcp                                 # expose the bundle to AI agents (MCP)
```

Reference: <https://cloud.google.com/blog/products/data-analytics/how-the-open-knowledge-format-can-improve-data-sharing>
