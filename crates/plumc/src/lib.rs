use plum_interp::{Interpreter, Value};
use plum_ir::fbip::optimize_program;
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::ast;
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;

/// `Option[T]`/`Result[T, E]` — DESIGN.md's "no null, anywhere, ever"
/// story specs these as ORDINARY generic enums, "under the hood," not
/// as compiler-magic types. This is exactly that: real Plum source,
/// prepended to every program before anything else sees it, rather
/// than a special-cased builtin type baked into `plum-types`/`plum-ir`
/// directly. A user program is free to pattern-match `Some`/`None`/
/// `Ok`/`Err` with no declaration of its own, exactly as if it had
/// written this itself at the top of the file.
const PRELUDE_SRC: &str = "\
enum Option[T] { Some(T), None }
enum Result[T, E] { Ok(T), Err(E) }
";

/// Parses the prelude once and prepends its items to `program`'s own —
/// items earlier in the list are declared FIRST, but `TypeContext`'s
/// two-phase construction (see its doc comment) already makes
/// declaration order not matter for name resolution, so this ordering
/// is only significant for one thing: a user program declaring its
/// OWN `Option`/`Result` (or anything else with the SAME name) shadows
/// the prelude's version, since later insertions into the same-keyed
/// maps win. There's no duplicate-declaration detection anywhere in
/// this codebase yet (a real gap, but a PRE-EXISTING one, not
/// introduced here) — redeclaring a prelude name silently overrides it
/// rather than erroring, same as redeclaring any other name would.
fn with_prelude(program: ast::Program) -> ast::Program {
    let prelude_tokens = Lexer::new(PRELUDE_SRC).tokenize();
    let prelude_items = Parser::new(prelude_tokens)
        .parse_program()
        .expect("PRELUDE_SRC is fixed, valid Plum source")
        .items;
    let mut items = prelude_items;
    items.extend(program.items);
    ast::Program { items }
}

/// Runs the whole pipeline — parse, type-check, lower, optimize, load,
/// call — and returns the result of calling `fn_name` with `args`.
///
/// Type-checking is a hard gate: a program that fails to type-check is
/// rejected here and never reaches lowering or the interpreter at all.
/// Before this function existed, `plum-types` was fully implemented and
/// tested but nothing in the compiler ever called it — a type error
/// (like adding a `Bool` to an `Int`) only ever surfaced as a confusing
/// runtime error deep in `Interpreter::eval`, if it surfaced at all.
pub fn typecheck_and_run(src: &str, fn_name: &str, args: Vec<Value>) -> Result<Value, String> {
    let tokens = Lexer::new(src).tokenize();
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program().map_err(|e| format!("parse error: {e}"))?;
    let program = with_prelude(program);

    let type_ctx = TypeContext::from_items(&program.items).map_err(|e| format!("type error: {e}"))?;
    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(&program).map_err(|e| format!("type error: {e}"))?;

    // `p.x` needs to know WHICH struct `p` is to lower correctly —
    // lowering has no type information of its own, so this carries
    // inference's own answer across as a span-keyed side-channel. See
    // `Infer::field_owners`/`LoweringContext::field_owners`'s doc
    // comments for the full reasoning.
    let lowering_ctx = LoweringContext::from_items(&program.items).with_field_owners(infer.field_owners().clone());
    let ir_program = lower_program(&program, &lowering_ctx).map_err(|e| format!("lowering error: {e}"))?;
    let ir_program = optimize_program(ir_program);

    let mut interp = Interpreter::new();
    interp.load_program(&ir_program).map_err(|e| format!("load error: {e}"))?;
    interp.call(fn_name, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_typed_recursive_program_runs_through_the_full_gated_pipeline() {
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        let result = typecheck_and_run(src, "sum", vec![Value::Int(5), Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(15)));
    }

    #[test]
    fn ill_typed_program_is_rejected_before_it_ever_reaches_the_interpreter() {
        // `n` is inferred Int from `n == 0`, so passing it to `want_bool`
        // (which requires a Bool) is a real type error. Before wiring,
        // this would have been silently accepted here (since nothing
        // called plum-types) and only misbehaved once actually run.
        let src = "let want_bool b = if b { 1 } else { 0 }\n\
                    let bad n = if n == 0 { want_bool(n) } else { 0 }";
        let err = typecheck_and_run(src, "bad", vec![Value::Int(1)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn parse_errors_are_still_reported_as_such_not_type_errors() {
        let err = typecheck_and_run("let (((", "f", vec![]).expect_err("expected a parse error");
        assert!(err.starts_with("parse error:"), "expected a parse error, got: {err}");
    }

    #[test]
    fn function_bodies_are_fbip_optimized_before_running() {
        // Construct-then-immediately-match-and-reconstruct is exactly
        // the reuse-in-place shape FBIP recognizes (`CtorReuse`). This
        // proves it fires correctly for a NAMED function loaded via
        // `load_program` and invoked via `call` — not just for a single
        // expression handed straight to `eval`, which is all the
        // capstone tests in plum-interp exercised before `optimize_program`
        // existed. If FBIP weren't wired in, this would still produce
        // the right VALUE (2) since reuse is a memory optimization, not
        // a behavior change — so this test is really about the pipeline
        // not panicking/erroring on the RcAnnotated/CtorReuse nodes it
        // now actually loads and evaluates.
        let src = "struct Point { x: Int, y: Int }\n\
                    let run dummy = match (match (Point { x: 1, y: 2 }) { Point(x, y) => Point { x: y, y: x } }) { Point(a, b) => a }";
        let result = typecheck_and_run(src, "run", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn for_and_unsafe_run_through_the_full_gated_pipeline() {
        // `for`/`unsafe` are the newest lowered forms — this proves
        // they're accepted by the type-check gate (not just runnable if
        // it were skipped) AND actually execute correctly together.
        let src = "let count n = unsafe { for i in 0..n { i } }";
        let result = typecheck_and_run(src, "count", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Unit));
    }

    #[test]
    fn closures_run_through_the_full_gated_pipeline_including_as_arguments() {
        let src = "let apply f x = f(x)\n\
                    let use_it n = apply(|x| x + 1, n)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn the_classic_for_loop_accumulator_runs_through_the_full_gated_pipeline() {
        // DESIGN.md's own motivating example for `let mut`, proven all
        // the way through: parse -> type-check -> lower -> FBIP
        // optimize -> run.
        let src = "let sum_to n = { let mut sum = 0; for i in 0..n { sum = sum + i; }; sum }";
        let result = typecheck_and_run(src, "sum_to", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn globals_run_through_the_full_gated_pipeline() {
        let src = "let pi_ish = 3\nlet area r = pi_ish * r * r";
        let result = typecheck_and_run(src, "area", vec![Value::Int(2)]);
        assert_eq!(result, Ok(Value::Int(12)));
    }

    #[test]
    fn a_global_forward_reference_is_rejected_before_running() {
        let src = "let a = b\nlet b = 1";
        let err = typecheck_and_run(src, "a", vec![]).expect_err("expected a type error");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn nested_patterns_run_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 3, y: 4 }, 10) { (Point { x, y }, n) => x + y + n }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(17)));
    }

    #[test]
    fn variant_construction_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float) }\n\
                    let area shape = match shape { Circle(r) => r * r }\n\
                    let use_it dummy = area(Circle(3.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(9.0)));
    }

    #[test]
    fn struct_field_referencing_another_struct_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Line { start: Point, end: Point }\n\
                    let dx (Line { start: Point { x: x0, .. }, end: Point { x: x1, .. } }) = x1 - x0\n\
                    let use_it dummy = dx(Line { start: Point { x: 1, y: 0 }, end: Point { x: 9, y: 0 } })";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(8)));
    }

    #[test]
    fn struct_destructuring_params_run_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let area (Point { x, y }) = x * y\n\
                    let use_it dummy = area(Point { x: 3, y: 4 })";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(12)));
    }

    #[test]
    fn the_swap_example_runs_through_the_full_gated_pipeline() {
        // Tuples now have real type-checker support (previously they
        // could run at the interpreter level but were rejected by the
        // type-check gate) — this is the same flagship
        // `let swap (a, b) = (b, a)` example proven at the interpreter
        // level, now proven through the FULL pipeline including the
        // type-check gate.
        let src = "let swap (a, b) = (b, a)\n\
                    let use_it n = match swap((n, true)) { (x, y) => x }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(7)]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn struct_update_spread_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; let q = Point { x: 9, ..p }; match q { Point { x, y } => x + y } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(13)));
    }

    #[test]
    fn struct_update_spread_with_a_mismatched_struct_type_is_rejected_before_running() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Color { r: Int, g: Int, b: Int }\n\
                    let use_it dummy = { let c = Color { r: 1, g: 2, b: 3 }; Point { x: 9, ..c } }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn match_guards_run_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float) }\n\
                    let classify r = match (Circle(r)) { Circle(r) if r > 5.0 => 1, Circle(r) => 0 }";
        let result = typecheck_and_run(src, "classify", vec![Value::Float(10.0)]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn a_non_bool_match_guard_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float) }\n\
                    let classify r = match (Circle(r)) { Circle(r) if r => 1, Circle(r) => 0 }";
        let err = typecheck_and_run(src, "classify", vec![Value::Float(10.0)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_range_stored_and_passed_around_runs_through_the_full_gated_pipeline() {
        let src = "let sum_range r = { let mut sum = 0; for i in r { sum = sum + i; }; sum }\n\
                    let use_it dummy = { let r = 0..5; sum_range(r) }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn for_over_a_non_range_value_is_rejected_before_running() {
        let src = "let use_it dummy = for i in 5 { i }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_correct_return_type_annotation_runs_through_the_full_gated_pipeline() {
        let src = "let square x: Int = x * x";
        let result = typecheck_and_run(src, "square", vec![Value::Int(4)]);
        assert_eq!(result, Ok(Value::Int(16)));
    }

    #[test]
    fn a_mismatched_return_type_annotation_is_rejected_before_running() {
        // Previously silently accepted: `ret_ty` was parsed but never
        // consulted by `plum-types` at all.
        let src = "let square x: Bool = x * x";
        let err = typecheck_and_run(src, "square", vec![Value::Int(4)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_bare_top_level_function_name_as_a_value_runs_through_the_full_gated_pipeline() {
        let src = "let square x = x * x\n\
                    let f = square\n\
                    let use_it n = f(n)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(25)));
    }

    #[test]
    fn a_bare_function_name_passed_as_a_higher_order_argument_runs_through_the_full_gated_pipeline() {
        let src = "let square x = x * x\n\
                    let apply f x = f(x)\n\
                    let use_it n = apply(square, n)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(4)]);
        assert_eq!(result, Ok(Value::Int(16)));
    }

    #[test]
    fn a_bare_variant_constructor_as_a_value_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float) }\n\
                    let apply f x = f(x)\n\
                    let use_it dummy = match apply(Circle, 5.0) { Circle(r) => r }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(5.0)));
    }

    #[test]
    fn field_access_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; p.x + p.y }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn field_access_on_a_function_call_result() {
        // Unlike a bare function PARAMETER (which has no annotation
        // syntax to pin its type — see this session's memory notes on
        // why `let f p = p.x` alone is correctly rejected as ambiguous
        // in a nominal type system with no row polymorphism), a
        // function's RETURN type is always fully determined from its
        // own body, independent of field access — so `.x` on a call
        // result works with no extra help.
        let src = "struct Point { x: Int, y: Int }\n\
                    let make_point dummy = Point { x: 5, y: 6 }\n\
                    let use_it dummy = make_point(dummy).x";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn chained_field_access_through_a_nested_struct() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Line { start: Point, end: Point }\n\
                    let use_it dummy = { let l = Line { start: Point { x: 1, y: 2 }, end: Point { x: 9, y: 9 } }; l.start.x }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn field_access_on_an_unknown_field_is_rejected_before_running() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 1, y: 2 }; p.z }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn the_prelude_option_type_is_available_with_no_declaration_of_its_own() {
        // No `enum Option[T] { .. }` anywhere in `src` — DESIGN.md's
        // "no null, anywhere, ever" story means `Option`/`Result` are
        // always available, injected via `with_prelude` before this
        // source is even parsed against.
        let src = "let unwrap_or default o = match o { Some(x) => x, None => default }\n\
                    let use_it dummy = unwrap_or(0, Some(42))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn the_prelude_none_case_works_with_no_declaration_of_its_own() {
        let src = "let unwrap_or default o = match o { Some(x) => x, None => default }\n\
                    let use_it dummy = unwrap_or(7, None)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn the_prelude_result_type_is_available_with_no_declaration_of_its_own() {
        let src = "let unwrap_or default r = match r { Ok(x) => x, Err(e) => default }\n\
                    let use_it dummy = unwrap_or(0, Ok(42))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn the_prelude_result_err_case_works_with_no_declaration_of_its_own() {
        let src = "let unwrap_or default r = match r { Ok(x) => x, Err(e) => default }\n\
                    let use_it dummy = unwrap_or(0, Err(true))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0)));
    }

    #[test]
    fn a_program_redeclaring_option_itself_shadows_the_prelude_without_erroring() {
        // No duplicate-declaration detection exists anywhere in this
        // codebase yet (a real, pre-existing, separate gap) — a user's
        // OWN `Option` simply wins over the prelude's, matching how any
        // other duplicate top-level name would already behave.
        let src = "enum Option[T] { Some(T), None, Neither }\n\
                    let use_it dummy = match (Neither) { Some(x) => 1, None => 2, Neither => 3 }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(3)));
    }

    #[test]
    fn a_generic_struct_runs_through_the_full_gated_pipeline() {
        let src = "struct Pair[T] { first: T, second: T }\n\
                    let use_it dummy = { let p = Pair { first: 3, second: 4 }; match p { Pair(a, b) => a + b } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn mismatched_generic_type_arguments_are_rejected_before_running() {
        let src = "struct Pair[T] { first: T, second: T }\n\
                    let use_it dummy = Pair { first: 1, second: true }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn spawn_and_join_run_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; let t = spawn { match p { Point(a, b) => a + b } }; t.join() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn joining_a_non_task_value_is_rejected_before_running() {
        let src = "let use_it dummy = 5.join()";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn for_loop_with_mistyped_range_bounds_is_rejected_before_running() {
        let src = "let bad n = for i in true..n { i }";
        let err = typecheck_and_run(src, "bad", vec![Value::Int(5)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }
}
