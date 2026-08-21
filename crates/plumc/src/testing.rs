//! `plum test` — discovery and both execution modes. See DESIGN.md's
//! "Testing framework" section for the full design (why native tests
//! need process-level isolation while the interpreter doesn't, why
//! discovery is a naming convention rather than an annotation, ...).

use plum_interp::{Interpreter, Value};
use plum_ir::fbip::optimize_program;
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::ast;
use plum_syntax::error::CompileError;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;

/// One test function's outcome — `Ok(())` on pass, `Err(message)` on
/// fail (an assertion failure via `panic_raw`, or any other ordinary
/// runtime error — treated identically, matching how a real test
/// runner treats "the test function errored" as failure regardless of
/// *why*, not just assertion failures specifically).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutcome {
    pub name: String,
    pub result: Result<(), String>,
}

/// Every top-level `let` in `program` (already resolved and fully
/// QUALIFIED — see `modules::resolve_modules_diag`'s own doc comment;
/// a non-root-module test like `shapes/foo.plum`'s `test_bar` arrives
/// here already renamed to `"shapes.test_bar"`) whose LAST dot-segment
/// starts with `test_`. No `pub` requirement: the CLI calls a
/// discovered test by its fully-qualified name directly, the same way
/// `main` itself is invoked — bypassing ordinary `use`-based
/// visibility checking entirely (confirmed by `modules.rs`'s own test
/// `root_module_declarations_are_visible_from_any_module_without_use`,
/// which already calls a non-root function by qualified name this same
/// way). Order matches declaration order in the merged program, which
/// is deterministic (`project::collect_plum_files` sorts by file path).
pub fn discover_tests(program: &ast::Program) -> Vec<String> {
    program
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ast::ItemKind::Let(def) => {
                let last_segment = def.name.rsplit('.').next().unwrap_or(&def.name);
                last_segment.starts_with("test_").then(|| def.name.clone())
            }
            _ => None,
        })
        .collect()
}

/// Runs every test in `names` through ONE loaded interpreter — type-
/// checks/move-checks/lowers/optimizes `program` exactly once (the
/// same front half `plumc::run_resolved_program_diag` runs for an
/// ordinary single-entry-point program), then calls each test in turn
/// via `Interpreter::call`, collecting its own `Result` independently.
/// Cheap and simple because the interpreter's own runtime errors are
/// ALREADY ordinary, catchable `Result`s (unlike a native-compiled
/// program's hard-abort-on-failure — see `run_tests_native`'s own doc
/// comment) — one failing test can never prevent any other test from
/// running, no process isolation needed at all.
///
/// The OUTER `Result` is for a project-level failure (a real syntax/
/// type error, unrelated to any individual test) — rendered through
/// `ModuleSources` by the caller, exactly like every other command.
/// The per-test `Result` inside each `TestOutcome` is never itself
/// propagated as the outer error — a failing assertion is a normal,
/// expected outcome of running tests, not a reason to abort the whole
/// `plum test` invocation.
pub fn run_tests_interpreted(program: ast::Program, names: &[String]) -> Result<Vec<TestOutcome>, CompileError> {
    let mut program = program;
    // The SAME front end, in the SAME order, as
    // `run_resolved_program_with_process_args_diag`. It used to run
    // `resolve_associated_calls` first and skip
    // `expand_nested_field_updates` entirely, which broke dotted-path
    // struct updates in tests only:
    //
    //   let g2 = Game { ship.pos.x: 99, ..g };
    //     -> parse-level error, inside `plum test`; fine under `plum run`
    //
    // The order is load-bearing and its reasoning lives at the run
    // path: `TypeContext` is built BEFORE `resolve_associated_calls` so
    // the nested-update expansion can run first, which is safe because
    // `TypeContext::from_items` only ever reads declarations.
    let type_ctx = TypeContext::from_items(&program.items).map_err(|e: CompileError| e.context("type error"))?;
    crate::nested_struct_update::expand_nested_field_updates(&mut program, &type_ctx)
        .map_err(|e: CompileError| e.context("type error"))?;
    crate::assoc_fns::resolve_associated_calls(&mut program);
    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(&program).map_err(|e: CompileError| e.context("type error"))?;
    plum_ir::movecheck::check_moves(&program).map_err(|e: CompileError| e.context("move error"))?;

    // All FOUR side-channels, matching `run_resolved_program_with_
    // process_args_diag` exactly. Two of them were missing until
    // 2026-08-21, and the effect was that `plum test` lowered a
    // DIFFERENT program than `plum run` did:
    //
    //   let seven (): Int = 7
    //   let test_x (): Unit = assert_eq(seven(), 7)
    //     -> "seven expects 1 argument(s), found 0"
    //
    //   let double = scale(2);
    //     -> "scale expects 2 argument(s), found 1"
    //
    // `unit_sugar_calls` carries inference's answer about which `f()`
    // calls mean zero arguments rather than one Unit; `partial_calls`
    // carries which calls are partial applications. Lowering has no
    // type information of its own and cannot re-derive either.
    //
    // This is the `sh test` bug in a different house (DESIGN.md's "The
    // test runner was running on the wrong engine"): a test runner on a
    // different pipeline than the thing it is testing, invisible
    // because the smoke fixture happened not to use the affected
    // features.
    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone())
        .with_unit_sugar_calls(infer.unit_sugar_calls().clone())
        .with_partial_calls(infer.partial_calls().clone());
    let ir_program = lower_program(&program, &lowering_ctx).map_err(|e: CompileError| e.context("lowering error"))?;
    let ir_program = optimize_program(ir_program);

    let mut interp = Interpreter::new();
    interp.set_struct_field_names(lowering_ctx.struct_fields().clone());
    interp
        .load_program(&ir_program)
        .map_err(|e| CompileError::spanless(format!("load error: {e}")))?;

    Ok(names
        .iter()
        .map(|name| {
            let result = interp.call(name, vec![Value::Unit]).map(|_| ());
            TestOutcome { name: name.clone(), result }
        })
        .collect())
}

// `plum test --native` -- which compiled each test to its own binary
// and ran it as a child process -- lived here until 2026-08-21, when
// the Rust BACKEND was removed (DESIGN.md's "Deleting the Rust
// backend"). It existed because a native runtime-check failure is a
// hard process abort rather than a catchable Result, so each test
// needed its own process to have its outcome observed at all.
//
// `plum test` runs through the interpreter now, and the self-hosted
// `./sh test` is what compiles tests. `bootstrap/test-smoke` exercises
// both.

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    fn parse(src: &str) -> ast::Program {
        let tokens = Lexer::new(src).tokenize();
        Parser::new(tokens).parse_program().unwrap_or_else(|e| panic!("parse error: {e}"))
    }

    #[test]
    fn discovers_root_and_qualified_test_functions_only() {
        // `shapes.test_two` isn't something you can literally write in
        // Plum SOURCE (module qualification is a post-parse rename the
        // `Resolver` applies — see `modules.rs`'s own doc comment), so
        // this goes through the real multi-module resolution pipeline
        // to produce one, rather than hand-constructing an `ast::Item`.
        let shapes = "pub let test_two (): Unit = ()";
        let root = "let test_one (): Unit = ()\n\
                     let not_a_test (): Unit = ()\n\
                     let testify (): Unit = ()";
        let program = crate::resolve_modules(&[("shapes", shapes), ("", root)]).unwrap();
        let mut names = discover_tests(&program);
        names.sort();
        assert_eq!(names, vec!["shapes.test_two".to_string(), "test_one".to_string()]);
    }

    #[test]
    fn discover_tests_on_a_program_with_no_tests_is_empty() {
        let program = parse("let main (): Unit = ()");
        assert_eq!(discover_tests(&program), Vec::<String>::new());
    }

    #[test]
    fn run_tests_interpreted_reports_pass_and_fail_independently() {
        let program = crate::with_prelude(parse(
            "let test_pass (): Unit = assert(true)\n\
             let test_fail (): Unit = assert_eq(1, 2)\n\
             let test_also_pass (): Unit = assert_eq(3, 3)",
        ));
        let names = discover_tests(&program);
        let mut outcomes = run_tests_interpreted(program, &names).unwrap();
        outcomes.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(outcomes[0].name, "test_also_pass");
        assert!(outcomes[0].result.is_ok());
        assert_eq!(outcomes[1].name, "test_fail");
        assert!(outcomes[1].result.as_ref().unwrap_err().contains("left != right"));
        assert_eq!(outcomes[2].name, "test_pass");
        assert!(outcomes[2].result.is_ok());
    }

    #[test]
    fn run_tests_interpreted_surfaces_a_project_level_type_error_as_the_outer_result() {
        let program = crate::with_prelude(parse("let test_bad (): Unit = 1 + true"));
        let names = discover_tests(&program);
        let err = run_tests_interpreted(program, &names).expect_err("expected a type error");
        assert!(err.to_string().contains("type error"), "unexpected error: {err}");
    }



}
