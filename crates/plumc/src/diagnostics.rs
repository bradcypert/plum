//! Turns a `plum_syntax::error::CompileError`'s optional `Span` back
//! into a human-readable `module:line:col` plus a source snippet with
//! a caret — the whole point of threading `CompileError` through the
//! front end in the first place (see `plum_syntax::error`'s own doc
//! comment) instead of baking `Span`'s `Debug` output directly into
//! error text.
//!
//! **Scope note, deliberate**: locations are reported by MODULE PATH
//! (`"shapes"`, `"<root>"` for the top-level module), not by exact
//! source FILE — `resolve_modules`'s public `&[(&str, &str)]` shape
//! (module_path, source) is used directly by many existing tests with
//! hand-written module names, so threading real per-file paths all the
//! way through would mean changing that widely-used signature. In the
//! common case (one file per module/directory, true of every example
//! in this codebase), this is already exact file-level precision; for
//! a directory with multiple files sharing one module, the error is
//! correctly attributed to the MODULE, just not narrowed to which of
//! its files. Still a large, honest improvement over a raw byte offset.

use plum_syntax::error::CompileError;

/// Rebuilds the SAME cumulative-base-offset scheme `modules::
/// resolve_modules_diag` uses internally (`base` starts at `crate::
/// PRELUDE_TOTAL_LEN`, advances by each module's own source length, in
/// the SAME order) — the two must stay in lockstep; `resolve_modules_
/// diag`'s own doc comment cross-references this.
pub struct ModuleSources {
    // (module_path, base_offset, source), in the same order as the
    // `modules` slice it was built from.
    entries: Vec<(String, usize, String)>,
}

impl ModuleSources {
    pub fn new(modules: &[(&str, &str)]) -> Self {
        let mut entries = Vec::with_capacity(modules.len());
        let mut base = crate::PRELUDE_TOTAL_LEN;
        for (mpath, src) in modules {
            entries.push((mpath.to_string(), base, src.to_string()));
            base += src.len();
        }
        ModuleSources { entries }
    }

    /// `None` when `offset` falls inside the prelude itself (before
    /// every module's own range starts) or otherwise doesn't land
    /// inside any known module — the caller falls back to message-only
    /// rendering, never panics on a bad offset.
    fn locate(&self, offset: u32) -> Option<(&str, usize, usize, &str)> {
        let (idx, local) = self.locate_offset(offset)?;
        let (mpath, _base, src) = &self.entries[idx];
        let (line, col) = line_col(src, local);
        Some((mpath, line, col, src))
    }

    /// The `plum lsp` sibling of `locate`: returns the INDEX into the
    /// original `modules` slice `Self::new` was built from (so a caller
    /// holding that same slice — or a parallel per-file table built in
    /// the same order, see `project::collect_project_files_with_paths`
    /// — can recover which exact file an offset landed in) plus the
    /// module-local byte offset, instead of an already-rendered
    /// (CHARACTER-counted) `(line, col)`. `plum lsp` needs UTF-16
    /// CODE-UNIT columns (the LSP spec's `Position` encoding), which is
    /// a different count for any non-ASCII source text — computed by
    /// `lsp::position` from this raw offset, not by `line_col` here.
    ///
    /// `None` under the same conditions as `locate`.
    ///
    /// Upper bound is INCLUSIVE (`offset <= base + src.len()`, not
    /// `<`) — a span's `end` is a one-past-the-last-byte offset (see
    /// `Span::to`), so a span covering all the way to end-of-file
    /// legitimately has `end == src.len()`, which is common for an
    /// in-memory editor buffer with no trailing newline. An exclusive
    /// check here would silently drop that span to the "unlocatable"
    /// fallback for that content only — found by hand while smoke-
    /// testing `plum lsp` against a real editor buffer, where it showed
    /// up as a diagnostic's range randomly collapsing to zero-width
    /// depending on whether the buffer happened to end in a newline.
    pub(crate) fn locate_offset(&self, offset: u32) -> Option<(usize, usize)> {
        let offset = offset as usize;
        self.entries
            .iter()
            .position(|(_, base, src)| offset >= *base && offset <= base + src.len())
            .map(|idx| (idx, offset - self.entries[idx].1))
    }

    /// Full `error: {msg}\n  --> {module}:{line}:{col}\n   |\n {line} | {source line}\n   | {caret}`
    /// rendering, falling back to `error: {msg}` alone when `err.span`
    /// is `None` (a genuinely spanless error — codegen/interpreter
    /// runtime/monomorphize, see `CompileError`'s own doc comment) or
    /// unlocatable (e.g. a bug inside the prelude itself).
    pub fn render(&self, err: &CompileError) -> String {
        let Some(span) = err.span else {
            return format!("error: {}", err.message);
        };
        let Some((mpath, line, col, src)) = self.locate(span.start) else {
            return format!("error: {}", err.message);
        };
        let display_name = if mpath.is_empty() { "<root>" } else { mpath };
        let source_line = src.lines().nth(line - 1).unwrap_or("");
        // The caret underlines from `col` up to the END OF THAT LINE if
        // the span extends past it (a span whose end lands on a
        // DIFFERENT line — rare, e.g. an unterminated construct — gets
        // just a single-character caret at the start position instead
        // of a multi-line snippet; a deliberate v1 simplification).
        let span_char_len = (span.end - span.start) as usize;
        let caret_len = span_char_len.clamp(1, source_line.chars().count().saturating_sub(col - 1).max(1));
        let gutter = format!("{line}");
        let pad = " ".repeat(gutter.len());
        format!(
            "error: {msg}\n  --> {display_name}:{line}:{col}\n{pad} |\n{gutter} | {source_line}\n{pad} | {caret_pad}{caret}",
            msg = err.message,
            caret_pad = " ".repeat(col - 1),
            caret = "^".repeat(caret_len),
        )
    }
}

/// Byte offset (within a single module's own source) -> (1-based line,
/// 1-based column). Column is counted in CHARACTERS, not bytes — matters
/// for any non-ASCII source text (matches how a human reads columns,
/// same approach rustc itself takes).
fn line_col(src: &str, byte_offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in src.char_indices() {
        if i >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::span::Span;

    #[test]
    fn locates_a_span_in_the_second_module() {
        let modules: &[(&str, &str)] = &[("a", "let x = 1"), ("b", "let y = 2")];
        let sources = ModuleSources::new(modules);
        let b_start = (crate::PRELUDE_TOTAL_LEN + "let x = 1".len()) as u32;
        let err = CompileError::new(Span::new(b_start + 4, b_start + 5), "boom");
        let rendered = sources.render(&err);
        assert!(rendered.contains("b:1:5"), "{rendered}");
        assert!(rendered.contains("let y = 2"), "{rendered}");
        assert!(rendered.contains('^'), "{rendered}");
    }

    #[test]
    fn a_spanless_error_renders_message_only() {
        let sources = ModuleSources::new(&[("a", "let x = 1")]);
        let err = CompileError::spanless("boom");
        assert_eq!(sources.render(&err), "error: boom");
    }

    #[test]
    fn a_span_inside_the_prelude_falls_back_to_message_only() {
        let sources = ModuleSources::new(&[("a", "let x = 1")]);
        let err = CompileError::new(Span::new(0, 1), "boom");
        assert_eq!(sources.render(&err), "error: boom");
    }

    #[test]
    fn line_and_column_are_counted_correctly_across_multiple_lines() {
        assert_eq!(line_col("abc\ndef\nghi", 0), (1, 1));
        assert_eq!(line_col("abc\ndef\nghi", 4), (2, 1));
        assert_eq!(line_col("abc\ndef\nghi", 9), (3, 2));
    }
}
