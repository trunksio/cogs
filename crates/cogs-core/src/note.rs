use std::ops::Range;

use chrono::NaiveDate;

/// A wikilink occurrence in a note body: `[[target]]`, `[[target|alias]]`, `[[target#anchor]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub target: String,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// Byte range of the full `[[...]]` token in the file.
    pub span: Range<usize>,
    /// Byte range of just the target text inside the brackets — what a
    /// rename replaces.
    pub target_span: Range<usize>,
    /// True when the link sits inside a code fence/span. The graph sync
    /// includes masked links (parity with sync_graph.py, which does not mask);
    /// editor features (diagnostics, rename) skip them.
    pub masked: bool,
}

/// A list item in a frontmatter edge field (e.g. `contradicts: [foo]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeFieldItem {
    pub field: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub span: Range<usize>,
}

/// Span of a wikilink plus which note it lives in — used by the backlink index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub note_id: String,
    pub span: Range<usize>,
    pub target_span: Range<usize>,
}

/// Fully parsed note, ready for indexing.
#[derive(Debug, Clone)]
pub struct ParsedNote {
    /// Vault-relative path, forward slashes: `wiki/concepts/agentic-unit.md`.
    pub rel_path: String,
    /// Stable id derived from the path: `concepts-agentic-unit`.
    pub id: String,
    /// File stem: `agentic-unit`.
    pub slug: String,
    /// First path component under the id prefix (`concepts`), or empty.
    pub dir: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub created: Option<NaiveDate>,
    pub updated: Option<NaiveDate>,
    pub tags: Vec<String>,
    /// Full frontmatter projected to JSON (stored on the node for filtering).
    pub frontmatter_json: String,
    /// Byte range of the frontmatter block (including delimiters), if present.
    pub frontmatter_range: Option<Range<usize>>,
    /// Body with wikilinks stripped to display text — what gets FTS-indexed.
    pub body_text: String,
    /// sha256 hex of `body_text`.
    pub body_hash: String,
    pub links: Vec<Link>,
    /// Frontmatter-driven edge values (source_refs, contradicts, ...), per configured field.
    pub edge_fields: Vec<EdgeFieldItem>,
    pub headings: Vec<Heading>,
}

/// A resource (immutable raw-layer file). Markdown resources carry body text;
/// binaries are metadata-only via their sibling `.meta.md`.
#[derive(Debug, Clone)]
pub struct ParsedResource {
    pub rel_path: String,
    pub title: String,
    pub captured: Option<NaiveDate>,
    pub source_date: Option<NaiveDate>,
    pub url: Option<String>,
    pub body_text: String,
    pub body_hash: String,
}

/// Lenient date parsing matching sync_graph.py's parse_date: first 10 chars,
/// ISO YYYY-MM-DD. YAML may hand us a string or (rarely) other scalars.
pub fn parse_date_lenient(value: &serde_json::Value) -> Option<NaiveDate> {
    let s = value.as_str()?;
    let s = if s.len() > 10 { s.get(..10)? } else { s };
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}
