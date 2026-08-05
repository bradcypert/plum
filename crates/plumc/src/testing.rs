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
    let type_ctx = TypeContext::from_items(&program.items).map_err(|e: CompileError| e.context("type error"))?;
    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(&program).map_err(|e: CompileError| e.context("type error"))?;
    plum_ir::movecheck::check_moves(&program).map_err(|e: CompileError| e.context("move error"))?;

    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone());
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

/// The native-compiled counterpart to `run_tests_interpreted` — see
/// this module's own doc comment for WHY it needs to be structured so
/// differently: a native runtime-check failure (`panic_raw`'s own
/// `@plum_abort`) is a hard process abort, not a catchable `Result`, so
/// there is no way to run more than one test per compiled PROCESS and
/// still observe every test's own outcome.
///
/// Compiles `program`'s IR body exactly ONCE (`compile_program_to_ir_
/// diag` — the `entry_fn` passed to it doesn't affect WHICH functions
/// end up in the shared IR body at all: `plum_ir::monomorphize::plan`'s
/// own doc comment confirms it re-lowers EVERY top-level function
/// regardless of entry point; `entry_fn` only affects the final entry-
/// point RESOLUTION/rename step, irrelevant here since a real test
/// function is never named `"main"`). For each discovered test, builds
/// its OWN native entry point by appending a fresh `emit_main` wrapper
/// to that SAME shared IR body (identical to what `plum build` does
/// for `"main"` specifically, just looped over every test name
/// instead) and runs the resulting binary as its own child process,
/// deleted again immediately after — pass on exit code 0, fail
/// (message from the compiled binary's own captured stdout, where
/// `@plum_abort`'s `printf` put it) on anything else.
///
/// A test whose signature doesn't match the required shape (exactly
/// one `Unit` parameter, a printable return type — the same convention
/// `main` itself is held to) is reported as a per-test FAILURE with a
/// clear message, not a whole-run abort — mirrors `run_tests_
/// interpreted`'s own graceful handling of an arity mismatch (an
/// ordinary `Interpreter::call` error, caught per-test there for free).
pub fn run_tests_native(program: &ast::Program, names: &[String]) -> Result<Vec<TestOutcome>, CompileError> {
    // MUST be `"main"`, not an arbitrary probe name — found via a real
    // `clang` "invalid redefinition of function 'main'" failure on a
    // throwaway project that (entirely reasonably) has its OWN real
    // `let main (): Unit = ...` alongside its tests: `compile_program_
    // to_ir_diag`'s own collision-avoidance rename (renaming a Plum-
    // level `main` to `@__plum_entry_main` so it never clashes with
    // `emit_main`'s hand-written native `@main()`) only fires when the
    // entry_fn IT WAS CALLED WITH resolves to `"main"` — passing
    // anything else left the project's real `main` compiled into the
    // shared `body_ir` under its own unrenamed `@main` symbol, which
    // then collided with EVERY per-test `emit_main` wrapper appended
    // below. Passing `"main"` here is always safe even when the
    // project has no `main` at all (confirmed: `compile_program_to_ir_
    // diag`'s own entry resolution doesn't require the name to exist,
    // only errors on an AMBIGUOUS generic entry).
    let (body_ir, signatures, _resolved_entry, has_globals) = crate::compile_program_to_ir_diag(program, "main")?;

    let mut outcomes = Vec::with_capacity(names.len());
    for name in names {
        outcomes.push(run_one_native_test(name, &body_ir, &signatures, has_globals));
    }
    Ok(outcomes)
}

fn run_one_native_test(
    name: &str,
    body_ir: &str,
    signatures: &std::collections::HashMap<String, plum_codegen::FnSig>,
    has_globals: bool,
) -> TestOutcome {
    let outcome = (|| -> Result<(), String> {
        let sig = signatures
            .get(name)
            .ok_or_else(|| format!("internal error: no signature found for discovered test {name:?}"))?
            .clone();
        if sig.params.len() != 1 {
            return Err(format!(
                "test functions must take exactly one Unit parameter (`let {name} (): T = ...`), found {} parameter(s)",
                sig.params.len()
            ));
        }
        crate::reject_unprintable_return(name, sig.ret.clone())?;
        let main_ir = crate::emit_main(name, sig.ret, &[crate::CgValue::Unit], has_globals);
        let full_ir = format!("{body_ir}\n{main_ir}");

        let bin_path = crate::codegen_cli::unique_temp_dir("plum-test").join(name.replace('.', "_"));
        std::fs::create_dir_all(bin_path.parent().expect("unique_temp_dir always has a parent")).map_err(|e| format!("failed to create temp build directory: {e}"))?;
        crate::compile_ir_to_binary(&full_ir, &bin_path)?;

        let run = std::process::Command::new(&bin_path)
            .output()
            .map_err(|e| format!("failed to run compiled test binary {bin_path:?}: {e}"))?;
        let _ = std::fs::remove_dir_all(bin_path.parent().expect("unique_temp_dir always has a parent"));

        if run.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
        }
    })();
    TestOutcome { name: name.to_string(), result: outcome }
}

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

    #[test]
    fn run_tests_native_reports_pass_and_fail_independently() {
        // One failing test's process abort must NOT prevent the other
        // tests from running (or being reported at all) — the whole
        // point of subprocess-per-test isolation.
        let program = crate::with_prelude(parse(
            "let test_pass (): Unit = assert(true)\n\
             let test_fail (): Unit = assert_eq(1, 2)\n\
             let test_also_pass (): Unit = assert_eq(3, 3)",
        ));
        let names = discover_tests(&program);
        let mut outcomes = run_tests_native(&program, &names).unwrap();
        outcomes.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(outcomes[0].name, "test_also_pass");
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
        assert_eq!(outcomes[1].name, "test_fail");
        assert!(outcomes[1].result.as_ref().unwrap_err().contains("left != right"), "{:?}", outcomes[1].result);
        assert_eq!(outcomes[2].name, "test_pass");
        assert!(outcomes[2].result.is_ok(), "{:?}", outcomes[2].result);
    }

    #[test]
    fn run_tests_native_and_interpreted_agree_on_the_same_program() {
        let program = crate::with_prelude(parse(
            "struct Point { x: Int, y: Int }\n\
             let test_struct_eq_ok (): Unit = assert_eq(Point { x: 1, y: 2 }, Point { x: 1, y: 2 })\n\
             let test_struct_eq_fail (): Unit = assert_eq(Point { x: 1, y: 2 }, Point { x: 1, y: 3 })",
        ));
        let names = discover_tests(&program);
        let native_outcomes = run_tests_native(&program, &names).unwrap();
        let interp_outcomes = run_tests_interpreted(program, &names).unwrap();
        let find = |outcomes: &[TestOutcome], name: &str| outcomes.iter().find(|o| o.name == name).unwrap().result.clone();

        assert!(find(&native_outcomes, "test_struct_eq_ok").is_ok());
        assert!(find(&interp_outcomes, "test_struct_eq_ok").is_ok());
        assert!(find(&native_outcomes, "test_struct_eq_fail").unwrap_err().contains("Point"));
        assert!(find(&interp_outcomes, "test_struct_eq_fail").unwrap_err().contains("Point"));
    }

    #[test]
    fn run_tests_native_works_on_a_project_that_also_has_its_own_real_main() {
        // Found via real end-to-end verification, not a hypothetical:
        // a project's own `let main (): Unit = ...` (a real, ordinary
        // thing for a project under test to have) was compiled into
        // the shared `body_ir` under its own unrenamed `@main` symbol,
        // colliding with every per-test `emit_main` wrapper — `clang`
        // rejected every single test with "invalid redefinition of
        // function 'main'". Fixed by passing `"main"` (not an arbitrary
        // probe name) as `compile_program_to_ir_diag`'s own `entry_fn`,
        // triggering its EXISTING Plum-`main`-vs-native-`main` rename
        // unconditionally.
        let program = crate::with_prelude(parse(
            "let main (): Unit = ()\n\
             let test_pass (): Unit = assert(true)",
        ));
        let names = discover_tests(&program);
        let outcomes = run_tests_native(&program, &names).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
    }
}
