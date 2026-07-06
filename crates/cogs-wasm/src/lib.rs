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

    /// Reciprocal-Rank-Fusion over ranked id lists (JSON string[][] in,
    /// JSON [id, score][] out, best-first, deterministic ties).
    #[wasm_bindgen(js_name = rrfFuse)]
    pub fn rrf_fuse(lists_json: &str, k: usize) -> Result<String, JsError> {
        let lists: Vec<Vec<String>> =
            serde_json::from_str(lists_json).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(serde_json::to_string(&cogs_core::textquery::rrf_fuse(&lists, k)).unwrap())
    }
}
