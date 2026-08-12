//! `plum lsp` — an LSP server served straight out of the `plum` binary
//! itself (the same shape `gopls` takes for Go, rather than a separate
//! `plum-lsp` binary/crate): one build artifact, no version-skew risk
//! between the compiler and the thing that's supposed to understand
//! its diagnostics.
//!
//! **v1 scope**: diagnostics (parse/resolution/type errors) on open/
//! change/save/close, plus hover (resolved type) and go-to-definition
//! (variables/params/`let`s, function/global calls, struct/enum names,
//! `.field` access, enum variant references — see `plum_types::infer::
//! Infer`'s `node_types`/`definitions` doc comments for exactly what's
//! covered and what isn't). No completion yet. Full-document sync only
//! (`TextDocumentSyncKind::FULL`, not incremental) — simplest correct
//! thing, and every project this compiler currently targets is small
//! enough that re-typechecking the whole thing on every keystroke is
//! not a real cost yet.
//!
//! Whole-PROJECT semantics, not whole-file: on every edit, the entire
//! workspace root is re-walked and re-checked (see `recheck`), with any
//! currently-open buffers' UNSAVED content overlaid onto the on-disk
//! tree. `check_modules_diag` (like the rest of this codebase's
//! front end) reports at most one `CompileError` per check — so does
//! this server. A project with more than one error only ever shows the
//! first one until it's fixed; a known, honest v1 limitation, not a
//! bug.
//!
//! Every event handler goes through `Backend::recheck_debounced`, never
//! `recheck` directly — a short debounce plus a monotonic `Generation`
//! tag (see that type's own doc comment) together fix a real race a
//! whole-project-on-every-keystroke design invites: overlapping
//! `recheck` calls finishing in a DIFFERENT order than they started in,
//! where an older one finishing LAST would publish a stale diagnostic
//! over a newer, already-correct one.

mod position;

use crate::check::{check_modules_diag, check_modules_diag_lenient_infer, check_modules_diag_with_infer};
use crate::project::collect_project_files_with_paths;
use plum_syntax::ast;
use plum_types::infer::Infer;
use plum_types::types::Type;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// How long `recheck_debounced` waits, after bumping the generation
/// counter, before actually doing the (whole-project) walk + re-
/// typecheck — see `Backend::generation`'s own doc comment for why
/// this exists at all. Short enough that a deliberate pause (finished
/// typing, moved the cursor) still feels instant; long enough that a
/// realistic typing burst collapses into one recheck instead of one
/// per keystroke.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// A monotonic "which edit is this for" tag, pulled out of `Backend`
/// itself so its actual correctness property — an older `bump()`'s
/// return value must stop being `is_current` once a newer `bump()`
/// happens, even interleaved from multiple threads — is unit-testable
/// directly, without needing a real `tower_lsp::Client` (no public
/// test-double constructor exists for one). See `Backend::generation`'s
/// own doc comment for what problem this solves and how it's used.
#[derive(Default)]
struct Generation(AtomicU64);

impl Generation {
    /// Advances to a new generation and returns its tag.
    fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// `true` iff `tag` (a previous `bump()`'s return value) is still
    /// the MOST RECENT one — i.e. nothing has called `bump()` again
    /// since.
    fn is_current(&self, tag: u64) -> bool {
        self.0.load(Ordering::SeqCst) == tag
    }
}

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
    /// Every file a diagnostic was last published to — so the NEXT
    /// recheck knows to clear any that are no longer broken (fixed, or
    /// the error moved elsewhere). More than one file can appear here
    /// at once: multiple FILES can each show their own parse error
    /// simultaneously (`parse_every_module_diag`, since each file
    /// parses fully independently — no cascade risk); type/resolution
    /// errors still cap out at one file at a time — see `recheck`'s own
    /// doc comment for why that boundary is deliberate, not just
    /// unfinished.
    last_diagnosed: Mutex<std::collections::HashSet<Url>>,
    /// Bumped once per open/change/save/close event, and read back
    /// twice per `recheck` (see `recheck_debounced`/`recheck`
    /// themselves) — a monotonic "which edit is this recheck for" tag.
    /// Whole-project re-walk + re-typecheck is not instant, and every
    /// editor event (not just `did_change`) triggers one with zero
    /// debounce before this existed: a burst of edits could launch
    /// several overlapping `recheck`s that finish in a DIFFERENT order
    /// than they started in, and an older one finishing LAST would
    /// publish its now-stale diagnostic over a newer, already-correct
    /// one — a real, live bug, not hypothetical (confirmed by manually
    /// racing two `recheck` calls with an artificial delay before this
    /// fix). Fixed two ways together: `recheck_debounced` (used by
    /// every LSP event handler now, in place of calling `recheck`
    /// directly) waits `DEBOUNCE` before doing any real work at all, so
    /// a realistic typing burst collapses into a single recheck rather
    /// than one per keystroke; and `recheck` itself re-checks this
    /// counter immediately before EVERY diagnostic-publishing/state-
    /// mutating step, abandoning silently (not erroring) if a NEWER
    /// event superseded it while it was still walking/typechecking —
    /// the newer recheck's own result is authoritative instead.
    generation: Generation,
    /// Top-level completion items (functions, globals, structs, enum
    /// names + variants, extern functions — see `top_level_completion_
    /// items`), refreshed on every SUCCESSFUL `recheck`. A CACHE, not
    /// computed fresh per completion request, on purpose: general
    /// completion is most useful exactly while the user is mid-edit —
    /// often the one moment the CURRENT buffer doesn't parse at all
    /// (this codebase's parser has no error-recovery — see `plum lsp`'s
    /// own module doc comment) — so a request arriving then still needs
    /// SOMETHING to suggest. Starts empty (before the first successful
    /// recheck); never cleared back to empty on a LATER failed recheck
    /// — stale-but-mostly-still-right beats nothing while the user
    /// fixes whatever they just broke.
    last_good_completions: Mutex<Vec<CompletionItem>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Backend {
            client,
            root: Mutex::new(None),
            open_docs: Mutex::new(HashMap::new()),
            last_diagnosed: Mutex::new(std::collections::HashSet::new()),
            generation: Generation::default(),
            last_good_completions: Mutex::new(Vec::new()),
        }
    }

    /// Bumps the generation counter, waits `DEBOUNCE`, then runs
    /// `recheck` — UNLESS a later event already bumped the counter
    /// again during the wait, in which case this call abandons
    /// silently (the later event's own debounced call will run
    /// instead). Every LSP event handler below calls this, never
    /// `recheck` directly.
    async fn recheck_debounced(&self) {
        let my_gen = self.generation.bump();
        tokio::time::sleep(DEBOUNCE).await;
        if !self.generation.is_current(my_gen) {
            return;
        }
        self.recheck(my_gen).await;
    }

    /// `true` iff a NEWER event has bumped the generation counter past
    /// `my_gen` since this `recheck` call started — see `generation`'s
    /// own doc comment.
    fn is_stale(&self, my_gen: u64) -> bool {
        !self.generation.is_current(my_gen)
    }

    /// Walks the workspace root and overlays any open buffers' unsaved
    /// content — the shared front half of `recheck`, `hover`, and
    /// `goto_definition` (all three need the exact same "what's the
    /// CURRENT content of every file in this project" view before
    /// type-checking it). `None` (after logging why) if the root isn't
    /// set yet or the project can't even be read — every caller treats
    /// that identically (nothing to check/hover/jump to yet).
    async fn overlaid_files(&self) -> Option<Vec<(PathBuf, String, String)>> {
        let root = self.root.lock().unwrap().clone()?;

        let files = match collect_project_files_with_paths(&root) {
            Ok(files) => files,
            Err(e) => {
                self.client.log_message(MessageType::ERROR, format!("plum lsp: failed to read project {root:?}: {e}")).await;
                return None;
            }
        };

        // Block-scoped so the `MutexGuard` (not `Send`, and so unable to
        // be held across any `.await` below without making this whole
        // async fn's future non-`Send` — which `tower_lsp`'s trait
        // requires it to be) is provably dropped by the time this block
        // ends, rather than relying on the async-fn-to-state-machine
        // lowering to notice a manual `drop()` call partway through a
        // single flat scope.
        let open_docs = self.open_docs.lock().unwrap();
        Some(
            files
                .into_iter()
                .map(|(path, mpath, disk_src)| {
                    let src = Url::from_file_path(&path).ok().and_then(|uri| open_docs.get(&uri).cloned()).unwrap_or(disk_src);
                    (path, mpath, src)
                })
                .collect(),
        )
    }

    /// Turns a `CompileError` into a `(Url, Diagnostic)` pair, using
    /// `sources`/`overlaid` to locate its span — shared by every path
    /// through `recheck` that ends up with an error to report (per-file
    /// parse errors AND the single type/resolution-error fallback).
    /// `None` (after logging instead) for a genuinely spanless error
    /// (codegen-/interpreter-runtime-level, or a file path that
    /// couldn't round-trip through `Url::from_file_path`) — there's no
    /// file/range to attach a `Diagnostic` to at all in that case.
    async fn diagnostic_for(
        &self,
        e: &plum_syntax::error::CompileError,
        sources: &crate::ModuleSources,
        overlaid: &[(PathBuf, String, String)],
    ) -> Option<(Url, Diagnostic)> {
        let (idx, local_start) = e.span.and_then(|span| sources.locate_offset(span.start))?;
        let span = e.span.expect("just matched Some above via `e.span.and_then`");
        let (path, _mpath, src) = &overlaid[idx];
        let Ok(uri) = Url::from_file_path(path) else {
            self.client.log_message(MessageType::ERROR, format!("plum lsp: {}", e.message)).await;
            return None;
        };
        let start_pos = position::byte_offset_to_position(src, local_start);
        // The span's END might land past this module's own source
        // (rare — see `ModuleSources::render`'s own comment on the same
        // edge case) or, in principle, inside a DIFFERENT file's range
        // entirely; either way, falling back to a zero-width range at
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
        Some((uri, diagnostic))
    }

    /// Re-walks the workspace root, overlays any open buffers' unsaved
    /// content, type-checks the result, and publishes (or clears)
    /// diagnostics accordingly. `my_gen` is checked against the CURRENT
    /// generation counter immediately before every publish/state-
    /// mutating step below — see `generation`'s own doc comment for
    /// why. Always called via `recheck_debounced`, never directly.
    ///
    /// Two tiers, deliberately different in how many errors they
    /// report at once — see `parse_every_module_diag`'s own doc comment
    /// for the full reasoning: every FILE that fails to PARSE gets its
    /// own diagnostic, all at once (parsing is fully independent per
    /// file, zero cascade risk); but the moment every file parses
    /// cleanly, module resolution and type-checking fall back to the
    /// single-error `check_modules_diag` exactly as before — a later
    /// function's error can genuinely depend on an earlier one's real,
    /// resolved signature (mutual recursion), so reporting more than
    /// one there risks a misleading cascade, not just extra convenience.
    async fn recheck(&self, my_gen: u64) {
        let Some(overlaid) = self.overlaid_files().await else {
            return;
        };

        let modules: Vec<(&str, &str)> = overlaid.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
        let sources = crate::ModuleSources::new(&modules);

        // Bail before doing the (potentially real) typecheck work at
        // all if a NEWER event already superseded this one — see
        // `generation`'s own doc comment. Checked again below, right
        // before each actual publish, since checking itself is the
        // slow part a newer recheck could easily finish ahead of.
        if self.is_stale(my_gen) {
            return;
        }

        let mut new_diagnostics: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
        match crate::modules::parse_every_module_diag(&modules) {
            Err(errors) => {
                for e in &errors {
                    if let Some((uri, diagnostic)) = self.diagnostic_for(e, &sources, &overlaid).await {
                        new_diagnostics.entry(uri).or_default().push(diagnostic);
                    }
                }
            }
            Ok(()) => match check_modules_diag(&modules) {
                Ok(()) => {
                    // Cache top-level completion items on every
                    // SUCCESSFUL check — see `Backend::last_good_
                    // completions`'s own doc comment for why this needs
                    // to be a cache (rather than computed fresh at
                    // completion-request time) at all: general
                    // completion is most useful mid-edit, exactly when
                    // the CURRENT buffer often doesn't parse.
                    if let Ok(program) = crate::modules::resolve_modules_diag(&modules) {
                        *self.last_good_completions.lock().unwrap() = top_level_completion_items(&program);
                    }
                }
                Err(e) => {
                    if let Some((uri, diagnostic)) = self.diagnostic_for(&e, &sources, &overlaid).await {
                        new_diagnostics.entry(uri).or_default().push(diagnostic);
                    }
                }
            },
        };

        if self.is_stale(my_gen) {
            return;
        }

        let new_diagnosed: std::collections::HashSet<Url> = new_diagnostics.keys().cloned().collect();
        // Each `.lock()` guard is scoped to its own statement here (not
        // held across the `.await`s below) — a `std::sync::Mutex` guard
        // isn't `Send`, so holding one across an await point would
        // make this whole async fn's future non-`Send`, which
        // `tower_lsp`'s trait requires it to be.
        let prev = std::mem::take(&mut *self.last_diagnosed.lock().unwrap());
        for uri in prev.difference(&new_diagnosed) {
            self.client.publish_diagnostics(uri.clone(), Vec::new(), None).await;
        }
        for (uri, diagnostics) in new_diagnostics {
            self.client.publish_diagnostics(uri, diagnostics, None).await;
        }
        *self.last_diagnosed.lock().unwrap() = new_diagnosed;
    }

    /// The shared core of `hover`/`goto_definition`: walks+overlays the
    /// project (same as `recheck`), finds `uri`'s own file within it,
    /// converts `position` into a GLOBAL (merged-module) byte offset,
    /// and type-checks the whole project — returning that offset
    /// alongside the resulting `Infer` (for `resolve_node_types`/
    /// `definitions`) and `ModuleSources` (for mapping a DEFINITION's
    /// span back to a real file+`Range` on the way out). `None` if the
    /// project can't be read, `uri` isn't one of its files, or the
    /// project doesn't currently type-check at all (see this module's
    /// own doc comment on the single-error-at-a-time limitation — a
    /// project with an error anywhere gets no hover/go-to-definition
    /// until it's fixed, matching the exact same scope boundary
    /// diagnostics already has, not a new, separate gap).
    #[allow(clippy::type_complexity)]
    async fn resolve_at(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<(Infer, crate::ModuleSources, u32, Vec<(PathBuf, String, String)>)> {
        let overlaid = self.overlaid_files().await?;
        let target_path = uri.to_file_path().ok()?;
        let idx = overlaid.iter().position(|(path, _, _)| path == &target_path)?;
        let local_offset = position::position_to_byte_offset(&overlaid[idx].2, position);

        let modules: Vec<(&str, &str)> = overlaid.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
        let sources = crate::ModuleSources::new(&modules);
        let global_offset = sources.to_global_offset(idx, local_offset)?;

        let infer = check_modules_diag_with_infer(&modules).ok()?;
        Some((infer, sources, global_offset, overlaid))
    }

    /// Struct-field completion for a `.` immediately before `position`
    /// — `None` (not "no fields," genuinely "not applicable here") when
    /// `position` isn't right after a `.`-prefixed partial/empty field
    /// name at all, so `completion` knows to fall back to general
    /// completion instead.
    ///
    /// The parser has no error recovery (see this module's own doc
    /// comment), so `base.` — or `base.partial_name` with `partial_
    /// name` not (yet) a real field — would normally fail to parse/
    /// type-check AT ALL, taking the whole project's hover/go-to-
    /// definition/completion down with it. Worked around by SPLICING:
    /// replace whatever partial name is being typed (possibly nothing
    /// yet) with a fixed placeholder identifier before handing the
    /// buffer to the ordinary, UNCHANGED parse+typecheck pipeline —
    /// `base.__plum_lsp_completion__` parses exactly like any other
    /// field access, so `Infer::field_owners` resolves it to `base`'s
    /// struct name completely normally, with zero parser changes
    /// anywhere. Always reflects the buffer's CURRENT state (unlike a
    /// last-known-good-snapshot heuristic), at the cost of one extra
    /// parse+typecheck per completion request — the same "reparse the
    /// whole project on demand" cost `resolve_at` itself already pays.
    async fn dot_completion(&self, uri: &Url, position: Position) -> Option<Vec<CompletionItem>> {
        let overlaid = self.overlaid_files().await?;
        let target_path = uri.to_file_path().ok()?;
        let idx = overlaid.iter().position(|(path, _, _)| path == &target_path)?;
        let (path, mpath, src) = &overlaid[idx];
        let local_offset = position::position_to_byte_offset(src, position);

        // Scan backward from the cursor over identifier-continue bytes
        // to find the start of whatever partial name is being typed
        // (zero-length if the cursor is immediately after the `.`
        // itself, the most common completion-trigger moment). Byte-
        // wise, not char-wise, is safe here specifically because every
        // identifier-continue character this loop matches is ASCII
        // (Plum identifiers are ASCII-only — ordinary alphanumeric/
        // underscore — so a multi-byte UTF-8 character never has a
        // continuation byte that could be misread as one).
        let bytes = src.as_bytes();
        let mut name_start = local_offset;
        while name_start > 0 && (bytes[name_start - 1].is_ascii_alphanumeric() || bytes[name_start - 1] == b'_') {
            name_start -= 1;
        }
        if name_start == 0 || bytes[name_start - 1] != b'.' {
            return None;
        }

        const PLACEHOLDER: &str = "__plum_lsp_completion__";
        let mut spliced = String::with_capacity(src.len() - (local_offset - name_start) + PLACEHOLDER.len());
        spliced.push_str(&src[..name_start]);
        spliced.push_str(PLACEHOLDER);
        spliced.push_str(&src[local_offset..]);

        let mut overlaid_spliced = overlaid.clone();
        overlaid_spliced[idx] = (path.clone(), mpath.clone(), spliced);

        let modules: Vec<(&str, &str)> = overlaid_spliced.iter().map(|(_p, m, s)| (m.as_str(), s.as_str())).collect();
        let sources = crate::ModuleSources::new(&modules);
        let field_end_global = sources.to_global_offset(idx, name_start + PLACEHOLDER.len())?;

        // Lenient — the placeholder deliberately names a field that
        // doesn't exist, so the program is EXPECTED not to type-check;
        // see `check_modules_diag_lenient_infer`'s own doc comment for
        // why this is the one place that's the right call.
        let infer = check_modules_diag_lenient_infer(&modules)?;
        // The synthetic `Expr::Field`'s span ENDS exactly at the
        // placeholder's own end — computed directly above, not
        // searched for — so this find is unambiguous even if the
        // placeholder text itself, in principle, appeared elsewhere in
        // the program (it wouldn't share this exact end offset).
        let struct_name = infer.field_owners().iter().find(|(span, _)| span.end == field_end_global).map(|(_, name)| name.clone())?;
        let fields = infer.ctx().struct_fields(&struct_name)?;
        Some(
            fields
                .iter()
                .map(|(name, ty)| CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(render_type(ty)),
                    ..Default::default()
                })
                .collect(),
        )
    }
}

/// The smallest span in `map` that CONTAINS `offset`, if any — "smallest"
/// (by byte width) so a nested expression's own, more specific span wins
/// over an enclosing one that also happens to contain `offset` (e.g.
/// hovering over `x` inside `x + 1` should show `x`'s own type, not the
/// whole binary expression's). Ties broken arbitrarily (by `HashMap`
/// iteration order) — genuinely equal-width containing spans shouldn't
/// arise in practice (two different AST nodes covering the exact same
/// byte range), so this is a pragmatic tie-break, not a load-bearing one.
fn smallest_containing<T>(map: &HashMap<plum_syntax::span::Span, T>, offset: u32) -> Option<(plum_syntax::span::Span, &T)> {
    map.iter()
        .filter(|(span, _)| span.start <= offset && offset <= span.end)
        .min_by_key(|(span, _)| span.end - span.start)
        .map(|(span, v)| (*span, v))
}

/// A readable, Plum-surface-syntax-like rendering of `ty` for hover text
/// — deliberately NOT `{ty:?}` (Rust `Debug`, e.g. `Struct("Point", [])`)
/// the way every internal error message in this codebase renders a type,
/// since hover text is user-facing UI, not a compiler-internal message.
/// `Type::Str`'s surface name is `String` (the internal variant predates
/// that surface renaming); `Var`/`Param` are the only two shapes that
/// can reach here already best-effort-resolved by `resolve_node_types`
/// and still be unresolved (a genuinely never-pinned-down type, or a
/// declaration TEMPLATE's own parameter) — rendered as `?` / the param's
/// own name respectively, rather than leaking `Var(3)`-style internal
/// `Debug` noise into the hover popup.
fn render_type(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::Str => "String".to_string(),
        Type::CStr => "CStr".to_string(),
        Type::Unit => "Unit".to_string(),
        Type::Range => "Range".to_string(),
        Type::Var(_) => "?".to_string(),
        Type::Param(name) => name.clone(),
        Type::Function(params, ret) => {
            let params = params.iter().map(render_type).collect::<Vec<_>>().join(", ");
            format!("({params}) -> {}", render_type(ret))
        }
        Type::Tuple(elems) => {
            let elems = elems.iter().map(render_type).collect::<Vec<_>>().join(", ");
            format!("({elems})")
        }
        Type::Struct(name, args) | Type::Enum(name, args) => {
            if args.is_empty() {
                name.clone()
            } else {
                let args = args.iter().map(render_type).collect::<Vec<_>>().join(", ");
                format!("{name}[{args}]")
            }
        }
    }
}

/// Every reserved word the LEXER recognizes (`lex_ident_or_keyword`'s
/// own match) — including `fn`/`mod`, which the grammar barely uses
/// (`let` is the real function-declaration keyword; `mod` has no
/// parser production consuming it at all yet) but are still real,
/// reserved tokens worth completing, not vestigial noise to filter out.
const KEYWORDS: &[&str] = &[
    "let", "mut", "fn", "struct", "enum", "match", "if", "else", "for", "in", "pub", "use", "mod", "extern",
    "unsafe", "spawn", "select", "true", "false",
];

/// Walks `program`'s own top-level items (which already includes the
/// injected prelude — `resolve_modules_diag` runs `with_prelude` before
/// this ever sees it, so this naturally covers every stdlib function/
/// type too, not just the user's own project) into completable
/// `CompletionItem`s. A zero-param top-level `let` is a global
/// (`CONSTANT`); one WITH params is a function; a `struct`/`enum`
/// declaration contributes its own name plus, for an enum, each
/// variant tag too (`Circle`/`None`-shaped bare references are common
/// enough to be worth suggesting directly, not just the enum's own
/// name); an `extern` block contributes each declared function. A
/// `use` declaration contributes nothing of its own (it names an
/// ALREADY-listed item from elsewhere, not a new declaration).
fn top_level_completion_items(program: &ast::Program) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for item in &program.items {
        match &item.kind {
            ast::ItemKind::Let(def) if def.params.is_empty() => {
                items.push(CompletionItem { label: def.name.clone(), kind: Some(CompletionItemKind::CONSTANT), ..Default::default() });
            }
            ast::ItemKind::Let(def) => {
                items.push(CompletionItem { label: def.name.clone(), kind: Some(CompletionItemKind::FUNCTION), ..Default::default() });
            }
            ast::ItemKind::Struct(decl) => {
                items.push(CompletionItem { label: decl.name.clone(), kind: Some(CompletionItemKind::STRUCT), ..Default::default() });
            }
            ast::ItemKind::Enum(decl) => {
                items.push(CompletionItem { label: decl.name.clone(), kind: Some(CompletionItemKind::ENUM), ..Default::default() });
                for variant in &decl.variants {
                    items.push(CompletionItem { label: variant.name.clone(), kind: Some(CompletionItemKind::ENUM_MEMBER), ..Default::default() });
                }
            }
            ast::ItemKind::Extern(block) => {
                for f in &block.fns {
                    items.push(CompletionItem { label: f.name.clone(), kind: Some(CompletionItemKind::FUNCTION), ..Default::default() });
                }
            }
            ast::ItemKind::Use(_) => {}
        }
    }
    items
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
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..CompletionOptions::default()
                }),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo { name: "plum-lsp".to_string(), version: Some(env!("CARGO_PKG_VERSION").to_string()) }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.recheck_debounced().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.open_docs.lock().unwrap().insert(params.text_document.uri, params.text_document.text);
        self.recheck_debounced().await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        // Full sync only (see this module's doc comment) — the LAST
        // content change event IS the whole new document text.
        if let Some(change) = params.content_changes.pop() {
            self.open_docs.lock().unwrap().insert(params.text_document.uri, change.text);
        }
        self.recheck_debounced().await;
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        self.recheck_debounced().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.open_docs.lock().unwrap().remove(&params.text_document.uri);
        self.recheck_debounced().await;
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let text_doc_pos = params.text_document_position_params;
        let Some((infer, _sources, offset, _overlaid)) = self.resolve_at(&text_doc_pos.text_document.uri, text_doc_pos.position).await else {
            return Ok(None);
        };
        let node_types = infer.resolve_node_types();
        let Some((_span, ty)) = smallest_containing(&node_types, offset) else {
            return Ok(None);
        };
        Ok(Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String(render_type(ty))),
            range: None,
        }))
    }

    async fn goto_definition(&self, params: GotoDefinitionParams) -> RpcResult<Option<GotoDefinitionResponse>> {
        let text_doc_pos = params.text_document_position_params;
        let Some((infer, sources, offset, overlaid)) = self.resolve_at(&text_doc_pos.text_document.uri, text_doc_pos.position).await else {
            return Ok(None);
        };
        let Some((_use_span, &decl_span)) = smallest_containing(infer.definitions(), offset) else {
            return Ok(None);
        };
        let Some((idx, local_start)) = sources.locate_offset(decl_span.start) else {
            return Ok(None);
        };
        // The declaration's END might, in principle, land past this
        // same module's own range (mirrors `recheck`'s identical
        // fallback for a diagnostic's span) — a zero-width range at
        // the start is always safe rather than risking a panic on a
        // `Range` whose end predates its start.
        let Some((path, _mpath, src)) = overlaid.get(idx) else {
            return Ok(None);
        };
        let Ok(uri) = Url::from_file_path(path) else {
            return Ok(None);
        };
        let start_pos = position::byte_offset_to_position(src, local_start);
        let end_pos = sources
            .locate_offset(decl_span.end)
            .filter(|(end_idx, _)| *end_idx == idx)
            .map(|(_, local_end)| position::byte_offset_to_position(src, local_end))
            .unwrap_or(start_pos);
        Ok(Some(GotoDefinitionResponse::Scalar(Location::new(uri, Range::new(start_pos, end_pos)))))
    }

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let text_doc_pos = params.text_document_position;
        if let Some(items) = self.dot_completion(&text_doc_pos.text_document.uri, text_doc_pos.position).await {
            return Ok(Some(CompletionResponse::Array(items)));
        }

        // General completion — see `last_good_completions`'s own doc
        // comment for why this is a CACHE rather than computed fresh
        // here (a mid-edit buffer often doesn't parse at all).
        let mut items = self.last_good_completions.lock().unwrap().clone();
        items.extend(
            KEYWORDS
                .iter()
                .map(|kw| CompletionItem { label: kw.to_string(), kind: Some(CompletionItemKind::KEYWORD), ..Default::default() }),
        );
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod generation_tests {
    use super::Generation;

    #[test]
    fn a_fresh_generations_first_bump_is_current() {
        let g = Generation::default();
        let tag = g.bump();
        assert!(g.is_current(tag));
    }

    #[test]
    fn an_older_bumps_tag_stops_being_current_once_a_newer_one_happens() {
        // The exact property `recheck`'s staleness guard depends on:
        // once a NEWER event has bumped the counter, an OLDER, still
        // in-flight recheck's own tag must read back as stale so it
        // abandons rather than publishing over the newer one's result.
        let g = Generation::default();
        let older = g.bump();
        let newer = g.bump();
        assert_ne!(older, newer);
        assert!(!g.is_current(older));
        assert!(g.is_current(newer));
    }

    #[test]
    fn many_interleaved_bumps_from_multiple_threads_leave_exactly_the_last_one_current() {
        // Not just "two calls in sequence" — real overlapping recheck
        // calls run on genuinely different tokio tasks, so this proves
        // the SAME property holds under real concurrent access, not
        // just single-threaded reasoning about it.
        use std::sync::Arc;
        let g = Arc::new(Generation::default());
        let handles: Vec<_> = (0..50)
            .map(|_| {
                let g = Arc::clone(&g);
                std::thread::spawn(move || g.bump())
            })
            .collect();
        let mut tags: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        tags.sort_unstable();
        // 50 threads, each incrementing by exactly 1 from a shared
        // starting point of 0 — the tags handed out must be exactly
        // 1..=50, no duplicates and no gaps, regardless of scheduling
        // order (proves `fetch_add` is doing real, atomic, race-free
        // work here, not just "usually fine").
        assert_eq!(tags, (1..=50).collect::<Vec<u64>>());
        // And only the LARGEST tag handed out is still current — every
        // other thread's own tag, no matter when it finished relative
        // to the others, correctly reads back as stale.
        let max = *tags.last().unwrap();
        for &tag in &tags {
            assert_eq!(g.is_current(tag), tag == max, "tag {tag} (max {max}) had the wrong is_current result");
        }
    }
}

#[cfg(test)]
mod integration_tests {
    // A real, end-to-end exercise of `hover`/`goto_definition` — not
    // just the helper functions they're built from (`resolve_node_
    // types`/`definitions`/`position_to_byte_offset`/... are already
    // covered directly, in `plum-types` and `position.rs`'s own test
    // modules). These go through the REAL `LanguageServer` trait
    // methods `plum lsp`'s client actually calls, against a REAL
    // temp-directory project on disk, proving the whole path — file
    // walk, module merge, type-check, span lookup, byte-offset-to-
    // `Position` conversion — works together, not just each piece in
    // isolation.
    //
    // `Backend` needs a real `tower_lsp::Client` to construct, and
    // `Client` has no public standalone constructor — only `LspService::
    // new`'s init closure ever receives one. `Client` derives `Clone`,
    // so this captures one out of that closure rather than driving a
    // real JSON-RPC transport (`service`/`socket`) just to get a handle
    // — the trait methods below are called DIRECTLY (bypassing JSON-RPC
    // serialization entirely), which is fine: nothing under test here
    // depends on the transport, only on `Backend`'s own logic. `_client`
    // itself is never used by anything `hover`/`goto_definition`
    // exercise (`self.client` is only touched by `recheck`'s error/
    // diagnostic-publishing paths, not by these two handlers or by a
    // SUCCESSFUL `overlaid_files`), so callbacks to it never fire on a
    // well-formed project like this module's own tests use.
    use super::*;
    use tower_lsp::LanguageServer;

    fn make_backend() -> Backend {
        let mut captured: Option<Client> = None;
        let (_service, _socket) = LspService::new(|client| {
            captured = Some(client.clone());
            Backend::new(client)
        });
        Backend::new(captured.expect("LspService::new always calls its init closure exactly once"))
    }

    /// Writes `content` to `dir/filename`, `initialize`s+`did_open`s
    /// `backend` against it, and returns the file's own `Url`.
    async fn open_project(backend: &Backend, dir: &std::path::Path, filename: &str, content: &str) -> Url {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
        let root_uri = Url::from_directory_path(dir).unwrap();
        backend
            .initialize(InitializeParams { root_uri: Some(root_uri), ..Default::default() })
            .await
            .unwrap();
        let file_uri = Url::from_file_path(dir.join(filename)).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: file_uri.clone(),
                    language_id: "plum".to_string(),
                    version: 1,
                    text: content.to_string(),
                },
            })
            .await;
        file_uri
    }

    #[tokio::test]
    async fn recheck_reports_a_diagnostic_for_each_broken_file_at_once() {
        // The actual new behavior this session's ask was about: TWO
        // separately-broken files (each a genuine parse error) must
        // BOTH get a diagnostic from one recheck, not just the first
        // one found — `last_diagnosed` (a private field, readable here
        // since this test module is nested inside the same crate) is
        // inspected DIRECTLY rather than trying to intercept published
        // notifications over a transport nothing in this harness
        // actually drives (see `make_backend`'s own doc comment on why
        // `_socket` is discarded).
        let dir = std::env::temp_dir().join(format!("plum-lsp-multifile-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.plum"), "let x = (").unwrap();
        std::fs::write(dir.join("b.plum"), "let y = 2").unwrap();
        std::fs::write(dir.join("c.plum"), "let z = (").unwrap();

        let backend = make_backend();
        let root_uri = Url::from_directory_path(&dir).unwrap();
        backend.initialize(InitializeParams { root_uri: Some(root_uri), ..Default::default() }).await.unwrap();
        backend.initialized(InitializedParams {}).await;

        let a_uri = Url::from_file_path(dir.join("a.plum")).unwrap();
        let b_uri = Url::from_file_path(dir.join("b.plum")).unwrap();
        let c_uri = Url::from_file_path(dir.join("c.plum")).unwrap();
        let diagnosed = backend.last_diagnosed.lock().unwrap().clone();
        assert_eq!(diagnosed.len(), 2, "expected exactly a.plum and c.plum to have a diagnostic, got: {diagnosed:?}");
        assert!(diagnosed.contains(&a_uri), "expected a.plum to be diagnosed: {diagnosed:?}");
        assert!(diagnosed.contains(&c_uri), "expected c.plum to be diagnosed: {diagnosed:?}");
        assert!(!diagnosed.contains(&b_uri), "b.plum parses fine, expected no diagnostic for it: {diagnosed:?}");

        // Fix `a` — its own diagnostic must clear while `c`'s (still
        // broken) stays, proving the diff-based clear logic works for
        // the now-multi-entry `last_diagnosed`, not just the old
        // single-`Option<Url>` case.
        backend
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri: a_uri.clone(), version: 2 },
                content_changes: vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: "let x = 1".to_string() }],
            })
            .await;
        let diagnosed_after_fix = backend.last_diagnosed.lock().unwrap().clone();
        assert_eq!(diagnosed_after_fix.len(), 1, "expected only c.plum left diagnosed, got: {diagnosed_after_fix:?}");
        assert!(diagnosed_after_fix.contains(&c_uri));
        assert!(!diagnosed_after_fix.contains(&a_uri), "expected a.plum's diagnostic to have cleared after the fix");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn hover_reports_a_local_variables_resolved_type() {
        let dir = std::env::temp_dir().join(format!("plum-lsp-hover-test-{}", std::process::id()));
        let backend = make_backend();
        let src = "let go (): Int = { let x = 5; x + 1 }";
        let uri = open_project(&backend, &dir, "main.plum", src).await;

        // The SECOND `x` — the use inside `x + 1`, not the binding one.
        let use_start = src.rfind('x').unwrap();
        let pos = position::byte_offset_to_position(src, use_start);
        let result = backend
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()
            .expect("expected a hover result");
        assert_eq!(result.contents, HoverContents::Scalar(MarkedString::String("Int".to_string())));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn goto_definition_jumps_from_a_function_call_to_its_declaration() {
        let dir = std::env::temp_dir().join(format!("plum-lsp-gotodef-test-{}", std::process::id()));
        let backend = make_backend();
        let src = "let add_one (x: Int): Int = x + 1\nlet go (): Int = add_one(41)";
        let uri = open_project(&backend, &dir, "main.plum", src).await;

        let call_start = src.rfind("add_one(41)").unwrap();
        let pos = position::byte_offset_to_position(src, call_start);
        let result = backend
            .goto_definition(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap()
            .expect("expected a go-to-definition result");
        let GotoDefinitionResponse::Scalar(loc) = result else {
            panic!("expected a scalar (single-location) response, got: {result:?}");
        };
        assert_eq!(loc.uri, uri);
        // `def.span` covers the WHOLE `let add_one (x: Int): Int = x +
        // 1` definition (see `plum-types`'s own regression test on this
        // exact point) — which starts at the very top of the file.
        assert_eq!(loc.range.start, Position::new(0, 0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn hover_and_goto_definition_both_return_none_rather_than_erroring_when_nothing_resolves() {
        // A position that lands on WHITESPACE (no expression node, no
        // reference) — neither handler should treat this as a failure,
        // just "nothing here."
        let dir = std::env::temp_dir().join(format!("plum-lsp-none-test-{}", std::process::id()));
        let backend = make_backend();
        let src = "let go (): Int = 1 + 1";
        let uri = open_project(&backend, &dir, "main.plum", src).await;

        let pos = Position::new(0, 0); // the very start of `let` — not an expression
        let hover_result = backend
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(hover_result, None);

        let def_result = backend
            .goto_definition(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(def_result, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    fn completion_params(uri: Url, position: Position) -> CompletionParams {
        CompletionParams {
            text_document_position: TextDocumentPositionParams { text_document: TextDocumentIdentifier { uri }, position },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }
    }

    fn completion_labels(response: CompletionResponse) -> Vec<String> {
        let CompletionResponse::Array(items) = response else {
            panic!("expected an Array completion response, got: {response:?}");
        };
        items.into_iter().map(|item| item.label).collect()
    }

    #[tokio::test]
    async fn general_completion_offers_keywords_and_top_level_names() {
        let dir = std::env::temp_dir().join(format!("plum-lsp-completion-general-test-{}", std::process::id()));
        let backend = make_backend();
        let src = "struct Point { x: Int, y: Int }\nlet add_one (x: Int): Int = x + 1\nlet go (): Int = add_one(1)";
        let uri = open_project(&backend, &dir, "main.plum", src).await;

        // Any position not immediately after a `.` — end of the file.
        let pos = position::byte_offset_to_position(src, src.len());
        let response = backend.completion(completion_params(uri, pos)).await.unwrap().expect("expected completion items");
        let labels = completion_labels(response);
        assert!(labels.contains(&"let".to_string()), "expected the `let` keyword, got: {labels:?}");
        assert!(labels.contains(&"struct".to_string()), "expected the `struct` keyword, got: {labels:?}");
        assert!(labels.contains(&"Point".to_string()), "expected the struct name Point, got: {labels:?}");
        assert!(labels.contains(&"add_one".to_string()), "expected the function name add_one, got: {labels:?}");
        assert!(labels.contains(&"go".to_string()), "expected the function name go, got: {labels:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn general_completion_falls_back_to_the_last_good_snapshot_when_the_current_buffer_is_broken() {
        // First open a WELL-TYPED buffer so a snapshot gets cached...
        let dir = std::env::temp_dir().join(format!("plum-lsp-completion-stale-test-{}", std::process::id()));
        let backend = make_backend();
        let good_src = "struct Point { x: Int, y: Int }\nlet go (): Int = 1";
        let uri = open_project(&backend, &dir, "main.plum", good_src).await;

        // ...then push a BROKEN edit (unterminated struct literal — a
        // real parse error) through `did_change`, matching a realistic
        // mid-typing moment. The general-completion path must still
        // offer `Point` from the cached snapshot, not silently go empty
        // just because the CURRENT buffer can't be checked at all.
        let broken_src = "struct Point { x: Int, y: Int }\nlet go (): Int = { let p = Point {";
        backend
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
                content_changes: vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: broken_src.to_string() }],
            })
            .await;

        let pos = position::byte_offset_to_position(broken_src, broken_src.len());
        let response = backend.completion(completion_params(uri, pos)).await.unwrap().expect("expected completion items");
        let labels = completion_labels(response);
        assert!(labels.contains(&"Point".to_string()), "expected the cached struct name Point despite the broken buffer, got: {labels:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dot_completion_lists_a_structs_fields_right_after_the_dot() {
        let dir = std::env::temp_dir().join(format!("plum-lsp-completion-dot-empty-test-{}", std::process::id()));
        let backend = make_backend();
        // Deliberately ends in a bare trailing `.` — exactly the parse-
        // error-inducing shape the splice trick exists for.
        let src = "struct Point { x: Int, y: Int }\nlet go (p: Point): Int = p.";
        let uri = open_project(&backend, &dir, "main.plum", src).await;

        let pos = position::byte_offset_to_position(src, src.len());
        let response = backend.completion(completion_params(uri, pos)).await.unwrap().expect("expected completion items");
        let labels = completion_labels(response);
        assert_eq!(labels.len(), 2, "expected exactly Point's 2 fields, got: {labels:?}");
        assert!(labels.contains(&"x".to_string()));
        assert!(labels.contains(&"y".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dot_completion_still_works_mid_typing_a_partial_field_name() {
        // `p.x` isn't a parse error on its own — but IS a bare use of
        // this handler's splice path too (`x` gets replaced by the
        // placeholder just the same as an empty partial name would),
        // proving the mechanism doesn't depend on the trailing dot
        // being freshly typed.
        let dir = std::env::temp_dir().join(format!("plum-lsp-completion-dot-partial-test-{}", std::process::id()));
        let backend = make_backend();
        let src = "struct Point { x: Int, y: Int }\nlet go (p: Point): Int = p.x";
        let uri = open_project(&backend, &dir, "main.plum", src).await;

        let pos = position::byte_offset_to_position(src, src.len()); // right after the `x`
        let response = backend.completion(completion_params(uri, pos)).await.unwrap().expect("expected completion items");
        let labels = completion_labels(response);
        assert!(labels.contains(&"x".to_string()));
        assert!(labels.contains(&"y".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
