//! cogs in the browser: the pure engine (`engine::Engine`) plus wasm-bindgen
//! bindings compiled only on wasm32. Consumed as the npm package
//! `@cogs/engine` by Cogitarium; the pure module is also tested natively
//! against cogs-graph for parity (same vault in → same graph out).

pub mod engine;

#[cfg(target_arch = "wasm32")]
mod bindings {
    use wasm_bindgen::prelude::*;

    use crate::engine::Engine;

    /// JSON-string API: every complex return is a JSON string (mirrors the
    /// server API shapes exactly; avoids serde-wasm-bindgen shape drift).
    #[wasm_bindgen]
    pub struct WasmVault {
        inner: Engine,
    }

    #[wasm_bindgen]
    impl WasmVault {
        #[wasm_bindgen(constructor)]
        pub fn new(config_toml: &str) -> Result<WasmVault, JsError> {
            Ok(WasmVault { inner: Engine::new(config_toml).map_err(|e| JsError::new(&format!("{e:#}")))? })
        }

        #[wasm_bindgen(js_name = isNote)]
        pub fn is_note(&self, rel_path: &str) -> bool {
            self.inner.is_note(rel_path)
        }

        #[wasm_bindgen(js_name = isResource)]
        pub fn is_resource(&self, rel_path: &str) -> bool {
            self.inner.is_resource(rel_path)
        }

        /// Returns the note id.
        pub fn upsert(&mut self, rel_path: &str, content: &str) -> String {
            self.inner.upsert(rel_path, content)
        }

        #[wasm_bindgen(js_name = upsertResource)]
        pub fn upsert_resource(&mut self, rel_path: &str, meta_text: &str, is_markdown: bool) {
            self.inner.upsert_resource(rel_path, meta_text, is_markdown)
        }

        #[wasm_bindgen(js_name = removeByPath)]
        pub fn remove_by_path(&mut self, rel_path: &str) {
            self.inner.remove_by_path(rel_path)
        }

        #[wasm_bindgen(js_name = rebuildDerived)]
        pub fn rebuild_derived(&mut self) {
            self.inner.rebuild_derived()
        }

        #[wasm_bindgen(js_name = noteCount)]
        pub fn note_count(&self) -> usize {
            self.inner.len()
        }

        pub fn meta(&self) -> String {
            self.inner.meta().to_string()
        }

        #[wasm_bindgen(js_name = graphSnapshot)]
        pub fn graph_snapshot(&self, include_resources: bool) -> String {
            self.inner.graph_snapshot(include_resources).to_string()
        }

        /// JSON note detail or null-string "null" when absent.
        pub fn note(&self, id: &str) -> String {
            self.inner.note(id).map(|v| v.to_string()).unwrap_or_else(|| "null".into())
        }

        /// seeds: JSON array of note ids. Returns JSON array of new ids.
        pub fn expand(&self, seeds_json: &str, hops: usize) -> Result<String, JsError> {
            let seeds: Vec<String> =
                serde_json::from_str(seeds_json).map_err(|e| JsError::new(&e.to_string()))?;
            Ok(serde_json::to_string(&self.inner.expand(&seeds, hops)).unwrap())
        }

        /// today_iso: "YYYY-MM-DD" (the browser passes its local date — wasm
        /// has no reliable clock without JS help).
        pub fn health(&self, today_iso: &str) -> String {
            self.inner.health(today_iso).to_string()
        }
    }

    #[wasm_bindgen]
    impl WasmVault {
        /// JSON [{id,title,body,tags}] — the searchable fields, body being
        /// the wikilink-stripped body_text native FTS indexes.
        #[wasm_bindgen(js_name = searchDocs)]
        pub fn search_docs(&self) -> String {
            self.inner.search_docs().to_string()
        }
    }

    /// FTS query sanitizer — parity with native retrieval (cogs-core).
    #[wasm_bindgen(js_name = sanitizeFts)]
    pub fn sanitize_fts(q: &str) -> String {
        cogs_core::textquery::sanitize_fts(q)
    }

    // ---- ingest validation (cogs-ingest-core) ------------------------------
    // Browser-side ingest (M6): a local model drafts an Extraction; THE SAME
    // hard validators that gate native `cogs ingest` gate it here. All inputs
    // and outputs are JSON strings, mirroring the crate's serde shapes.

    use std::collections::{BTreeMap, HashSet};

    use cogs_core::resolve::LinkResolver;
    use cogs_ingest_core::{validators, Extraction, NewPageSpec};

    fn js_err(e: impl std::fmt::Display) -> JsError {
        JsError::new(&e.to_string())
    }

    /// Tolerant parse of a raw model reply into an Extraction (balanced-JSON
    /// extraction, max_tokens truncation repair, one-element-array unwrap —
    /// the same ladder native teacher calls use). Returns the canonical
    /// Extraction JSON.
    #[wasm_bindgen(js_name = parseExtractionReply)]
    pub fn parse_extraction_reply(raw: &str) -> Result<String, JsError> {
        let ex: Extraction = cogs_ingest_core::parse_json_reply(raw)
            .map_err(|e| JsError::new(&format!("{e:#}")))?;
        Ok(serde_json::to_string(&ex).unwrap())
    }

    /// Validate a stage-1 extraction. `existing_slugs_json`: JSON string[] of
    /// slugs already taken in the source dir (drives collision suffixing);
    /// `fallback_slug` replaces a malformed model slug (see
    /// `slugFromFilename`). Returns JSON {"extraction", "warnings"}; an
    /// extraction whose `key_claims` come back empty is unsalvageable and the
    /// ingest must be aborted (native parity).
    #[wasm_bindgen(js_name = validateExtraction)]
    pub fn validate_extraction(
        ex_json: &str,
        raw_body: &str,
        existing_slugs_json: &str,
        fallback_slug: &str,
    ) -> Result<String, JsError> {
        let ex: Extraction = serde_json::from_str(ex_json).map_err(js_err)?;
        let slugs: HashSet<String> = serde_json::from_str(existing_slugs_json).map_err(js_err)?;
        let (extraction, warnings) =
            validators::validate_extraction(ex, raw_body, &slugs, fallback_slug);
        Ok(serde_json::json!({ "extraction": extraction, "warnings": warnings }).to_string())
    }

    /// Validate weave-stage new-page specs. `specs_json`: JSON NewPageSpec[];
    /// `new_page_dirs_json`: JSON {dir: impliedKind} (the [ingest].new_pages
    /// map); `kinds_json`: JSON string[] of known kinds (empty = kinds
    /// unused); `existing_ids_json`: JSON string[] of existing note ids.
    /// Returns JSON {"new_pages", "warnings"}.
    #[wasm_bindgen(js_name = validateNewPages)]
    pub fn validate_new_pages(
        specs_json: &str,
        new_page_dirs_json: &str,
        kinds_json: &str,
        existing_ids_json: &str,
    ) -> Result<String, JsError> {
        let specs: Vec<NewPageSpec> = serde_json::from_str(specs_json).map_err(js_err)?;
        let dirs: BTreeMap<String, String> =
            serde_json::from_str(new_page_dirs_json).map_err(js_err)?;
        let kinds: Vec<String> = serde_json::from_str(kinds_json).map_err(js_err)?;
        let existing: HashSet<String> = serde_json::from_str(existing_ids_json).map_err(js_err)?;
        let (new_pages, warnings) =
            validators::validate_new_pages(specs, &dirs, &kinds, &existing);
        Ok(serde_json::json!({ "new_pages": new_pages, "warnings": warnings }).to_string())
    }

    /// Validate weave-stage linked claims against the plain originals.
    /// `linked_json` / `plain_json`: JSON string[]; `id_slug_pairs_json`:
    /// JSON [id, slug][] over which links must resolve (existing notes +
    /// accepted new pages + the source page — exactly what will exist after
    /// the ingest); `source_dir` is the resolver's same-dir tiebreak context.
    /// Returns JSON {"linked_claims", "warnings"}.
    #[wasm_bindgen(js_name = validateLinkedClaims)]
    pub fn validate_linked_claims(
        linked_json: &str,
        plain_json: &str,
        id_slug_pairs_json: &str,
        source_dir: &str,
    ) -> Result<String, JsError> {
        let linked: Vec<String> = serde_json::from_str(linked_json).map_err(js_err)?;
        let plain: Vec<String> = serde_json::from_str(plain_json).map_err(js_err)?;
        let pairs: Vec<(String, String)> =
            serde_json::from_str(id_slug_pairs_json).map_err(js_err)?;
        let resolver = LinkResolver::new(pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())));
        let (linked_claims, warnings) =
            validators::validate_linked_claims(linked, &plain, &resolver, source_dir);
        Ok(serde_json::json!({ "linked_claims": linked_claims, "warnings": warnings })
            .to_string())
    }

    /// Fallback slug from a raw capture filename (date prefix and extension
    /// stripped, non-slug chars squashed) — feed to `validateExtraction`.
    #[wasm_bindgen(js_name = slugFromFilename)]
    pub fn slug_from_filename(rel_path: &str) -> String {
        validators::slug_from_filename(rel_path)
    }

    /// Reciprocal-Rank-Fusion over ranked id lists (JSON string[][] in,
    /// JSON [id, score][] out, best-first, deterministic ties).
    #[wasm_bindgen(js_name = rrfFuse)]
    pub fn rrf_fuse(lists_json: &str, k: usize) -> Result<String, JsError> {
        let lists: Vec<Vec<String>> =
            serde_json::from_str(lists_json).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(serde_json::to_string(&cogs_core::textquery::rrf_fuse(&lists, k)).unwrap())
    }
}
