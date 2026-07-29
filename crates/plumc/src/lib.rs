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
/// declaration order not matter for name resolution. A user program
/// declaring its OWN `Option`/`Result` (or anything else with the SAME
/// name) is now a real "already declared" error, same as redeclaring
/// any other name — see `TypeContext::from_items`'s duplicate-name
/// check.
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

    // A second, independent static gate — DESIGN.md's "channel send is
    // a move": reusing a value after `tx.send(v)` is a compile error.
    // Runs on the AST (see `movecheck`'s own doc comment for why), so
    // it doesn't need to wait for lowering; placed after type-checking
    // simply to keep the "cheapest/most-fundamental check first" order,
    // not because either gate depends on the other.
    plum_ir::movecheck::check_moves(&program).map_err(|e| format!("move error: {e}"))?;

    // `p.x` needs to know WHICH struct `p` is to lower correctly —
    // lowering has no type information of its own, so this carries
    // inference's own answer across as a span-keyed side-channel. See
    // `Infer::field_owners`/`LoweringContext::field_owners`'s doc
    // comments for the full reasoning.
    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone());
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
    fn extern_call_inside_unsafe_runs_through_the_full_gated_pipeline() {
        let src = r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let main x = unsafe { sqrt(x) }
        "#;
        let result = typecheck_and_run(src, "main", vec![Value::Float(9.0)]);
        assert_eq!(result, Ok(Value::Float(3.0)));
    }

    #[test]
    fn extern_call_outside_unsafe_is_rejected_before_it_ever_reaches_the_interpreter() {
        let src = r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let main x = sqrt(x)
        "#;
        let err = typecheck_and_run(src, "main", vec![Value::Float(9.0)]).expect_err("expected a type error");
        assert!(err.contains("unsafe"), "unexpected error: {err}");
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
    fn for_over_an_array_bound_to_a_local_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let arr = [1, 2, 3, 4]; let mut sum = 0; for x in arr { sum = sum + x; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn for_over_an_array_literal_directly_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let mut sum = 0; for x in [10, 20, 30] { sum = sum + x; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(60)));
    }

    #[test]
    fn for_over_an_empty_array_runs_zero_iterations() {
        let src = "let use_it dummy = { let mut sum = 0; for x in [] { sum = sum + x; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0)));
    }

    #[test]
    fn array_pop_set_remove_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let a = [1, 2, 3]; a.pop().len() + a.set(0, 99)[0] + a.remove(1)[1] }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        // pop().len() = 2, set(0,99)[0] = 99, remove(1)[1] = 3
        assert_eq!(result, Ok(Value::Int(2 + 99 + 3)));
    }

    #[test]
    fn array_map_filter_fold_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = [1, 2, 3, 4, 5].map(|x| x * 2).filter(|x| x > 4).fold(0, |acc, x| acc + x)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        // [2,4,6,8,10] -> filter >4 -> [6,8,10] -> fold sum -> 24
        assert_eq!(result, Ok(Value::Int(24)));
    }

    #[test]
    fn array_map_can_change_the_element_type() {
        let src = "let use_it dummy = [1, 2, 3].map(|x| x > 1).filter(|b| b).len()";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn array_push_pop_set_remove_reuse_in_place_end_to_end() {
        // The reuse-in-place path (uniquely-owned `a` fed straight into
        // `.push()`/`.pop()`/`.set()`/`.remove()`) must produce the same
        // RESULTS as the always-copy path — reuse is purely a memory
        // optimization, never a semantic change.
        let src = "let use_it dummy = { let a = [1, 2, 3]; a.push(4).set(0, 99).remove(1).pop().len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        // [1,2,3] -> push 4 -> [1,2,3,4] -> set(0,99) -> [99,2,3,4] ->
        // remove(1) -> [99,3,4] -> pop -> [99,3] -> len -> 2
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn array_reused_after_a_reuse_in_place_operation_still_sees_its_original_contents() {
        // `a` is used again after `.push()` — the exact case that must
        // NOT reuse in place (see the plum-ir/plum-interp tests proving
        // this at the mark_reuse/heap-refcount level); this proves it
        // holds true through the entire real pipeline too.
        let src = "let use_it dummy = { let a = [1, 2, 3]; let b = a.push(4); a.len() + b.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn string_concat_and_len_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = \"hello, \".concat(\"world\").len()";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(12)));
    }

    #[test]
    fn string_equality_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = if \"abc\" == \"abc\" { 1 } else { 0 }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn string_concat_argument_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = \"abc\".concat(5)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_struct_field_can_hold_a_string_end_to_end() {
        let src = "struct Person { name: String, age: Int }\n\
                    let greet (p: Person) = \"hi \".concat(p.name)\n\
                    let use_it dummy = greet(Person { name: \"Ada\", age: 30 }).len()";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn string_reused_after_a_reuse_in_place_concat_still_sees_its_original_contents() {
        // Same "used again later, so reuse must not fire" regression
        // proof as arrays', now for the heap-backed string
        // representation.
        let src = "let use_it dummy = { let s = \"ab\"; let t = s.concat(\"cd\"); s.len() + t.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn string_indexing_and_runes_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let s = \"café\"; s.runes().len() * 10 + s[0] - 87 }";
        // runes().len() = 4 -> 40; s[0] = 'c' = 99; 99 - 87 = 12; 40+12 = 52
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(52)));
    }

    #[test]
    fn string_runes_can_be_iterated_with_for_over_arrays() {
        // Combines two chunks from tonight: `.runes()` (Array[Int]) fed
        // straight into `for x in arr` iteration.
        let src = "let use_it dummy = { let mut sum = 0; for r in \"abc\".runes() { sum = sum + r; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int('a' as i64 + 'b' as i64 + 'c' as i64)));
    }

    #[test]
    fn string_index_out_of_bounds_is_a_runtime_error_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = \"abc\"[10]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("out of bounds"), "expected an out-of-bounds error, got: {err}");
    }

    #[test]
    fn indexing_with_a_non_int_index_is_rejected_before_running() {
        let src = "let use_it dummy = \"abc\"[true]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn string_trim_and_split_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let parts = \"  a, b, c  \".trim().split(\", \"); parts.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(3)));
    }

    #[test]
    fn string_split_parts_can_be_further_processed() {
        // Combines this evening's chunks: `.split()` produces
        // `Array[Str]`, which `.map()`/`.filter()`/`for` all already
        // handle for free, same as `.runes()`.
        let src = "let use_it dummy = { let mut total = 0; for part in \"ab,cde,f\".split(\",\") { total = total + part.len(); }; total }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn string_trim_reused_after_a_reuse_in_place_operation_still_sees_its_original_contents() {
        let src = "let use_it dummy = { let s = \"  ab  \"; let t = s.trim(); s.len() + t.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(8)));
    }

    #[test]
    fn split_argument_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = \"a,b\".split(5)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn string_case_conversion_and_predicates_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let s = \"Hello World\";\
                    if s.to_lower().contains(\"world\") && s.starts_with(\"Hello\") && s.ends_with(\"World\") { 1 } else { 0 } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn string_replace_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = \"2026-07-28\".replace(\"-\", \"/\") == \"2026/07/28\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn string_replace_reused_after_a_reuse_in_place_operation_still_sees_its_original_contents() {
        let src = "let use_it dummy = { let s = \"a-b\"; let t = s.replace(\"-\", \"_\"); s == \"a-b\" && t == \"a_b\" }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn to_string_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let n = 42; \"the answer is \".concat(n.to_string()) == \"the answer is 42\" }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn to_string_on_a_still_unresolved_generic_parameter_is_permitted_and_works_when_called() {
        // Regression coverage: `.to_string()` used inside a generic
        // function body (where the parameter's type isn't resolved
        // until a call site pins it) must type-check and run
        // correctly — this is exactly the shape `.map(|x| x.to_string())`
        // needs, just spelled out as an ordinary top-level function.
        let src = "let stringify x = x.to_string()\n\
                    let use_it dummy = stringify(42) == \"42\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn to_string_on_an_unsupported_type_reached_only_through_a_generic_parameter_is_a_runtime_error() {
        // The other half of the permissive-at-compile-time tradeoff:
        // if a generic `.to_string()` call's parameter type turns out
        // to be something unsupported (here, `Array[Int]`) ONLY
        // discoverable at the call site rather than the definition,
        // the interpreter's own runtime check is what catches it.
        let src = "let stringify x = x.to_string()\n\
                    let use_it dummy = stringify([1, 2])";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("not yet supported"), "expected a not-yet-supported error, got: {err}");
    }

    #[test]
    fn to_string_composes_with_map_and_fold_to_build_a_string() {
        // Combines several of tonight's chunks: build a string out of
        // numbers via .map()/.to_string()/.concat()/.fold().
        let src = "let use_it dummy = [1, 2, 3].map(|x| x.to_string()).fold(\"\", |acc, s| acc.concat(s)) == \"123\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn ref_aliasing_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let a = ref(1); let b = a; b.set(99); a.get() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(99)));
    }

    #[test]
    fn ref_used_as_a_counter_through_a_for_loop() {
        // A real-world-shaped use case: a Ref accumulator threaded
        // through a `for` loop, mutated on every iteration.
        let src = "let use_it dummy = { let counter = ref(0); for i in 0..5 { counter.set(counter.get() + i); }; counter.get() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0 + 1 + 2 + 3 + 4)));
    }

    #[test]
    fn ref_set_argument_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = { let r = ref(5); r.set(true) }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_struct_field_can_hold_a_ref() {
        let src = "struct Counter { value: Ref[Int] }\n\
                    let increment (c: Counter) = c.value.set(c.value.get() + 1)\n\
                    let use_it dummy = { let c = Counter { value: ref(0) }; increment(c); increment(c); c.value.get() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn capturing_a_ref_across_a_spawn_boundary_is_rejected_at_runtime() {
        let src = "let use_it dummy = { let r = ref(5); spawn { r.get() }.join() }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("Ref"), "expected a Ref-boundary error, got: {err}");
    }

    #[test]
    fn literal_match_runs_through_the_full_gated_pipeline() {
        let src = "let classify n = match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" }\n\
                    let use_it dummy = classify(1) == \"one\" && classify(5) == \"many\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn literal_match_without_a_trailing_catchall_is_rejected_before_running() {
        let src = "let use_it dummy = match 1 { 0 => \"zero\", 1 => \"one\" }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn match_single_wildcard_arm_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 1, y: 2 }) { _ => 42 }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn trailing_catchall_mixed_into_a_variant_match_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let area shape = match shape { Circle(r) => 3.14 * r * r, _ => 0.0 }\n\
                    let use_it dummy = area(Square(5.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(0.0)));
    }

    #[test]
    fn non_last_catchall_mixed_into_a_variant_match_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { _ => 0.0, Circle(r) => r }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn or_pattern_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float), Square(Float), Triangle(Float) }\n\
                    let area shape = match shape { Circle(r) | Square(r) => r, Triangle(r) => 0.0 }\n\
                    let use_it dummy = area(Circle(3.0)) + area(Square(4.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(7.0)));
    }

    #[test]
    fn or_pattern_with_mismatched_bindings_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { Circle(v) | Square(w) => v }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_non_exhaustive_match_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { Circle(r) => r }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
        assert!(err.contains("Square"), "expected the missing variant named, got: {err}");
    }

    #[test]
    fn an_exhaustive_match_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let area shape = match shape { Circle(r) => 3.14 * r * r, Square(s) => s * s }\n\
                    let use_it dummy = area(Square(4.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(16.0)));
    }

    #[test]
    fn a_match_missing_none_on_the_prelude_option_is_rejected_before_running() {
        let src = "let use_it dummy = match Some(5) { Some(x) => x }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
        assert!(err.contains("None"), "expected None named as missing, got: {err}");
    }

    #[test]
    fn a_self_referential_global_closure_runs_through_the_full_gated_pipeline() {
        let src = "let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }\n\
                    let use_it dummy = fib(10)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(55)));
    }

    #[test]
    fn a_self_referential_local_closure_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }; fib(10) }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(55)));
    }

    #[test]
    fn a_range_for_loop_still_works_after_the_array_for_loop_side_channel_was_added() {
        // Regression coverage alongside `a_range_stored_and_passed_around_
        // runs_through_the_full_gated_pipeline` above: a genuinely
        // Range-typed, still-generic-at-the-point-of-inference loop
        // variable must not get misclassified as array-typed.
        let src = "let sum_range r = { let mut sum = 0; for i in r { sum = sum + i; }; sum }\n\
                    let use_it dummy = sum_range(0..5)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
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
    fn a_program_redeclaring_option_itself_is_now_a_real_error() {
        // Previously silently shadowed the prelude's `Option` (no
        // duplicate-declaration detection existed anywhere) — now that
        // detection exists, redeclaring a prelude type is caught the
        // same as redeclaring anything else.
        let src = "enum Option[T] { Some(T), None, Neither }\n\
                    let use_it dummy = match (Neither) { Some(x) => 1, None => 2, Neither => 3 }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
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
    fn channel_send_and_recv_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); tx.send(42); rx.recv() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn a_value_crosses_a_real_thread_boundary_via_spawn_and_a_channel() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let (tx, rx) = channel[Point]();\
                    let t = spawn { tx.send(Point { x: 5, y: 6 }) };\
                    let p = rx.recv();\
                    t.join();\
                    match p { Point(a, b) => a + b } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(11)));
    }

    #[test]
    fn sending_the_wrong_element_type_is_rejected_before_running() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); tx.send(true) }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_bound_satisfying_generic_struct_runs_through_the_full_gated_pipeline() {
        let src = "struct Box[T: Num] { val: T }\n\
                    let use_it dummy = match (Box { val: 5 }) { Box(v) => v }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn a_bound_violating_generic_struct_is_rejected_before_running() {
        let src = "struct Box[T: Num] { val: T }\n\
                    let use_it dummy = Box { val: true }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_redeclared_function_is_rejected_before_running() {
        let src = "let square x = x * x\nlet square x = x + x\nlet use_it dummy = square(3)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn using_a_channel_send_value_afterward_is_rejected_before_running() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); let p = 5; tx.send(p); p }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a move error, not a successful run");
        assert!(err.starts_with("move error:"), "expected a move error, got: {err}");
    }

    #[test]
    fn sending_a_value_and_never_reusing_it_runs_normally() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); let p = 5; tx.send(p); rx.recv() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn a_correctly_annotated_parameter_runs_through_the_full_gated_pipeline() {
        let src = "let square (x: Int) = x * x";
        let result = typecheck_and_run(src, "square", vec![Value::Int(6)]);
        assert_eq!(result, Ok(Value::Int(36)));
    }

    #[test]
    fn a_mismatched_parameter_annotation_is_rejected_before_running() {
        let src = "let use_it (x: Bool) = x + 1";
        let err = typecheck_and_run(src, "use_it", vec![Value::Bool(true)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_struct_typed_parameter_annotation_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let dx (p: Point) = match p { Point(a, b) => a }\n\
                    let use_it dummy = dx(Point { x: 7, y: 0 })";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn an_array_typed_parameter_annotation_runs_through_the_full_gated_pipeline() {
        // This is the gap flagged when `for x in arr` landed: `arr`'s
        // type couldn't previously be pinned via an explicit `Array[T]`
        // annotation, only inferred structurally.
        let src = "let sum_array (arr: Array[Int]) = { let mut sum = 0; for x in arr { sum = sum + x; }; sum }\n\
                    let use_it dummy = sum_array([1, 2, 3, 4])";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn a_mismatched_array_typed_parameter_annotation_is_rejected_before_running() {
        let src = "let sum_array (arr: Array[Int]) = arr.len()\n\
                    let use_it dummy = sum_array([true, false])";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn an_array_typed_return_annotation_runs_through_the_full_gated_pipeline() {
        let src = "let doubled (arr: Array[Int]): Array[Int] = arr.map(|x| x * 2)\n\
                    let use_it dummy = doubled([1, 2, 3])[1]";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(4)));
    }

    #[test]
    fn a_generic_function_annotation_runs_through_the_full_gated_pipeline() {
        let src = "let identity[T] (x: T): T = x\nlet use_it dummy = identity(42)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn mismatched_shared_generic_parameters_are_rejected_before_running() {
        let src = "let pair[T] (a: T) (b: T): T = a\nlet use_it dummy = pair(1, true)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_bound_satisfying_generic_function_call_runs_through_the_full_gated_pipeline() {
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(21)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(21)));
    }

    #[test]
    fn a_bound_violating_generic_function_call_is_rejected_before_running() {
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(true)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn select_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let (tx1, rx1) = channel[Int]();\
                    let (tx2, rx2) = channel[Int]();\
                    tx2.send(7);\
                    select { v = rx1.recv() => v, w = rx2.recv() => w } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn select_waits_on_a_spawned_task_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int]();\
                    let t = spawn { tx.send(11) };\
                    let v = select { v = rx.recv() => v };\
                    t.join();\
                    v }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(11)));
    }

    #[test]
    fn select_arms_with_mismatched_result_types_are_rejected_before_running() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); tx.send(1);\
                    select { v = rx.recv() => v, w = rx.recv() => true } }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn array_construction_indexing_and_push_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let a = [1, 2, 3]; let b = a.push(4); a[0] + b[3] + b.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1 + 4 + 4)));
    }

    #[test]
    fn an_array_of_structs_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let arr = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];\
                    match arr[1] { Point(a, b) => a + b } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn array_element_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = [1, true]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn array_index_out_of_bounds_is_a_runtime_error_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = [1, 2, 3][10]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("out of bounds"), "expected an out-of-bounds error, got: {err}");
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
