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
    Ok(())
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
}
