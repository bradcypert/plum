//! `plum lsp` — an LSP server served straight out of the `plum` binary
//! itself (the same shape `gopls` takes for Go, rather than a separate
//! `plum-lsp` binary/crate): one build artifact, no version-skew risk
//! between the compiler and the thing that's supposed to understand
//! its diagnostics.
//!
//! **v1 scope, deliberate**: diagnostics only (parse errors, module
//! resolution errors, type errors — everything `check::check_modules_
//! diag` can report) on open/change/save/close. No hover, go-to-
//! definition, or completion yet — those need span-indexed type/
//! resolution info threaded out of `plum-types`/`modules.rs` that
//! doesn't exist as a public API yet (see the LSP plan this was scoped
//! from). Full-document sync only (`TextDocumentSyncKind::FULL`, not
//! incremental) — simplest correct thing, and every project this
//! compiler currently targets is small enough that re-typechecking the
//! whole thing on every keystroke is not a real cost yet.
//!
//! Whole-PROJECT semantics, not whole-file: on every edit, the entire
//! workspace root is re-walked and re-checked (see `recheck`), with any
//! currently-open buffers' UNSAVED content overlaid onto the on-disk
//! tree. `check_modules_diag` (like the rest of this codebase's
//! front end) reports at most one `CompileError` per check — so does
//! this server. A project with more than one error only ever shows the
//! first one until it's fixed; a known, honest v1 limitation, not a
//! bug.

mod position;

use crate::check::check_modules_diag;
use crate::project::collect_project_files_with_paths;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Starts serving `plum lsp` over stdio and blocks until the client
/// disconnects (stdin closes) — the standard LSP transport, and the
/// one every editor's client config expects by default. Builds its own
/// (single, this-call-only) tokio runtime rather than requiring
/// `main.rs` to be async itself, since every other `plum` subcommand
/// is synchronous and should stay that way.
pub fn run() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime `plum lsp` needs");
    rt.block_on(run_async());
}

async fn run_async() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

struct Backend {
    client: Client,
    /// The workspace root — set once from `initialize`'s `root_uri` (or
    /// its first `workspace_folders` entry). Every recheck walks this
    /// same directory tree, matching `plum run <project-dir>`'s own
    /// "a project is a directory of `.plum` files" model — there's no
    /// project manifest file to look for.
    root: Mutex<Option<PathBuf>>,
    /// Unsaved editor buffer content, keyed by file URI — overlaid onto
    /// the on-disk tree on every recheck so diagnostics reflect what's
    /// actually in the editor, not the last save.
    open_docs: Mutex<HashMap<Url, String>>,
    /// The file a diagnostic was last published to, if any — so the
    /// NEXT recheck knows to clear it first if the error moved to a
    /// different file (or disappeared). `check_modules_diag` only ever
    /// reports one error at a time (see this module's own doc comment),
    /// so this server only ever needs to track one.
    last_diagnosed: Mutex<Option<Url>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Backend { client, root: Mutex::new(None), open_docs: Mutex::new(HashMap::new()), last_diagnosed: Mutex::new(None) }
    }

    /// Re-walks the workspace root, overlays any open buffers' unsaved
    /// content, type-checks the result, and publishes (or clears)
    /// exactly one diagnostic accordingly. Called after every open/
    /// change/save/close — see this module's own doc comment for why
    /// whole-project-on-every-edit is an acceptable v1 cost.
    async fn recheck(&self) {
        let Some(root) = self.root.lock().unwrap().clone() else {
            return;
        };

        let files = match collect_project_files_with_paths(&root) {
            Ok(files) => files,
            Err(e) => {
                self.client.log_message(MessageType::ERROR, format!("plum lsp: failed to read project {root:?}: {e}")).await;
                return;
            }
        };

        // Block-scoped so the `MutexGuard` (not `Send`, and so unable to
        // be held across any `.await` below without making this whole
        // async fn's future non-`Send` — which `tower_lsp`'s trait
        // requires it to be) is provably dropped by the time this block
        // ends, rather than relying on the async-fn-to-state-machine
        // lowering to notice a manual `drop()` call partway through a
        // single flat scope.
        let overlaid: Vec<(PathBuf, String, String)> = {
            let open_docs = self.open_docs.lock().unwrap();
            files
                .into_iter()
                .map(|(path, mpath, disk_src)| {
                    let src = Url::from_file_path(&path).ok().and_then(|uri| open_docs.get(&uri).cloned()).unwrap_or(disk_src);
                    (path, mpath, src)
                })
                .collect()
        };

        let modules: Vec<(&str, &str)> = overlaid.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
        let sources = crate::ModuleSources::new(&modules);

        let new_diagnosed = match check_modules_diag(&modules) {
            Ok(()) => None,
            Err(e) => match e.span.and_then(|span| sources.locate_offset(span.start).map(|start| (start, span))) {
                Some(((idx, local_start), span)) => {
                    let (path, _mpath, src) = &overlaid[idx];
                    let Ok(uri) = Url::from_file_path(path) else {
                        self.client.log_message(MessageType::ERROR, format!("plum lsp: {}", e.message)).await;
                        return;
                    };
                    let start_pos = position::byte_offset_to_position(src, local_start);
                    // The span's END might land past this module's own
                    // source (rare — see `ModuleSources::render`'s own
                    // comment on the same edge case) or, in principle,
                    // inside a DIFFERENT file's range entirely; either
                    // way, falling back to a zero-width range at
                    // `start_pos` is always safe and never panics.
                    let end_pos = sources
                        .locate_offset(span.end)
                        .filter(|(end_idx, _)| *end_idx == idx)
                        .map(|(_, local_end)| position::byte_offset_to_position(src, local_end))
                        .unwrap_or(start_pos);
                    let diagnostic = Diagnostic {
                        range: Range::new(start_pos, end_pos),
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("plum".to_string()),
                        message: e.message.clone(),
                        ..Diagnostic::default()
                    };
                    self.client.publish_diagnostics(uri.clone(), vec![diagnostic], None).await;
                    Some(uri)
                }
                // A spanless error (see `CompileError`'s own doc
                // comment — genuinely locationless, e.g. a codegen- or
                // interpreter-runtime-level failure) can't be attached
                // to a file/range at all; surface it as a log message
                // instead of silently dropping it.
                None => {
                    self.client.log_message(MessageType::ERROR, format!("plum lsp: {}", e.message)).await;
                    None
                }
            },
        };

        // Each `.lock()` guard is scoped to its own statement here (not
        // held across the `.await` in between) — a `std::sync::Mutex`
        // guard isn't `Send`, so holding one across an await point
        // would make this whole async fn's future non-`Send`, which
        // `tower_lsp`'s trait requires it to be.
        let prev = self.last_diagnosed.lock().unwrap().take();
        if let Some(prev) = prev
            && new_diagnosed.as_ref() != Some(&prev)
        {
            self.client.publish_diagnostics(prev, Vec::new(), None).await;
        }
        *self.last_diagnosed.lock().unwrap() = new_diagnosed;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .or_else(|| params.workspace_folders.as_ref()?.first()?.uri.to_file_path().ok());
        *self.root.lock().unwrap() = root;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo { name: "plum-lsp".to_string(), version: Some(env!("CARGO_PKG_VERSION").to_string()) }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.recheck().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.open_docs.lock().unwrap().insert(params.text_document.uri, params.text_document.text);
        self.recheck().await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        // Full sync only (see this module's doc comment) — the LAST
        // content change event IS the whole new document text.
        if let Some(change) = params.content_changes.pop() {
            self.open_docs.lock().unwrap().insert(params.text_document.uri, change.text);
        }
        self.recheck().await;
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        self.recheck().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.open_docs.lock().unwrap().remove(&params.text_document.uri);
        self.recheck().await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }
}
