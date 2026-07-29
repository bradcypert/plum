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
        // A struct/enum reference — GENERIC instantiations (`args`
        // non-empty, e.g. `Option[Int]`) are included too, now that
        // `plum_ir::monomorphize` resolves each one to its own mangled,
        // fully concrete `tag_fields` entry: at the LLVM level a heap
        // value is opaque either way (a plain `ptr` — see `CgType::Heap`'s
        // own doc comment in plum-codegen), so this conversion doesn't
        // need to know or care WHICH concrete instantiation a given
        // struct/enum-typed signature position holds. If the program
        // never actually reaches a construction of that specific
        // instantiation, `monomorphize::plan` simply never produces a
        // `tag_fields` entry for it — caught, if it matters, by
        // `plum_codegen::emit_program`'s own "unknown tag" error at the
        // point something actually tries to construct/match it, not
        // here.
        PlumType::Str => Ok(CgType::Str),
        // `Array[T]` recurses into `T` — this is ALSO a genuine bug fix
        // over the previous chunk: before this arm existed, `Array[T]`
        // fell through to the `Struct(..)` wildcard arm just above,
        // silently mapping to plain `CgType::Heap` (indistinguishable
        // from any ordinary struct at the LLVM level), which would only
        // fail LATER with a confusing "unknown tag: 0Array" error the
        // first time codegen actually tried to construct/index one —
        // rather than this clean, specific `Array[T]`-shaped `CgType`
        // from the start. Every OTHER `Struct`/`Enum` reference is
        // still handled by the wildcard arm below.
        PlumType::Struct(name, args) if name == "Array" && args.len() == 1 => {
            Ok(CgType::Array(Box::new(plum_type_to_cg_type(&args[0])?)))
        }
        PlumType::Struct(..) | PlumType::Enum(..) => Ok(CgType::Heap),
        other => Err(format!(
            "codegen only supports Int/Float/Bool/Unit/Str/Array[T] or a non-generic struct/enum, found a \
             signature involving {other:?}"
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
    let (body_ir, signatures, resolved_entry) = compile_to_ir(src, entry_fn)?;
    let sig = signatures
        .get(&resolved_entry)
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
    // A heap-shaped (struct/enum) OR array-shaped entry-point RETURN
    // isn't printable (this chunk's `ToString` only covers Int/Float/
    // Bool/Str — no positional-field/element rendering for a compiled
    // heap or array value) — real programs construct/consume those
    // INTERNALLY, only ever exposing a scalar or `Str` result at the
    // entry point itself; `Str` (NEW as of this chunk) IS printable,
    // via `emit_main`'s own `Str` case below, so it's excluded from
    // this rejection.
    if sig.ret == CgType::Heap {
        return Err(format!(
            "codegen: {entry_fn:?} returns a heap-shaped value, which the compiled entry point can't print yet"
        ));
    }
    if matches!(sig.ret, CgType::Array(_)) {
        return Err(format!(
            "codegen: {entry_fn:?} returns an array-shaped value, which the compiled entry point can't print yet"
        ));
    }

    let main_ir = emit_main(&resolved_entry, sig.ret, args);
    let full_ir = format!("{body_ir}\n{main_ir}");

    run_via_clang(&full_ir)
}

/// The shared front-half of `compile_and_run`: parse → prelude → type-
/// check → monomorphize → lower → codegen, stopping just short of
/// appending a `main` wrapper and shelling out to `clang`. Split out
/// (rather than inlined into `compile_and_run` alone) so tests can
/// inspect the raw generated LLVM IR TEXT directly — e.g. asserting two
/// distinct mangled tags/function definitions both appear — without
/// needing a real `clang` toolchain or process spawn just to check
/// static text shape. Returns the generated IR body, every function's
/// concrete `FnSig` (including every monomorphized instantiation, keyed
/// by its MANGLED name), and `entry_fn`'s own resolved (possibly
/// mangled) name.
fn compile_to_ir(src: &str, entry_fn: &str) -> Result<(String, HashMap<String, FnSig>, String), String> {
    let tokens = Lexer::new(src).tokenize();
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program().map_err(|e| format!("parse error: {e}"))?;
    let program = with_prelude(program);

    let type_ctx = TypeContext::from_items(&program.items).map_err(|e| format!("type error: {e}"))?;
    let mut tag_fields = derive_tag_fields(&program, &type_ctx);
    let mut infer = Infer::with_context(type_ctx);
    let types = infer.infer_program(&program).map_err(|e| format!("type error: {e}"))?;
    let resolved_sites = infer.resolve_generic_sites().map_err(|e| format!("type error: {e}"))?;
    let empty_array_elem_types = infer.resolve_empty_array_elem_types().map_err(|e| format!("type error: {e}"))?;

    plum_ir::movecheck::check_moves(&program).map_err(|e| format!("move error: {e}"))?;

    // `resolve_generic_sites` needs its own `TypeContext` too (the
    // first one was moved into `infer` above — see `Infer::with_context`)
    // — cheap to rebuild from the same, already-validated items rather
    // than threading a second owned copy through `Infer` itself.
    let type_ctx_for_mono = TypeContext::from_items(&program.items).map_err(|e| format!("type error: {e}"))?;
    let mono_plan = plum_ir::monomorphize::plan(
        &program,
        &type_ctx_for_mono,
        &resolved_sites,
        infer.fn_generics(),
        &types,
        infer.field_owners(),
        infer.array_for_loops(),
    )
    .map_err(|e| format!("monomorphization error: {e}"))?;

    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone())
        .with_empty_array_elem_types(empty_array_elem_types);
    let mut ir_program = lower_program(&program, &lowering_ctx).map_err(|e| format!("lowering error: {e}"))?;
    // `mono_plan.functions` REPLACES `lower_program`'s own function list
    // wholesale — it already covers every function actually needed,
    // including ordinary (never-generic) ones re-lowered with mangled
    // tags/callee names wherever their body touches a generic
    // instantiation (see `monomorphize::MonoPlan::functions`'s doc
    // comment for why the plain `lower_program` output can't just be
    // spliced alongside it: an ordinary function's PLAIN-tagged body
    // would reference tags `tag_fields` never has an entry for).
    // Globals/externs are untouched (generics stay out of their scope).
    ir_program.functions = mono_plan.functions;
    let ir_program = optimize_program(ir_program);

    for (mangled, field_types) in &mono_plan.tag_fields {
        let cg_fields = field_types.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?;
        tag_fields.insert(mangled.clone(), cg_fields);
    }

    // Every top-level FUNCTION's signature (globals are out of v1
    // codegen scope, filtered out here rather than left for
    // `plum_codegen::emit_program`'s own — separate — global-rejection
    // check to catch, since a global's `types` entry is just its
    // value's type, not a `Type::Function`, and would otherwise
    // produce a confusing "not Int/Float/Bool/Unit" error instead of
    // codegen's own clearer "globals aren't supported" one). A GENERIC
    // function's own (unmangled) name is never in `function_names`
    // (only its mangled instantiations are — see `MonoPlan::functions`),
    // so this loop naturally skips it; its mangled entries come from
    // `mono_plan.signatures` in the loop right after instead, since
    // `types[name]` for a generic function is a nonsensically
    // unresolved, var-templated signature, not a concrete one.
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
    for (mangled, (params, ret)) in &mono_plan.signatures {
        let cg_params = params.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?;
        let cg_ret = plum_type_to_cg_type(ret)?;
        signatures.insert(mangled.clone(), FnSig { params: cg_params, ret: cg_ret });
    }

    // `entry_fn` may name a GENERIC function with more than one reachable
    // instantiation — there's no single concrete signature to compile a
    // `main` wrapper against in that case, so it's rejected with a clear
    // error rather than silently picking one. A non-generic name (or a
    // generic one instantiated exactly once) resolves straight through.
    let resolved_entry: String = match mono_plan.entry_rename.get(entry_fn) {
        Some(names) if names.len() == 1 => names[0].clone(),
        Some(names) if names.len() > 1 => {
            return Err(format!(
                "codegen: {entry_fn:?} is ambiguous as an entry point — it has {} reachable generic \
                 instantiation(s) ({names:?}); call it from a concrete, non-generic wrapper function instead",
                names.len()
            ));
        }
        _ => entry_fn.to_string(),
    };

    let body_ir = plum_codegen::emit_program(&ir_program, &signatures, &tag_fields)?;
    Ok((body_ir, signatures, resolved_entry))
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
        // Plum strings are length-prefixed, not NUL-terminated on their
        // own — `printf`-ing one directly via `%s` would read past the
        // end of its actual content. Safe here ONLY because every
        // string cell `plum-codegen` allocates keeps one extra trailing
        // `\0` byte in sync (see `plum_codegen::emit_runtime`'s own
        // string-runtime doc comment) purely to make THIS print path
        // safe — an implementation detail invisible to Plum code
        // itself; `.len()` still always reports the true byte length,
        // never `len+1`.
        CgType::Str => (
            "%s\\0A\\00",
            4,
            format!(
                "  %r = call ptr @{entry_fn}({args_ir})\n  \
                 %bytes = getelementptr i8, ptr %r, i64 16\n  \
                 call i32 (ptr, ...) @printf(ptr @fmt, ptr %bytes)\n"
            ),
        ),
        // `compile_and_run` already rejects a `Heap`- or `Array`-
        // returning entry point before ever calling this function — see
        // its own doc comment on why. Unreachable in practice, kept as
        // a defensive error (not a panic) rather than silently
        // producing garbage IR if that check is ever bypassed.
        CgType::Heap | CgType::Array(_) => {
            return "; unreachable: compile_and_run rejects a Heap/Array-returning entry point before this point"
                .to_string()
        }
    };
    // `@printf` is NOT re-declared here — `plum_codegen::emit_runtime`
    // already declares it unconditionally (needed by `@plum_abort`), and
    // LLVM IR rejects a duplicate `declare` for the same function.
    format!(
        "@fmt = constant [{fmt_len} x i8] c\"{fmt_bytes}\"\n\ndefine i32 @main() {{\nentry:\n{call_line}  ret i32 0\n}}\n"
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
        // supported scope), but its BODY calls `.runes()` — a
        // genuinely Unicode-aware string op still outside codegen's
        // scope this chunk (core, byte-level string ops ARE supported
        // now — see the string tests below) — exercising `plum_codegen`'s
        // own per-expression rejection, not just `plumc`'s signature-
        // conversion gate (see the next test for that one).
        let src = "let go (n: Int): Int = { let r = \"hi\".runes(); 5 }";
        let err = compile_and_run(src, "go", &[CgValue::Int(1)]).expect_err("expected a codegen scope error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_function_returning_a_generic_heap_value_directly_at_the_entry_point_is_a_clear_error() {
        // Monomorphization now resolves `Option[Int]`'s signature just
        // fine (see the generics tests below) — but a heap-shaped value
        // still isn't PRINTABLE by the compiled entry point's hand-
        // written `main` wrapper (no `ToString`-equivalent for compiled
        // heap values yet), so this is still a clear error, just a
        // DIFFERENT one than "generics aren't supported at all".
        let src = "let go (): Option[Int] = Some(1)";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected a heap-return error");
        assert!(err.contains("heap-shaped value"), "unexpected error: {err}");
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

    // --- generics / monomorphization ---

    /// Runs the pipeline exactly through `monomorphize::plan` (the SAME
    /// real `with_prelude` pipeline `compile_and_run` uses, not a hand-
    /// built plum-ir-only program) and returns every mangled TAG it
    /// produced — needed because, unlike a function name, a struct/enum
    /// tag never appears as readable text in the generated `.ll` itself
    /// (`plum_codegen` interns every tag to a small integer — see
    /// `plum_codegen::intern_tags` — so there's no `.ll`-text assertion
    /// that could prove two tags stayed distinct; this inspects
    /// `MonoPlan::tag_fields`'s keys directly instead).
    fn mono_tags(src: &str) -> std::collections::HashSet<String> {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let program = with_prelude(program);
        let type_ctx = TypeContext::from_items(&program.items).unwrap_or_else(|e| panic!("context error: {e}"));
        let mut infer = Infer::with_context(type_ctx);
        let types = infer.infer_program(&program).unwrap_or_else(|e| panic!("type error: {e}"));
        let resolved_sites = infer.resolve_generic_sites().unwrap_or_else(|e| panic!("resolve error: {e}"));
        let type_ctx2 = TypeContext::from_items(&program.items).unwrap();
        let mono_plan = plum_ir::monomorphize::plan(
            &program,
            &type_ctx2,
            &resolved_sites,
            infer.fn_generics(),
            &types,
            infer.field_owners(),
            infer.array_for_loops(),
        )
        .unwrap_or_else(|e| panic!("monomorphization error: {e}"));
        mono_plan.tag_fields.into_keys().collect()
    }

    #[test]
    fn generic_struct_single_instantiation_compiles_and_runs() {
        let src = "\
            struct Pair[A, B] { first: A, second: B }\n\
            let go (): Int = { let p = Pair { first: 3, second: 4 }; p.first + p.second }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn a_generic_recursive_enum_instantiated_at_two_concrete_types_produces_two_distinct_tags() {
        // The direct mangling-collision-avoidance proof: `List[Int]`
        // and `List[Bool]` must each get their OWN `Cons$..`/`Nil$..`
        // tags, not share one `Cons`/`Nil` pair — see `mono_tags`'s
        // doc comment for why this is checked via `MonoPlan::tag_fields`
        // directly rather than `.ll` text.
        let src = "\
            enum List[T] { Cons(T, List[T]), Nil }\n\
            let sum_int (lst: List[Int]): Int = match lst {\n\
                Cons(h, t) => h + sum_int(t),\n\
                Nil => 0,\n\
            }\n\
            let count_true (lst: List[Bool]): Int = match lst {\n\
                Cons(h, t) => (if h { 1 } else { 0 }) + count_true(t),\n\
                Nil => 0,\n\
            }\n\
            let go (): Int = sum_int(Cons(1, Cons(2, Nil))) + count_true(Cons(true, Cons(false, Cons(true, Nil))))\n\
        ";
        let tags = mono_tags(src);
        assert!(tags.contains("Cons$Int"), "tags: {tags:?}");
        assert!(tags.contains("Cons$Bool"), "tags: {tags:?}");
        assert!(tags.contains("Nil$Int"), "tags: {tags:?}");
        assert!(tags.contains("Nil$Bool"), "tags: {tags:?}");
        assert!(!tags.contains("Cons"));
        assert!(!tags.contains("Nil"));

        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "5");
    }

    #[test]
    fn a_generic_function_called_at_multiple_concrete_types_produces_two_distinct_definitions() {
        let src = "\
            let identity[T] (x: T): T = x\n\
            let go (): Int = { let n = identity(5); let b = identity(true); n + (if b { 1 } else { 0 }) }\n\
        ";
        let (_body_ir, signatures, _entry) = compile_to_ir(src, "go").unwrap();
        assert!(signatures.contains_key("identity$Int"), "signatures: {:?}", signatures.keys());
        assert!(signatures.contains_key("identity$Bool"), "signatures: {:?}", signatures.keys());
        assert!(!signatures.contains_key("identity"));

        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn nested_generic_calling_generic_monomorphizes_correctly() {
        // Exercises the tier-2 template/worklist propagation
        // specifically: `wrap`'s own body constructs `Box[T]` from ITS
        // OWN still-generic `T`, never pinned to anything concrete
        // until `go` actually calls `wrap(5)`.
        let src = "\
            struct Box[T] { val: T }\n\
            let wrap[T] (x: T): Box[T] = Box { val: x }\n\
            let go (): Int = wrap(41).val + 1\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn a_recursive_generic_function_monomorphizes_and_terminates() {
        // Proves self-recursive monomorphization terminates (the
        // worklist's "already specialized" guard makes repeated calls
        // to `len[Int]` a no-op re-lookup, not infinite re-expansion)
        // AND is correct end to end.
        let src = "\
            enum List[T] { Cons(T, List[T]), Nil }\n\
            let len[T] (lst: List[T]): Int = match lst {\n\
                Cons(h, t) => 1 + len(t),\n\
                Nil => 0,\n\
            }\n\
            let go (): Int = len(Cons(1, Cons(2, Cons(3, Nil))))\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn instantiating_a_generic_type_at_an_unsupported_concrete_type_is_a_clear_error() {
        let src = "\
            struct Box[T] { val: T }\n\
            let go (): Int = { let b = Box { val: \"hi\" }; 0 }\n\
        ";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected an unsupported-instantiation error");
        assert!(
            err.contains("monomorphization") || err.contains("outside codegen's supported scope"),
            "unexpected error: {err}"
        );
    }

    // --- strings and arrays: real compile-and-run tests ---

    #[test]
    fn string_literal_concat_and_to_string_print_correctly() {
        // Exercises a string LITERAL, `.concat()`, `Int::to_string()`,
        // and a further `.concat()` of the result, all the way through
        // to the compiled entry point's own `Str`-printing path
        // (`emit_main`'s new `CgType::Str` case).
        let src = "\
            let go (): String = { let n = 42; \"answer: \".concat(n.to_string()) }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "answer: 42");
    }

    #[test]
    fn array_built_via_literal_push_and_set_sums_correctly_via_recursive_indexing() {
        // No `for` iteration yet (general array iteration is explicitly
        // deferred — see DESIGN.md/the plan) — summed via a small
        // TAIL-recursive direct-indexing wrapper instead, exactly the
        // workaround the plan calls for.
        let src = "\
            let sum_from (a: Array[Int]) (i: Int) (acc: Int): Int = \
                if i == a.len() { acc } else { sum_from(a, i + 1, acc + a[i]) }\n\
            let go (): Int = { \
                let a = [1, 2].push(3).set(0, 10); \
                sum_from(a, 0, 0) \
            }\n\
        ";
        // a = [1, 2, 3] after push, then set(0, 10) -> [10, 2, 3] -> sum 15
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "15");
    }

    #[test]
    fn array_push_reuse_is_correct_end_to_end_on_a_real_reuse_eligible_program() {
        // `grow`'s `a.push(v)` is exactly FBIP's reuse-in-place shape
        // (bare-`Var` receiver, uniquely owned by the time it's called
        // here) — `plum-codegen`'s own unit test already verifies the
        // reuse-vs-fresh branch SHAPE structurally; this proves it
        // actually EXECUTES correctly end to end.
        let src = "\
            let grow (a: Array[Int]) (v: Int): Array[Int] = a.push(v)\n\
            let sum_from (a: Array[Int]) (i: Int) (acc: Int): Int = \
                if i == a.len() { acc } else { sum_from(a, i + 1, acc + a[i]) }\n\
            let go (): Int = { let a = grow([1, 2, 3], 4); sum_from(a, 0, 0) }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "10");
    }

    #[test]
    fn nested_heap_array_elements_are_refcounted_correctly_when_popped() {
        // `Array[List[Int]]` — proves `@plum_rc_dec_array_Heap` (the
        // array release function for a HEAP-shaped, tag-dispatched
        // element type) composes correctly with the existing `@plum_rc_
        // dec`/`@plum_release_fields` machinery: popping one off leaves
        // a SHORTER array that still needs every SURVIVING element
        // correctly incremented (see `inc_copied_array_elements`), and
        // the popped-off list's own refcount must still end up correct
        // (accessed independently below, proving it wasn't
        // double-freed/leaked into an inconsistent state).
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let sum (lst: List): Int = match lst {\n\
                Cons(h, t) => h + sum(t),\n\
                Nil => 0,\n\
            }\n\
            let go (): Int = {\n\
                let a = [Cons(1, Nil), Cons(2, Nil), Cons(3, Nil)];\n\
                let popped = a.pop();\n\
                sum(popped[0]) + sum(popped[1]) + sum(a[2])\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn a_runtime_array_bounds_check_failure_exits_non_zero() {
        // `a[10]` on a length-3 array — a RUNTIME-checked failure (see
        // `plum_codegen::codegen`'s `emit_runtime_check`/`@plum_abort`),
        // never a silent wraparound or a Rust-level panic. Asserted via
        // the compiled BINARY's own non-zero exit status, not a
        // `Result::Err` — codegen itself succeeds; it's the COMPILED
        // PROGRAM that fails at runtime.
        let src = "let go (): Int = { let a = [1, 2, 3]; a[10] }";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected the compiled binary to exit non-zero");
        assert!(err.contains("non-zero"), "unexpected error: {err}");
    }
}
