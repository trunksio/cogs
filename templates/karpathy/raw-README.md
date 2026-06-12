# raw/ — immutable source layer

Primary sources land here and are **never modified, renamed, or deleted**.
Unreliable sources stay; record the unreliability in the corresponding
`wiki/sources/` page instead.

Conventions:
- Date-prefixed filenames: `YYYY-MM-DD-<slug>.<ext>` (capture date).
- Markdown captures carry frontmatter: `title`, `captured_at`, `source` (URL).
- Binary files (PDF, images, audio) get a sibling `<name>.meta.md` with the
  same frontmatter plus an `## About` paragraph.
- Suggested subdirectories: `clips/` (web articles), `research/` (LLM deep
  research), `transcripts/`, `files/` (binaries).
