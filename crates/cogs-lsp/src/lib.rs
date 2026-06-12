//! Graph-powered LSP server for wikilinked markdown vaults.
//!
//! All latency-critical features run off cogs-runtime's in-memory VaultIndex;
//! the graph DB is kept fresh in the background for MCP/HTTP consumers.

pub mod diagnostics;
pub mod pos;

use std::path::PathBuf;
use std::sync::OnceLock;

use dashmap::DashMap;
use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing::info;

use cogs_core::config::Vault;
use cogs_core::note::{Link, ParsedNote};
use cogs_core::resolve::Resolution;
use cogs_runtime::Runtime;

use pos::{offset_to_position, position_to_offset, span_to_range};

pub struct Backend {
    client: Client,
    /// Vault root override from the CLI (beats workspace-root discovery).
    vault_override: Option<PathBuf>,
    runtime: OnceLock<Runtime>,
    /// Open-document text keyed by vault-relative path.
    docs: DashMap<String, String>,
}

impl Backend {
    fn runtime(&self) -> Result<&Runtime> {
        self.runtime.get().ok_or_else(Error::internal_error)
    }

    fn rel_of(&self, uri: &Uri) -> Option<String> {
        let path = uri.to_file_path()?;
        self.runtime.get()?.rel_path(&path)
    }

    fn uri_of(&self, rel: &str) -> Option<Uri> {
        let rt = self.runtime.get()?;
        Uri::from_file_path(rt.vault.root.join(rel))
    }

    /// Current text of a note: open-doc overlay, else disk.
    fn text_of(&self, rel: &str) -> Option<String> {
        if let Some(t) = self.docs.get(rel) {
            return Some(t.clone());
        }
        let rt = self.runtime.get()?;
        std::fs::read_to_string(rt.vault.root.join(rel)).ok()
    }

    /// The parsed note for a URI plus its current text.
    fn note_and_text(&self, uri: &Uri) -> Option<(ParsedNote, String)> {
        let rel = self.rel_of(uri)?;
        let text = self.text_of(&rel)?;
        let rt = self.runtime.get()?;
        let idx = rt.index.read().unwrap();
        let note = idx.get_by_path(&rel)?.clone();
        Some((note, text))
    }

    fn link_at(note: &ParsedNote, offset: usize) -> Option<&Link> {
        note.links
            .iter()
            .find(|l| !l.masked && l.span.contains(&offset))
    }

    async fn publish_diagnostics_for(&self, uri: Uri, rel: &str) {
        let Some(rt) = self.runtime.get() else { return };
        let Some(text) = self.text_of(rel) else { return };
        let diags = {
            let idx = rt.index.read().unwrap();
            match idx.get_by_path(rel) {
                Some(note) => {
                    diagnostics::for_note(note, idx.resolutions(&note.id), &rt.vault.config, &text)
                }
                None => vec![],
            }
        };
        self.client.publish_diagnostics(uri, diags, None).await;
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Vault root: CLI override > initialization_options.vault_root >
        // first workspace folder > cwd.
        let root = self
            .vault_override
            .clone()
            .or_else(|| {
                params
                    .initialization_options
                    .as_ref()
                    .and_then(|o| o.get("vault_root"))
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
            })
            .or_else(|| {
                params
                    .workspace_folders
                    .as_ref()
                    .and_then(|f| f.first())
                    .and_then(|f| f.uri.to_file_path())
                    .map(|p| p.into_owned())
            })
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(Error::internal_error)?;

        let vault = Vault::discover(&root).map_err(|e| {
            tracing::error!("vault discovery failed: {e:#}");
            Error::internal_error()
        })?;
        info!(root = %vault.root.display(), "initializing cogs lsp");
        let runtime = Runtime::start(vault, true).map_err(|e| {
            tracing::error!("runtime start failed: {e:#}");
            Error::internal_error()
        })?;
        let _ = self.runtime.set(runtime);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "cogs".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["[".into(), "#".into()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let n = self
            .runtime
            .get()
            .map(|rt| rt.index.read().unwrap().len())
            .unwrap_or(0);
        self.client
            .log_message(MessageType::INFO, format!("cogs ready: {n} notes indexed"))
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some(rt) = self.runtime.get() {
            rt.shutdown();
        }
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(rel) = self.rel_of(&uri) else { return };
        let text = params.text_document.text;
        self.docs.insert(rel.clone(), text.clone());
        if let Some(rt) = self.runtime.get() {
            rt.update_overlay(&rel, Some(text));
        }
        self.publish_diagnostics_for(uri, &rel).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(rel) = self.rel_of(&uri) else { return };
        let Some(change) = params.content_changes.into_iter().next_back() else { return };
        self.docs.insert(rel.clone(), change.text.clone());
        if let Some(rt) = self.runtime.get() {
            rt.update_overlay(&rel, Some(change.text));
        }
        self.publish_diagnostics_for(uri, &rel).await;
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        if let Some(rt) = self.runtime.get() {
            rt.sync_now();
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(rel) = self.rel_of(&params.text_document.uri) else { return };
        self.docs.remove(&rel);
        if let Some(rt) = self.runtime.get() {
            rt.update_overlay(&rel, None);
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let Some(rel) = self.rel_of(&uri) else { return Ok(None) };
        let Some(text) = self.text_of(&rel) else { return Ok(None) };
        let offset = position_to_offset(&text, pos);
        let rt = self.runtime()?;

        // Wikilink context: an unclosed `[[` on the current line before the cursor.
        let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let before = &text[line_start..offset];
        if let Some(open) = before.rfind("[[") {
            if !before[open..].contains("]]") {
                let typed = before[open + 2..].to_string();
                let typed_start = line_start + open + 2;
                let idx = rt.index.read().unwrap();
                let mut items: Vec<CompletionItem> = idx
                    .notes()
                    .map(|n| {
                        // Insert the bare slug when unambiguous, the id otherwise.
                        let insert = match idx.resolver().resolve(&n.slug, &n.dir) {
                            Resolution::Resolved(id) if id == n.id => n.slug.clone(),
                            _ => n.id.clone(),
                        };
                        CompletionItem {
                            label: insert.clone(),
                            kind: Some(CompletionItemKind::FILE),
                            detail: Some(match &n.kind {
                                Some(k) => format!("{} · {}", n.title, k),
                                None => n.title.clone(),
                            }),
                            filter_text: Some(format!("{} {}", insert, n.title)),
                            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                                range: Range::new(
                                    offset_to_position(&text, typed_start),
                                    offset_to_position(&text, offset),
                                ),
                                new_text: insert,
                            })),
                            ..Default::default()
                        }
                    })
                    .collect();
                // Light prefix pre-filter to keep payloads sane on big vaults.
                if !typed.is_empty() {
                    let t = typed.to_lowercase();
                    items.retain(|i| {
                        i.filter_text
                            .as_deref()
                            .unwrap_or(&i.label)
                            .to_lowercase()
                            .contains(&t)
                    });
                }
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        // Tag context: word starting with '#'.
        if let Some(hash) = before.rfind('#') {
            let typed = &before[hash + 1..];
            if typed
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '/' || c == '_')
            {
                let idx = rt.index.read().unwrap();
                let mut tags: Vec<String> =
                    idx.notes().flat_map(|n| n.tags.iter().cloned()).collect();
                tags.sort();
                tags.dedup();
                let items = tags
                    .into_iter()
                    .filter(|t| typed.is_empty() || t.starts_with(typed))
                    .map(|t| CompletionItem {
                        label: t,
                        kind: Some(CompletionItemKind::KEYWORD),
                        ..Default::default()
                    })
                    .collect();
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        Ok(None)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let Some((note, text)) = self.note_and_text(&uri) else { return Ok(None) };
        let offset = position_to_offset(&text, pos);
        let Some(link) = Self::link_at(&note, offset) else { return Ok(None) };
        let rt = self.runtime()?;

        let target = {
            let idx = rt.index.read().unwrap();
            match idx.resolver().resolve(&link.target, &note.dir) {
                Resolution::Resolved(id) => idx.get(&id).cloned(),
                _ => None,
            }
        };
        let Some(target) = target else { return Ok(None) };
        let Some(target_uri) = self.uri_of(&target.rel_path) else { return Ok(None) };

        // Anchor: jump to the matching heading when present.
        let range = match &link.anchor {
            Some(anchor) => {
                let target_text = self.text_of(&target.rel_path).unwrap_or_default();
                let a = anchor.to_lowercase().replace('-', " ");
                target
                    .headings
                    .iter()
                    .find(|h| h.text.to_lowercase().replace('-', " ") == a)
                    .map(|h| span_to_range(&target_text, &h.span))
                    .unwrap_or_default()
            }
            None => Range::default(),
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location::new(target_uri, range))))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let Some((note, text)) = self.note_and_text(&uri) else { return Ok(None) };
        let offset = position_to_offset(&text, pos);
        let rt = self.runtime()?;

        // On a wikilink: references of its target; otherwise of this note.
        let target_id = {
            let idx = rt.index.read().unwrap();
            match Self::link_at(&note, offset) {
                Some(link) => match idx.resolver().resolve(&link.target, &note.dir) {
                    Resolution::Resolved(id) => id,
                    _ => return Ok(None),
                },
                None => note.id.clone(),
            }
        };

        let spans = {
            let idx = rt.index.read().unwrap();
            idx.backlinks(&target_id).to_vec()
        };
        let mut locations = Vec::new();
        for ls in spans {
            let rel = {
                let idx = rt.index.read().unwrap();
                match idx.get(&ls.note_id) {
                    Some(src) => src.rel_path.clone(),
                    None => continue,
                }
            };
            let Some(src_text) = self.text_of(&rel) else { continue };
            let Some(src_uri) = self.uri_of(&rel) else { continue };
            locations.push(Location::new(src_uri, span_to_range(&src_text, &ls.span)));
        }
        Ok(Some(locations))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let Some((note, text)) = self.note_and_text(&uri) else { return Ok(None) };
        let offset = position_to_offset(&text, pos);
        let Some(link) = Self::link_at(&note, offset) else { return Ok(None) };
        let rt = self.runtime()?;

        let (target, backlink_count) = {
            let idx = rt.index.read().unwrap();
            match idx.resolver().resolve(&link.target, &note.dir) {
                Resolution::Resolved(id) => {
                    let count = idx.backlinks(&id).len();
                    (idx.get(&id).cloned(), count)
                }
                Resolution::Ambiguous(candidates) => {
                    let md = format!(
                        "**Ambiguous link** — candidates:\n\n{}",
                        candidates
                            .iter()
                            .map(|c| format!("- `{c}`"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: md,
                        }),
                        range: Some(span_to_range(&text, &link.span)),
                    }));
                }
                Resolution::Broken => return Ok(None),
            }
        };
        let Some(target) = target else { return Ok(None) };

        let mut md = format!("**{}**", target.title);
        let mut meta = Vec::new();
        if let Some(k) = &target.kind {
            meta.push(k.clone());
        }
        if let Some(s) = &target.status {
            meta.push(s.clone());
        }
        if let Some(u) = &target.updated {
            meta.push(format!("updated {u}"));
        }
        meta.push(format!("{backlink_count} backlinks"));
        md.push_str(&format!("\n\n_{}_\n\n---\n\n", meta.join(" · ")));
        let preview: String = target.body_text.trim_start().chars().take(600).collect();
        md.push_str(&preview);
        if target.body_text.chars().count() > 600 {
            md.push('…');
        }

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range: Some(span_to_range(&text, &link.span)),
        }))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let Some((note, text)) = self.note_and_text(&params.text_document.uri) else {
            return Ok(None);
        };
        let offset = position_to_offset(&text, params.position);
        Ok(Self::link_at(&note, offset).map(|link| {
            PrepareRenameResponse::RangeWithPlaceholder {
                range: span_to_range(&text, &link.target_span),
                placeholder: link.target.clone(),
            }
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = params.new_name.trim().to_string();
        if new_name.is_empty() || new_name.contains(['[', ']', '#', '|', '\n']) {
            return Err(Error::invalid_params("invalid link name"));
        }
        let Some((note, text)) = self.note_and_text(&uri) else { return Ok(None) };
        let offset = position_to_offset(&text, pos);
        let Some(link) = Self::link_at(&note, offset) else { return Ok(None) };
        let rt = self.runtime()?;

        let idx = rt.index.read().unwrap();
        let Resolution::Resolved(target_id) = idx.resolver().resolve(&link.target, &note.dir)
        else {
            return Ok(None);
        };
        let Some(target) = idx.get(&target_id) else { return Ok(None) };

        // Edit every wikilink whose target resolves to this note.
        let mut ops: Vec<DocumentChangeOperation> = Vec::new();
        for src in idx.notes() {
            let Some(src_text) = self.text_of(&src.rel_path) else { continue };
            let edits: Vec<TextEdit> = src
                .links
                .iter()
                .zip(idx.resolutions(&src.id))
                .filter(|(l, r)| {
                    !l.masked && matches!(r, Resolution::Resolved(id) if *id == target_id)
                })
                .map(|(l, _)| TextEdit {
                    range: span_to_range(&src_text, &l.target_span),
                    new_text: new_name.clone(),
                })
                .collect();
            if !edits.is_empty() {
                let uri = self.uri_of(&src.rel_path).ok_or_else(Error::internal_error)?;
                ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri,
                        version: None,
                    },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                }));
            }
        }

        // Rename the target file itself (same directory, new slug).
        let old_rel = target.rel_path.clone();
        let new_rel = match old_rel.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{new_name}.md"),
            None => format!("{new_name}.md"),
        };
        if new_rel != old_rel {
            let old_uri = self.uri_of(&old_rel).ok_or_else(Error::internal_error)?;
            let new_uri = self.uri_of(&new_rel).ok_or_else(Error::internal_error)?;
            ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                old_uri,
                new_uri,
                options: None,
                annotation_id: None,
            })));
        }

        Ok(Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(ops)),
            ..Default::default()
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some((note, text)) = self.note_and_text(&params.text_document.uri) else {
            return Ok(None);
        };
        let uri = params.text_document.uri;
        #[allow(deprecated)]
        let symbols: Vec<SymbolInformation> = note
            .headings
            .iter()
            .map(|h| SymbolInformation {
                name: h.text.clone(),
                kind: SymbolKind::STRING,
                tags: None,
                deprecated: None,
                location: Location::new(uri.clone(), span_to_range(&text, &h.span)),
                container_name: None,
            })
            .collect();
        Ok(Some(DocumentSymbolResponse::Flat(symbols)))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let rt = self.runtime()?;
        let q = params.query.to_lowercase();
        let idx = rt.index.read().unwrap();
        #[allow(deprecated)]
        let symbols: Vec<SymbolInformation> = idx
            .notes()
            .filter(|n| {
                q.is_empty()
                    || n.title.to_lowercase().contains(&q)
                    || n.id.to_lowercase().contains(&q)
            })
            .take(200)
            .filter_map(|n| {
                Some(SymbolInformation {
                    name: n.title.clone(),
                    kind: SymbolKind::FILE,
                    tags: None,
                    deprecated: None,
                    location: Location::new(self.uri_of(&n.rel_path)?, Range::default()),
                    container_name: n.kind.clone(),
                })
            })
            .collect();
        Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
    }
}

/// Run the LSP server on stdio until the client disconnects.
pub async fn run_stdio(vault_override: Option<PathBuf>) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        vault_override,
        runtime: OnceLock::new(),
        docs: DashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
