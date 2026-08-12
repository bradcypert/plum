//! Type-checks a program WITHOUT running anything — the front half of
//! `codegen_cli::compile_program_to_ir_diag`, stopped right after
//! move-checking (before any codegen-only step: monomorphization
//! planning, tag derivation, channel registration — none of which
//! affects whether the program is well-typed).
//!
//! Built for `plum lsp`'s diagnostics: unlike `typecheck_and_run_
//! project_diag`/`compile_and_run`, this never calls a function or
//! produces a value, so it's safe to run on ANY open project on every
//! keystroke, regardless of whether it even defines `main`, and
//! regardless of what side effects that `main` might have if it were
//! actually invoked.

use crate::modules::resolve_modules_diag;
use plum_syntax::ast;
use plum_syntax::error::CompileError;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;

/// Type-checks an already-parsed+resolved `Program`. See this module's
/// own doc comment for why this stops where it does.
pub(crate) fn check_program_diag(program: &ast::Program) -> Result<(), CompileError> {
    check_program_diag_with_infer(program).map(|_| ())
}

/// The same pipeline as `check_program_diag`, but hands back the
/// `Infer` instance itself on success instead of discarding it — needed
/// by `plum lsp`'s hover/go-to-definition handlers, which read `Infer::
/// resolve_node_types`/`Infer::definitions` off of it (see those
/// methods' own doc comments in `plum-types`). `check_program_diag`
/// itself is now a thin wrapper around this — one real implementation,
/// not two that could drift apart.
pub(crate) fn check_program_diag_with_infer(program: &ast::Program) -> Result<Infer, CompileError> {
    // Cloned for the same reason `compile_program_to_ir_diag` clones —
    // `resolve_associated_calls` rewrites `_.method` sugar in place,
    // and this fn's callers only ever hand over a `&Program` they still
    // need afterward (or don't own at all, e.g. a fresh one built
    // per-keystroke).
    let mut program = program.clone();
    // Built BEFORE `resolve_associated_calls` (unlike this fn's own
    // comment above once suggested) — `nested_struct_update` needs it
    // for struct field-name lookups, and it only ever reads top-level
    // declarations, never expression bodies, so running it before
    // `assoc_fns`'s call-site rewrites changes nothing it looks at. See
    // `nested_struct_update`'s own doc comment for the full ordering
    // story.
    let type_ctx = TypeContext::from_items(&program.items).map_err(|e: CompileError| e.context("type error"))?;
    crate::nested_struct_update::expand_nested_field_updates(&mut program, &type_ctx).map_err(|e| e.context("type error"))?;
    crate::assoc_fns::resolve_associated_calls(&mut program);
    let program = &program;

    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(program).map_err(|e: CompileError| e.context("type error"))?;
    plum_ir::movecheck::check_moves(program).map_err(|e: CompileError| e.context("move error"))?;
    Ok(infer)
}

/// Parses+resolves `modules` (same `&[(module_path, source)]` shape
/// `resolve_modules_diag` takes) and type-checks the result. This is
/// `plum lsp`'s single entry point: parse errors, resolution errors,
/// and type errors all surface the same way (`Err(CompileError)`,
/// span-carrying whenever the underlying error has one), which is all
/// a `textDocument/publishDiagnostics` notification needs.
pub(crate) fn check_modules_diag(modules: &[(&str, &str)]) -> Result<(), CompileError> {
    let program = resolve_modules_diag(modules)?;
    check_program_diag(&program)
}

/// The hover/go-to-definition sibling of `check_modules_diag` — same
/// input shape, but returns the `Infer` instance on success instead of
/// `()`. Spans inside the returned `Infer`'s `resolve_node_types`/
/// `definitions` are in the SAME merged-module coordinate space
/// `check_modules_diag`'s own `CompileError::span` already is — see
/// `ModuleSources::locate_offset` for converting one back to a specific
/// module + local offset.
pub(crate) fn check_modules_diag_with_infer(modules: &[(&str, &str)]) -> Result<Infer, CompileError> {
    let program = resolve_modules_diag(modules)?;
    check_program_diag_with_infer(&program)
}

/// The completion-probe sibling of `check_modules_diag_with_infer` —
/// same input, but NEVER propagates `Infer::infer_program`'s own
/// failure; it always hands back the `Infer` instance regardless of
/// whether the program actually type-checked, carrying whatever got
/// recorded into its side-channels (`field_owners`, `node_types`, ...)
/// before inference gave up. `None` only if something BEFORE inference
/// even starts fails (parsing, module resolution, `TypeContext::from_
/// items`) — there's no `Infer` to speak of at all in that case.
///
/// Exists for exactly one caller: `plum lsp`'s dot-completion handler,
/// which deliberately probes with a field name that ISN'T real yet
/// (the user hasn't finished typing it) — the resulting "unknown
/// field" error is EXPECTED, not a reason to discard everything
/// learned before it fired (specifically, which struct the base
/// expression resolved to — see `Infer::field_owners`'s own doc
/// comment on why that's recorded before the field-name check, not
/// after). Every OTHER LSP handler (diagnostics, hover, go-to-
/// definition, general completion) still uses the strict, error-
/// propagating `check_modules_diag_with_infer` — answering with a
/// WRONG type/location because the program doesn't actually type-check
/// would be worse than answering nothing, and only this one narrow
/// probe has a reason to accept a knowingly-incomplete result.
pub(crate) fn check_modules_diag_lenient_infer(modules: &[(&str, &str)]) -> Option<Infer> {
    let mut program = resolve_modules_diag(modules).ok()?;
    let type_ctx = TypeContext::from_items(&program.items).ok()?;
    crate::nested_struct_update::expand_nested_field_updates(&mut program, &type_ctx).ok()?;
    crate::assoc_fns::resolve_associated_calls(&mut program);

    let mut infer = Infer::with_context(type_ctx);
    // Deliberately ignored — see this function's own doc comment.
    let _ = infer.infer_program(&program);
    Some(infer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_typed_program_checks_clean() {
        let modules: &[(&str, &str)] = &[("", "let main unused = 1 + 2")];
        assert_eq!(check_modules_diag(modules), Ok(()));
    }

    #[test]
    fn a_type_error_is_reported() {
        let modules: &[(&str, &str)] = &[("", "let main unused = 1 + \"nope\"")];
        let result = check_modules_diag(modules);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn a_parse_error_is_reported_too() {
        let modules: &[(&str, &str)] = &[("", "let main unused = (")];
        assert!(check_modules_diag(modules).is_err());
    }

    #[test]
    fn lenient_infer_still_returns_something_for_a_program_with_a_real_type_error() {
        // The whole point of the lenient variant: `check_modules_diag`
        // itself correctly rejects this (a real type error, `main`
        // returns a Bool where an Int/whatever else is expected — not
        // asserted precisely here, just that it's an error), but the
        // lenient sibling must still hand back a real `Infer`, not
        // `None`, so a caller (`plum lsp`'s dot-completion) can dig
        // into whatever DID get resolved before the error fired.
        let modules: &[(&str, &str)] = &[("", "struct Point { x: Int, y: Int }\nlet main (p: Point): Int = p.z")];
        assert!(check_modules_diag(modules).is_err(), "expected p.z (no field z) to be a real type error");
        let infer = check_modules_diag_lenient_infer(modules).expect("expected Some(Infer) even though the program doesn't type-check");
        // Not asserting the TOTAL count — `resolve_modules_diag`
        // injects the whole prelude, which has plenty of its OWN field
        // accesses recorded too; just that `p`'s owning struct made it
        // in among them despite the error.
        assert!(
            infer.field_owners().values().any(|s| s == "Point"),
            "expected p's owning struct Point to have been recorded before the error, got: {:?}",
            infer.field_owners().values().collect::<Vec<_>>()
        );
    }

    #[test]
    fn lenient_infer_returns_none_for_a_genuine_parse_error() {
        // Unlike a type error (recoverable-enough to still learn
        // something from), a program that doesn't even PARSE has no
        // `Infer` to speak of at all — `TypeContext::from_items` is
        // never reached, let alone `Infer::with_context`.
        let modules: &[(&str, &str)] = &[("", "let main unused = (")];
        assert!(check_modules_diag_lenient_infer(modules).is_none());
    }
}
