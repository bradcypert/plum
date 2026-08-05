use crate::span::Span;

/// A compile-time error carrying an OPTIONAL source location, threaded
/// through the front end (lexer/parser, `plum-types` inference, `plum-
/// ir` lowering/move-checking) in place of the plain `String` these
/// stages used to return with a `Span`'s `Debug` output baked directly
/// into the message text (`format!("... at {span:?}")`). Keeping the
/// span as structured data lets a caller with access to the original
/// source text (the CLI) translate it into a real `file:line:col` +
/// snippet; `Display` alone (used by every pre-existing `.to_string()`/
/// `.contains(...)` test) deliberately does NOT embed any span text, so
/// those checks keep working unchanged.
///
/// `span: None` is a real, honest case, not a placeholder — `plum-ir`'s
/// lowered IR is deliberately span-free (see `ir.rs`'s own doc
/// comment), so every error from `plum-codegen`, `plum-interp`'s
/// runtime, and most of `plum-ir::monomorphize` has no location to
/// report at all. Those print message-only, same as before this type
/// existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub span: Option<Span>,
    pub message: String,
}

impl CompileError {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        CompileError { span: Some(span), message: message.into() }
    }

    pub fn spanless(message: impl Into<String>) -> Self {
        CompileError { span: None, message: message.into() }
    }

    /// Prefixes `self`'s message with `prefix` while PRESERVING
    /// whatever span it already had — used when a caller adds context
    /// to an inner error (`"function parameter: {inner}"`) without
    /// wanting to throw away a real location the inner error already
    /// pinned down. Contrast with building a brand-new `CompileError`
    /// via `format!("...: {e}")` + `.into()`, which always loses the
    /// inner span (message text alone has no structure left to recover
    /// it from).
    pub fn context(self, prefix: impl std::fmt::Display) -> Self {
        CompileError { span: self.span, message: format!("{prefix}: {}", self.message) }
    }

    /// Backfills `span` onto `self` if it doesn't already have one —
    /// used at a call site that has a reasonable fallback location
    /// (e.g. the enclosing expression's own span) for an inner error
    /// that couldn't attach anything more precise itself (e.g. `unify`,
    /// which has no span of its own at all). A no-op if `self` already
    /// has a (more precise) span.
    pub fn or_span(self, span: Span) -> Self {
        if self.span.is_some() {
            self
        } else {
            CompileError { span: Some(span), message: self.message }
        }
    }

    /// Passthrough to `self.message.contains(..)` — lets the many
    /// existing `err.contains("...")` test assertions across this
    /// workspace (written back when every error was a plain `String`)
    /// keep working unchanged against a `CompileError` directly,
    /// without needing every call site to change to `err.message.
    /// contains(..)`/`err.to_string().contains(..)`.
    pub fn contains(&self, pat: &str) -> bool {
        self.message.contains(pat)
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CompileError {}

// Deliberately blanket, not per-call-site: this is what lets a function
// whose return type changes from `Result<_, String>` to `Result<_,
// CompileError>` keep `?`-propagating an untouched `Result<_, String>`
// call with ZERO edits at that call site — `?` invokes `From`
// automatically. Only call sites that actually construct a span-
// bearing error need to change, at every OTHER site the conversion is
// free.
impl From<String> for CompileError {
    fn from(message: String) -> Self {
        CompileError::spanless(message)
    }
}

impl From<&str> for CompileError {
    fn from(message: &str) -> Self {
        CompileError::spanless(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shows_only_the_message_never_the_span() {
        let e = CompileError::new(Span::new(3, 7), "boom");
        assert_eq!(e.to_string(), "boom");
    }

    #[test]
    fn spanless_has_no_span() {
        let e = CompileError::spanless("boom");
        assert_eq!(e.span, None);
        assert_eq!(e.to_string(), "boom");
    }

    #[test]
    fn a_string_error_auto_converts_via_question_mark() {
        fn inner() -> Result<(), String> {
            Err("inner failure".to_string())
        }
        fn outer() -> Result<(), CompileError> {
            inner()?;
            Ok(())
        }
        let err = outer().unwrap_err();
        assert_eq!(err.span, None);
        assert_eq!(err.message, "inner failure");
    }
}
