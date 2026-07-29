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
use plum_syntax::ast;
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
        // A non-generic struct/enum reference — `args` empty means the
        // type has no type parameters of its own AND isn't a generic
        // INSTANTIATION either (e.g. `Option[Int]` is `Enum("Option",
        // [Int])`, non-empty, rejected here too) — see DESIGN.md:
        // monomorphization is separate, later work, since generics are
        // fully erased by lowering and a generic type's field types
        // can't be resolved to one concrete LLVM representation from
        // the erased IR alone.
        PlumType::Struct(_, args) | PlumType::Enum(_, args) if args.is_empty() => Ok(CgType::Heap),
        other => Err(format!(
            "codegen only supports Int/Float/Bool/Unit or a non-generic struct/enum, found a signature \
             involving {other:?}"
        )),
    }
}

/// Every non-generic struct/enum-variant declared in `program`,
/// resolved to its fields' `CgType`s via `type_ctx` — see `plum_codegen::
/// TagFields`'s doc comment for how this is used. A struct/enum with
/// type parameters of its own, or any field whose type resolves
/// through `plum_type_to_cg_type` as unsupported (including a field
/// that's itself a GENERIC instantiation, like `Option[Int]`), is
/// simply OMITTED here rather than failing this whole derivation —
/// `plum_codegen::emit_program` reports a clear "unknown tag" error
/// only if the program's REACHABLE code actually tries to construct or
/// match that specific type, rather than failing the whole compile
/// over an unrelated, unused generic declaration.
fn derive_tag_fields(program: &ast::Program, type_ctx: &TypeContext) -> plum_codegen::TagFields {
    let mut tag_fields = plum_codegen::TagFields::new();
    for item in &program.items {
        match &item.kind {
            ast::ItemKind::Struct(decl) if decl.generics.is_empty() => {
                if let Some(fields) = type_ctx.struct_fields(&decl.name) {
                    if let Ok(cg_fields) = fields.iter().map(|(_, ty)| plum_type_to_cg_type(ty)).collect::<Result<Vec<_>, _>>() {
                        tag_fields.insert(decl.name.clone(), cg_fields);
                    }
                }
            }
            ast::ItemKind::Enum(decl) if decl.generics.is_empty() => {
                for variant in &decl.variants {
                    if let Some((_, payload)) = type_ctx.variant(&variant.name) {
                        if let Ok(cg_fields) = payload.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>() {
                            tag_fields.insert(variant.name.clone(), cg_fields);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    tag_fields
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
    let tag_fields = derive_tag_fields(&program, &type_ctx);
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
    // A heap-shaped entry-point RETURN isn't printable (this chunk
    // adds no `ToString`-equivalent for compiled heap values) — real
    // programs construct/consume heap values INTERNALLY; only the
    // scalar entry-point signatures this whole test suite already uses
    // are supported for the compiled `main` wrapper itself.
    if sig.ret == CgType::Heap {
        return Err(format!(
            "codegen: {entry_fn:?} returns a heap-shaped value, which the compiled entry point can't print yet"
        ));
    }

    let body_ir = plum_codegen::emit_program(&ir_program, &signatures, &tag_fields)?;
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
        // `compile_and_run` already rejects a `Heap`-returning entry
        // point before ever calling this function — see its own doc
        // comment on why. Unreachable in practice, kept as a defensive
        // error (not a panic) rather than silently producing garbage
        // IR if that check is ever bypassed.
        CgType::Heap => {
            return "; unreachable: compile_and_run rejects a Heap-returning entry point before this point"
                .to_string()
        }
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
    fn a_construct_outside_codegen_scope_is_a_clear_error() {
        // `go`'s own DECLARED signature is Int -> Int (fully within
        // supported scope), but its BODY constructs a string — a real
        // construct still outside codegen's scope (structs/enums are
        // now supported — see the heap-value tests below) — exercising
        // `plum_codegen`'s own per-expression rejection, not just
        // `plumc`'s signature-conversion gate (see the next test for
        // that one).
        let src = "let go (n: Int): Int = { let s = \"hi\"; 5 }";
        let err = compile_and_run(src, "go", &[CgValue::Int(1)]).expect_err("expected a codegen scope error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_function_whose_signature_is_outside_codegen_scope_is_a_clear_error() {
        // `Option` is generic — its signature can't resolve to one
        // concrete LLVM representation (see DESIGN.md: monomorphization
        // is separate, later work).
        let src = "let go (): Option[Int] = Some(1)";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected a signature-scope error");
        assert!(err.contains("non-generic"), "unexpected error: {err}");
    }

    // --- heap values: structs, enums, refcounting, Match ---

    #[test]
    fn a_struct_is_constructed_and_its_fields_read_back_via_match() {
        // Field access (`p.x`) desugars through `Match`, same as
        // everywhere else in this codebase — so this exercises Ctor
        // construction AND Match-based field extraction together.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = { let p = Point { x: 3, y: 4 }; p.x + p.y }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn a_recursive_enum_linked_list_sums_via_tail_recursion() {
        // Proves Ctor/Match/refcounting and guaranteed tail calls all
        // compose correctly: a real self-referential enum (`List`
        // contains a `List`), built via nested `Ctor`s, summed via a
        // TAIL-recursive accumulator function.
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let sum_acc (lst: List) (acc: Int): Int = match lst {\n\
                Cons(h, t) => sum_acc(t, acc + h),\n\
                Nil => acc,\n\
            }\n\
            let go (): Int = sum_acc(Cons(1, Cons(2, Cons(3, Nil))), 0)\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn a_match_guard_falls_through_to_the_next_arm_when_it_fails() {
        let src = "\
            enum Shape { Circle(Int), Square(Int) }\n\
            let classify (s: Shape): Int = match s {\n\
                Circle(r) if r > 10 => 1,\n\
                Circle(r) => 2,\n\
                Square(side) => 3,\n\
            }\n\
            let go (): Int = classify(Circle(5))\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "2");
    }

    #[test]
    fn ctor_reuse_produces_correct_output_for_a_real_reuse_eligible_program() {
        // `inc_all`'s `Cons(h + 1, inc_all(t))` arm is exactly FBIP's
        // reuse-in-place shape (bare-`Var` scrutinee `lst`, arm body a
        // direct same-arity `Ctor`) — `plum-codegen`'s own unit test
        // (`ctor_reuse_never_calls_plum_alloc_on_the_reuse_path`)
        // already verifies the REUSE-vs-fresh-alloc branch SHAPE
        // structurally; this test verifies the shape actually EXECUTES
        // correctly end to end, which a text-only IR inspection can't
        // prove by itself.
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let inc_all (lst: List): List = match lst {\n\
                Cons(h, t) => Cons(h + 1, inc_all(t)),\n\
                Nil => Nil,\n\
            }\n\
            let sum_acc (lst: List) (acc: Int): Int = match lst {\n\
                Cons(h, t) => sum_acc(t, acc + h),\n\
                Nil => acc,\n\
            }\n\
            let go (): Int = sum_acc(inc_all(Cons(1, Cons(2, Cons(3, Nil)))), 0)\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "9");
    }
}
