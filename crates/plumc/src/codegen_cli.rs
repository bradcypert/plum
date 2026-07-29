//! Compiles a Plum program to a real native binary via `plum-codegen`
//! (LLVM IR text) + `clang` (compile + link) — an ADDITIONAL backend
//! alongside the tree-walking interpreter (`run_resolved_program`),
//! not a replacement. See DESIGN.md's "Implementation plan" section:
//! the interpreter validated the memory model first; this is the
//! LLVM backend that was always the intended next step, scoped for
//! v1 to scalars + control flow + guaranteed tail calls (see
//! `plum_codegen::emit_program`'s own doc comment for the exact
//! supported subset).

use crate::with_prelude;
use plum_codegen::{CgType, FnSig};
use plum_ir::fbip::optimize_program;
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;
use plum_types::types::Type as PlumType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// A concrete argument value for calling the compiled entry point —
/// deliberately separate from `plum_interp::Value` (which can hold a
/// `HeapRef`/`Closure`/etc. that codegen's v1 scalar-only scope has no
/// way to represent), matching this backend's own narrower CgType set.
#[derive(Debug, Clone, Copy)]
pub enum CgValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
}

impl CgValue {
    fn cg_type(self) -> CgType {
        match self {
            CgValue::Int(_) => CgType::Int,
            CgValue::Float(_) => CgType::Float,
            CgValue::Bool(_) => CgType::Bool,
            CgValue::Unit => CgType::Unit,
        }
    }
}

fn plum_type_to_cg_type(ty: &PlumType) -> Result<CgType, String> {
    match ty {
        PlumType::Int => Ok(CgType::Int),
        PlumType::Float => Ok(CgType::Float),
        PlumType::Bool => Ok(CgType::Bool),
        PlumType::Unit => Ok(CgType::Unit),
        other => Err(format!(
            "codegen v1 only supports Int/Float/Bool/Unit typed functions, found a signature involving {other:?}"
        )),
    }
}

/// Compiles `src` all the way to a native executable and runs it,
/// calling `entry_fn` with `args` and returning captured stdout.
///
/// Runs the SAME parse → prelude → type-check pipeline
/// `run_resolved_program` uses, up through `lower_program`/
/// `optimize_program` — the exact point DESIGN.md's own sequencing
/// note describes as where the interpreter and a future codegen
/// backend diverge ("the frontend and refcount-insertion pass should
/// barely change" when swapping from interpret to codegen). From
/// there: derive a `HashMap<String, FnSig>` from `Infer::
/// infer_program`'s own concrete monomorphic types (the ONLY place
/// real type information exists in this pipeline — `ir::Function`
/// itself carries none), hand the lowered `ir::Program` to
/// `plum_codegen::emit_program`, append a hand-written LLVM `main`
/// that calls `entry_fn` and prints its result, write the `.ll` to a
/// temp file, shell out to `clang` to compile+link it, then run the
/// resulting binary and capture its stdout.
pub fn compile_and_run(src: &str, entry_fn: &str, args: &[CgValue]) -> Result<String, String> {
    let tokens = Lexer::new(src).tokenize();
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program().map_err(|e| format!("parse error: {e}"))?;
    let program = with_prelude(program);

    let type_ctx = TypeContext::from_items(&program.items).map_err(|e| format!("type error: {e}"))?;
    let mut infer = Infer::with_context(type_ctx);
    let types = infer.infer_program(&program).map_err(|e| format!("type error: {e}"))?;

    plum_ir::movecheck::check_moves(&program).map_err(|e| format!("move error: {e}"))?;

    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone());
    let ir_program = lower_program(&program, &lowering_ctx).map_err(|e| format!("lowering error: {e}"))?;
    let ir_program = optimize_program(ir_program);

    // Every top-level FUNCTION's signature (globals are out of v1
    // codegen scope, filtered out here rather than left for
    // `plum_codegen::emit_program`'s own — separate — global-rejection
    // check to catch, since a global's `types` entry is just its
    // value's type, not a `Type::Function`, and would otherwise
    // produce a confusing "not Int/Float/Bool/Unit" error instead of
    // codegen's own clearer "globals aren't supported" one).
    let function_names: std::collections::HashSet<&str> =
        ir_program.functions.iter().map(|f| f.name.as_str()).collect();
    let mut signatures = HashMap::new();
    for (name, ty) in &types {
        if !function_names.contains(name.as_str()) {
            continue;
        }
        let PlumType::Function(params, ret) = ty else {
            return Err(format!("codegen: internal error — function {name:?} has a non-function type {ty:?}"));
        };
        let cg_params = params.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?;
        let cg_ret = plum_type_to_cg_type(ret)?;
        signatures.insert(name.clone(), FnSig { params: cg_params, ret: cg_ret });
    }

    let sig = signatures
        .get(entry_fn)
        .ok_or_else(|| format!("codegen: no such function {entry_fn:?}"))?
        .clone();
    if sig.params.len() != args.len() {
        return Err(format!(
            "codegen: {entry_fn:?} expects {} argument(s), found {}",
            sig.params.len(),
            args.len()
        ));
    }
    for (arg, expected) in args.iter().zip(&sig.params) {
        if arg.cg_type() != *expected {
            return Err(format!(
                "codegen: argument type mismatch calling {entry_fn:?} — expected {expected:?}, found {:?}",
                arg.cg_type()
            ));
        }
    }

    let body_ir = plum_codegen::emit_program(&ir_program, &signatures)?;
    let main_ir = emit_main(entry_fn, sig.ret, args);
    let full_ir = format!("{body_ir}\n{main_ir}");

    run_via_clang(&full_ir)
}

/// A hand-written LLVM `main` — not something `plum_codegen` itself
/// generates, since "what does a Plum program's entry point look
/// like as a native executable" (argument marshaling, how the result
/// becomes observable) is a `plumc`-level concern, not a codegen-
/// library one. Declares `printf` from libc (which `clang` links
/// against automatically) to make the entry point's result
/// observable via stdout.
fn emit_main(entry_fn: &str, ret_ty: CgType, args: &[CgValue]) -> String {
    let args_ir = args
        .iter()
        .map(|a| match a {
            CgValue::Int(n) => format!("i64 {n}"),
            CgValue::Float(f) => format!("double 0x{:016X}", f.to_bits()),
            CgValue::Bool(b) => format!("i1 {}", if *b { 1 } else { 0 }),
            CgValue::Unit => "i1 0".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let (fmt_bytes, fmt_len, call_line) = match ret_ty {
        CgType::Int => (
            "%lld\\0A\\00",
            6,
            format!("  %r = call i64 @{entry_fn}({args_ir})\n  call i32 (ptr, ...) @printf(ptr @fmt, i64 %r)\n"),
        ),
        CgType::Float => (
            "%f\\0A\\00",
            4,
            format!("  %r = call double @{entry_fn}({args_ir})\n  call i32 (ptr, ...) @printf(ptr @fmt, double %r)\n"),
        ),
        CgType::Bool | CgType::Unit => (
            "%d\\0A\\00",
            4,
            format!(
                "  %r = call i1 @{entry_fn}({args_ir})\n  %rz = zext i1 %r to i32\n  call i32 (ptr, ...) @printf(ptr @fmt, i32 %rz)\n"
            ),
        ),
    };
    format!(
        "declare i32 @printf(ptr, ...)\n@fmt = constant [{fmt_len} x i8] c\"{fmt_bytes}\"\n\ndefine i32 @main() {{\nentry:\n{call_line}  ret i32 0\n}}\n"
    )
}

fn run_via_clang(ir: &str) -> Result<String, String> {
    // A unique directory per CALL, not just per process — test threads
    // within the same process (`cargo test` runs them in parallel by
    // default) would otherwise race to write/execute the SAME binary
    // path, surfacing as a spurious "Text file busy" error, not a real
    // correctness bug.
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("plumc-codegen-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create temp build directory: {e}"))?;
    let ll_path = dir.join("program.ll");
    let bin_path: PathBuf = dir.join("program");
    std::fs::write(&ll_path, ir).map_err(|e| format!("failed to write generated IR: {e}"))?;

    let compile = Command::new("clang")
        .arg(&ll_path)
        .arg("-o")
        .arg(&bin_path)
        .output()
        .map_err(|e| format!("could not run `clang` (required to compile generated LLVM IR — is it on PATH?): {e}"))?;
    if !compile.status.success() {
        return Err(format!(
            "clang failed to compile the generated IR:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        ));
    }

    let run = Command::new(&bin_path)
        .output()
        .map_err(|e| format!("failed to run compiled binary {bin_path:?}: {e}"))?;
    if !run.status.success() {
        return Err(format!(
            "compiled program exited with a non-zero status: {:?}\nstdout: {}\nstderr: {}",
            run.status.code(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arithmetic_compiles_and_runs() {
        let out = compile_and_run("let go () = 2 + 3 * 4", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "14");
    }

    #[test]
    fn deep_tail_recursion_does_not_stack_overflow() {
        // The key correctness proof for this whole chunk: a million
        // levels of "recursion" would overflow the stack without real
        // tail-call elimination — this only succeeds because `musttail`
        // actually reused the same stack frame.
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        let out = compile_and_run(src, "sum", &[CgValue::Int(1_000_000), CgValue::Int(0)]).unwrap();
        assert_eq!(out, "500000500000");
    }

    #[test]
    fn mutual_tail_recursion_does_not_stack_overflow() {
        let src = "\
            let is_even n = if n == 0 { true } else { is_odd(n - 1) }\n\
            let is_odd n = if n == 0 { false } else { is_even(n - 1) }\n\
        ";
        let out = compile_and_run(src, "is_even", &[CgValue::Int(1_000_001)]).unwrap();
        assert_eq!(out, "0");
    }

    #[test]
    fn if_and_comparison_compile_and_run() {
        let src = "let max a b = if a > b { a } else { b }";
        let out = compile_and_run(src, "max", &[CgValue::Int(3), CgValue::Int(7)]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn short_circuit_and_never_evaluates_the_untaken_side() {
        // `false && (1 / 0 == 0)` — if `&&` were compiled as a plain,
        // eager instruction (both sides always evaluated), this would
        // trap on the integer division by zero. Success here proves
        // the untaken branch's code genuinely never executes.
        let src = "let go () = false && (1 / 0 == 0)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "0");
    }

    #[test]
    fn short_circuit_or_never_evaluates_the_untaken_side() {
        let src = "let go () = true || (1 / 0 == 0)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn float_arithmetic_compiles_and_runs() {
        let out = compile_and_run("let go () = 1.5 + 2.5", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "4.000000");
    }

    #[test]
    fn a_construct_outside_v1_scope_is_a_clear_error() {
        // `go`'s own DECLARED signature is Int -> Int (fully within
        // v1 scope), but its BODY constructs a struct — this exercises
        // `plum_codegen`'s own per-expression rejection, not just
        // `plumc`'s signature-conversion gate (see the next test for
        // that one).
        let src = "struct Point { x: Int }\nlet go (n: Int): Int = { let p = Point { x: n }; 5 }";
        let err = compile_and_run(src, "go", &[CgValue::Int(1)]).expect_err("expected a codegen scope error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_function_whose_signature_is_outside_v1_scope_is_a_clear_error() {
        let src = "struct Point { x: Int }\nlet go () = Point { x: 1 }";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected a signature-scope error");
        assert!(err.contains("Int/Float/Bool/Unit"), "unexpected error: {err}");
    }
}
