mod codegen;

use plum_ir::ir;
use std::collections::HashMap;

/// LLVM IR type a Plum value maps to, for the narrow v1 codegen scope
/// (scalars + control flow + tail calls — see DESIGN.md's
/// "Implementation plan" section). Deliberately NOT `plum_types::Type`
/// — that would pull a `plum-types` dependency in for four primitive
/// cases; `plum-codegen` stays self-contained and testable in
/// isolation with hand-built `ir::Program` values, matching every
/// other crate in this workspace. `Unit` maps to `i1` (an unused
/// placeholder bit) purely so a `()`-pattern function parameter (see
/// `lower.rs`'s `__unit_paramN` synthetic name) still has SOME LLVM
/// type to declare — no Plum expression in this v1 scope ever
/// produces a meaningful `Unit` VALUE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgType {
    Int,
    Float,
    Bool,
    Unit,
}

impl CgType {
    fn llvm_type(self) -> &'static str {
        match self {
            CgType::Int => "i64",
            CgType::Float => "double",
            CgType::Bool | CgType::Unit => "i1",
        }
    }
}

/// A top-level function's concrete signature — `ir::Function` itself
/// carries no type information at all (`ir::Type` is vestigial/unused;
/// confirmed via grep before writing this crate), so the caller
/// (`plumc`) is responsible for deriving this from `plum_types::
/// Infer::infer_program`'s own results and handing it to
/// `emit_program`. Every name codegen calls (as a callee, not just the
/// function currently being emitted) must have an entry here, or
/// codegen reports a clear "unknown function" error rather than
/// guessing.
#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<CgType>,
    pub ret: CgType,
}

/// Emits an entire program as LLVM IR TEXT (the `.ll` format) — no
/// LLVM Rust binding involved at all (see DESIGN.md's "Implementation
/// plan" section for why: this machine has no `llvm-config`/dev
/// headers installed, and text + shelling to `clang` is also more
/// self-hosting-friendly than binding to a version-specific LLVM C
/// API). `signatures` must contain an entry for every function
/// `program.functions` calls (including itself, for recursion) —
/// see `FnSig`'s doc comment.
///
/// v1 scope (see DESIGN.md): scalars (`Int`/`Float`/`Bool`/`Unit`),
/// `Var`, `Unary`, `Binary` (including short-circuit `&&`/`||`),
/// `Let`, `If`, and plain named `Call` — with any tail call (self- or
/// mutual-recursive) compiled to a `musttail call` immediately
/// followed by `ret`, LLVM's portable guaranteed-tail-call-elimination
/// mechanism. `program.globals`/`program.externs` and every other
/// `ir::Expr` variant (heap/`Ctor`/`Match`, strings, arrays, closures,
/// concurrency, FFI, ...) are out of scope for now and produce a
/// clear error naming what's missing, never a panic — see this
/// crate's tests for the exact error shapes.
pub fn emit_program(program: &ir::Program, signatures: &HashMap<String, FnSig>) -> Result<String, String> {
    if !program.globals.is_empty() {
        return Err("codegen does not yet support top-level globals (v1 scope is functions only)".to_string());
    }
    if !program.externs.is_empty() {
        return Err("codegen does not yet support extern \"C\" functions (v1 scope has no FFI)".to_string());
    }
    let mut out = String::new();
    for f in &program.functions {
        out.push_str(&emit_function(f, signatures)?);
        out.push('\n');
    }
    Ok(out)
}

fn emit_function(f: &ir::Function, signatures: &HashMap<String, FnSig>) -> Result<String, String> {
    let sig = signatures
        .get(&f.name)
        .ok_or_else(|| format!("codegen: no signature known for function {:?}", f.name))?
        .clone();
    if sig.params.len() != f.params.len() {
        return Err(format!(
            "codegen: function {:?} has {} parameter(s) in the IR but {} in its signature",
            f.name,
            f.params.len(),
            sig.params.len()
        ));
    }

    let mut env = HashMap::new();
    let mut param_decls = Vec::with_capacity(f.params.len());
    for (name, ty) in f.params.iter().zip(&sig.params) {
        env.insert(name.clone(), (format!("%{name}"), *ty));
        param_decls.push(format!("{} %{name}", ty.llvm_type()));
    }

    let mut em = codegen::Emitter::new();
    let result = codegen::codegen_expr(&f.body, &env, &mut em, signatures, true)?;
    if result.is_some() {
        return Err(format!(
            "internal codegen error: function {:?}'s body did not terminate with a `ret` in tail position",
            f.name
        ));
    }

    let mut out = format!("define {} @{}({}) {{\n", sig.ret.llvm_type(), f.name, param_decls.join(", "));
    for line in &em.lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_ir::ir::{BinOp, Expr, Function, Program, UnOp};

    fn sigs(entries: &[(&str, Vec<CgType>, CgType)]) -> HashMap<String, FnSig> {
        entries
            .iter()
            .map(|(name, params, ret)| (name.to_string(), FnSig { params: params.clone(), ret: *ret }))
            .collect()
    }

    fn program(functions: Vec<Function>) -> Program {
        Program { functions, globals: vec![], externs: vec![] }
    }

    #[test]
    fn emits_a_define_with_correct_signature() {
        let prog = program(vec![Function {
            name: "double".to_string(),
            params: vec!["n".to_string()],
            body: Expr::Binary(BinOp::Mul, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(2))),
        }]);
        let ir = emit_program(&prog, &sigs(&[("double", vec![CgType::Int], CgType::Int)])).unwrap();
        assert!(ir.contains("define i64 @double(i64 %n) {"), "{ir}");
        assert!(ir.contains("mul i64 %n, 2"), "{ir}");
        assert!(ir.contains("ret i64"), "{ir}");
    }

    #[test]
    fn self_recursive_tail_call_becomes_musttail() {
        // let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }
        let body = Expr::If {
            cond: Box::new(Expr::Binary(BinOp::Eq, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(0)))),
            then_branch: Box::new(Expr::Var("acc".to_string())),
            else_branch: Box::new(Expr::Call {
                callee: Box::new(Expr::Var("sum".to_string())),
                args: vec![
                    Expr::Binary(BinOp::Sub, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(1))),
                    Expr::Binary(BinOp::Add, Box::new(Expr::Var("acc".to_string())), Box::new(Expr::Var("n".to_string()))),
                ],
            }),
        };
        let prog = program(vec![Function {
            name: "sum".to_string(),
            params: vec!["n".to_string(), "acc".to_string()],
            body,
        }]);
        let ir = emit_program(&prog, &sigs(&[("sum", vec![CgType::Int, CgType::Int], CgType::Int)])).unwrap();
        assert!(ir.contains("musttail call i64 @sum"), "{ir}");
        // The `ret` must be the VERY NEXT instruction after the
        // `musttail call` — the exact shape LLVM's musttail requires.
        let call_idx = ir.find("musttail call").unwrap();
        let after_call = &ir[call_idx..];
        let call_line_end = after_call.find('\n').unwrap();
        let next_line = after_call[call_line_end + 1..].lines().next().unwrap();
        assert!(next_line.trim_start().starts_with("ret "), "expected ret immediately after musttail call, got: {next_line:?}");
    }

    #[test]
    fn mutual_tail_call_becomes_musttail() {
        // let is_even n = if n == 0 { true } else { is_odd(n - 1) }
        // let is_odd n = if n == 0 { false } else { is_even(n - 1) }
        let mk = |self_ret: bool, other: &str| Function {
            name: if self_ret { "is_even" } else { "is_odd" }.to_string(),
            params: vec!["n".to_string()],
            body: Expr::If {
                cond: Box::new(Expr::Binary(BinOp::Eq, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(0)))),
                then_branch: Box::new(Expr::Bool(self_ret)),
                else_branch: Box::new(Expr::Call {
                    callee: Box::new(Expr::Var(other.to_string())),
                    args: vec![Expr::Binary(BinOp::Sub, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(1)))],
                }),
            },
        };
        let prog = program(vec![mk(true, "is_odd"), mk(false, "is_even")]);
        let ir = emit_program(
            &prog,
            &sigs(&[
                ("is_even", vec![CgType::Int], CgType::Bool),
                ("is_odd", vec![CgType::Int], CgType::Bool),
            ]),
        )
        .unwrap();
        assert!(ir.contains("musttail call i1 @is_odd"), "{ir}");
        assert!(ir.contains("musttail call i1 @is_even"), "{ir}");
    }

    #[test]
    fn non_tail_call_is_an_ordinary_call() {
        // let go n = double(n) + 1 — the call is NOT in tail position.
        let prog = program(vec![
            Function {
                name: "double".to_string(),
                params: vec!["n".to_string()],
                body: Expr::Binary(BinOp::Mul, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(2))),
            },
            Function {
                name: "go".to_string(),
                params: vec!["n".to_string()],
                body: Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Call { callee: Box::new(Expr::Var("double".to_string())), args: vec![Expr::Var("n".to_string())] }),
                    Box::new(Expr::Int(1)),
                ),
            },
        ]);
        let ir = emit_program(
            &prog,
            &sigs(&[("double", vec![CgType::Int], CgType::Int), ("go", vec![CgType::Int], CgType::Int)]),
        )
        .unwrap();
        let go_start = ir.find("define i64 @go").unwrap();
        let go_body = &ir[go_start..];
        assert!(go_body.contains("call i64 @double"), "{go_body}");
        assert!(!go_body.contains("musttail"), "{go_body}");
    }

    #[test]
    fn if_produces_a_phi_when_not_in_tail_position() {
        // let go n = (if n > 0 { 1 } else { -1 }) + 10
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["n".to_string()],
            body: Expr::Binary(
                BinOp::Add,
                Box::new(Expr::If {
                    cond: Box::new(Expr::Binary(BinOp::Gt, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(0)))),
                    then_branch: Box::new(Expr::Int(1)),
                    else_branch: Box::new(Expr::Unary(UnOp::Neg, Box::new(Expr::Int(1)))),
                }),
                Box::new(Expr::Int(10)),
            ),
        }]);
        let ir = emit_program(&prog, &sigs(&[("go", vec![CgType::Int], CgType::Int)])).unwrap();
        assert!(ir.contains(" = phi i64 "), "{ir}");
    }

    #[test]
    fn short_circuit_and_uses_branching_not_a_plain_and_instruction() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: Expr::Binary(BinOp::And, Box::new(Expr::Var("a".to_string())), Box::new(Expr::Var("b".to_string()))),
        }]);
        let ir = emit_program(&prog, &sigs(&[("go", vec![CgType::Bool, CgType::Bool], CgType::Bool)])).unwrap();
        assert!(ir.contains("br i1 %a"), "{ir}");
        assert!(ir.contains(" = phi i1 "), "{ir}");
        assert!(!ir.contains(" and i1 "), "{ir}");
    }

    #[test]
    fn unsupported_construct_is_a_clear_error_not_a_panic() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Ctor { tag: "Point".to_string(), fields: vec![] },
        }]);
        let err = emit_program(&prog, &sigs(&[("go", vec![], CgType::Unit)])).expect_err("expected a clear error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_global_is_rejected_with_a_clear_error() {
        let prog = Program {
            functions: vec![],
            globals: vec![plum_ir::ir::Global { name: "x".to_string(), value: Expr::Int(1) }],
            externs: vec![],
        };
        let err = emit_program(&prog, &HashMap::new()).expect_err("expected globals to be rejected");
        assert!(err.contains("globals"), "unexpected error: {err}");
    }

    #[test]
    fn a_call_through_a_computed_callee_is_rejected() {
        // codegen only supports a direct, bare function-name callee —
        // not the result of some other expression (no first-class
        // function values in this v1 subset).
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Call { callee: Box::new(Expr::Int(0)), args: vec![] },
        }]);
        let err = emit_program(&prog, &sigs(&[("go", vec![], CgType::Unit)]))
            .expect_err("expected a computed callee to be rejected");
        assert!(err.contains("directly-named function"), "unexpected error: {err}");
    }

    #[test]
    fn a_call_to_an_unknown_function_is_rejected() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Call { callee: Box::new(Expr::Var("nope".to_string())), args: vec![] },
        }]);
        let err = emit_program(&prog, &sigs(&[("go", vec![], CgType::Unit)]))
            .expect_err("expected an unknown callee to be rejected");
        assert!(err.contains("unknown function"), "unexpected error: {err}");
    }
}
