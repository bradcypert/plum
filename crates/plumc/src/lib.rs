use plum_interp::{Interpreter, Value};
use plum_ir::fbip::optimize_program;
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;

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

    let type_ctx = TypeContext::from_items(&program.items).map_err(|e| format!("type error: {e}"))?;
    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(&program).map_err(|e| format!("type error: {e}"))?;

    let lowering_ctx = LoweringContext::from_items(&program.items);
    let ir_program = lower_program(&program, &lowering_ctx).map_err(|e| format!("lowering error: {e}"))?;
    let ir_program = optimize_program(ir_program);

    let mut interp = Interpreter::new();
    interp.load_program(&ir_program);
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
}
