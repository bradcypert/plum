//! The actual `ir::Expr` -> LLVM IR text walk. See lib.rs's `emit_program`
//! doc comment for v1 scope. Everything here works over `String`
//! instruction lines — no LLVM binding, no typed IR builder, just text
//! (see DESIGN.md's "Implementation plan" for why: this project shells
//! out to `clang` to compile the emitted `.ll` rather than binding to
//! LLVM's C API directly).
//!
//! # Tail-position handling
//!
//! `codegen_expr`'s `tail: bool` parameter is the whole guaranteed-
//! tail-call-elimination story: `tail=true` means "the caller needs
//! THIS expression's value returned directly, with a `ret` — there is
//! no more Plum code between this expression's result and the
//! function actually returning." When `tail` is true, `codegen_expr`
//! is responsible for emitting the function's actual terminator
//! (`ret`, or `musttail call` + `ret` for a Call) and returns `Ok(None)`
//! — there's no SSA value left for a caller to consume, since control
//! flow ends here. When `tail` is false, it returns `Ok(Some((reg,
//! ty)))`, an ordinary value the caller keeps computing with.
//!
//! The recursive tail-position RULE (which sub-expressions inherit
//! `tail` from their parent) is: a function's whole body; both
//! branches of an `If` that is itself in tail position; a `Let`'s
//! `body` (never its `value`). Nothing else is ever a tail position —
//! `Binary`/`Unary` operands, `Call` arguments, and `If`'s `cond` are
//! always evaluated via `codegen_value` (implicitly `tail=false`).

use crate::{CgType, FnSig};
use plum_ir::ir::{BinOp, Expr, UnOp};
use std::collections::HashMap;

type Env = HashMap<String, (String, CgType)>;
type Sigs = HashMap<String, FnSig>;

/// Accumulates a function body's instructions as flat text lines (each
/// LLVM basic block is just a `"label:"` line followed by its
/// instructions — textual order doesn't need to match control-flow
/// order beyond "a block's own instructions appear together, ending in
/// its own terminator," which every code path here maintains). Starts
/// pre-seeded with the mandatory `entry:` label.
pub(crate) struct Emitter {
    next_id: usize,
    pub(crate) lines: Vec<String>,
    current_block: String,
}

impl Emitter {
    pub(crate) fn new() -> Self {
        Emitter {
            next_id: 0,
            lines: vec!["entry:".to_string()],
            current_block: "entry".to_string(),
        }
    }

    fn fresh_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// A fresh SSA virtual register name — always `%v<N>`, never
    /// reusing a Plum source name directly (only function PARAMETERS
    /// get named registers, seeded once in `lib.rs::emit_function`),
    /// so a shadowing `Let` (`let n = n + 1`) can never collide with
    /// an already-defined register: it just gets its OWN fresh one,
    /// with the Plum name `"n"` remapped to it going forward in `Env`
    /// — ordinary, valid SSA (the old register simply goes unused
    /// after the shadow, not redefined).
    fn fresh_reg(&mut self) -> String {
        format!("%v{}", self.fresh_id())
    }

    /// A fresh, uniquely-suffixed block label — `hint` is purely for
    /// human readability in the emitted `.ll` (`"then7"`, `"merge12"`,
    /// ...), sharing the same counter as `fresh_reg` costs nothing
    /// (labels and `%`-registers are separate LLVM namespaces, so
    /// there's no collision risk either way — this is just for unique
    /// Rust-side bookkeeping).
    fn fresh_label(&mut self, hint: &str) -> String {
        format!("{hint}{}", self.fresh_id())
    }

    fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// Starts a new basic block: pushes its label line and updates
    /// `current_block` — callers doing multi-block control flow (`If`,
    /// short-circuit `&&`/`||`) must capture `current_block` again
    /// AFTER codegen'ing a sub-expression that might itself have
    /// opened further nested blocks, since that's the real block a
    /// `phi` needs to name as its predecessor, not necessarily the
    /// block this function just started.
    fn start_block(&mut self, label: &str) {
        self.push(format!("{label}:"));
        self.current_block = label.to_string();
    }
}

fn format_double(f: f64) -> String {
    // LLVM's decimal float-constant parser only accepts values that
    // round-trip EXACTLY through its internal parsing — an ordinary
    // `format!("{f}")` can silently fail to parse for values that
    // don't happen to round-trip. The hex-float form (`0x` + the raw
    // 64-bit IEEE754 bit pattern) always round-trips exactly, so it's
    // used unconditionally rather than only as a fallback.
    format!("0x{:016X}", f.to_bits())
}

fn expect_direct_callee(callee: &Expr) -> Result<&str, String> {
    match callee {
        Expr::Var(name) => Ok(name),
        other => Err(format!(
            "codegen requires a call to a directly-named function (found {other:?}) — closures and other \
             first-class function values aren't supported yet"
        )),
    }
}

fn codegen_call_args(
    args: &[Expr],
    env: &Env,
    em: &mut Emitter,
    sigs: &Sigs,
    sig: &FnSig,
    name: &str,
) -> Result<String, String> {
    if args.len() != sig.params.len() {
        return Err(format!("{name:?} expects {} argument(s), found {}", sig.params.len(), args.len()));
    }
    let mut parts = Vec::with_capacity(args.len());
    for (arg, expected_ty) in args.iter().zip(&sig.params) {
        let (reg, ty) = codegen_value(arg, env, em, sigs)?;
        if ty != *expected_ty {
            return Err(format!("{name:?}: argument type mismatch — expected {expected_ty:?}, found {ty:?}"));
        }
        parts.push(format!("{} {reg}", ty.llvm_type()));
    }
    Ok(parts.join(", "))
}

fn codegen_binop(op: BinOp, l: String, r: String, ty: CgType, em: &mut Emitter) -> Result<(String, CgType), String> {
    let (instr, result_ty) = match (op, ty) {
        (BinOp::Add, CgType::Int) => ("add i64", CgType::Int),
        (BinOp::Sub, CgType::Int) => ("sub i64", CgType::Int),
        (BinOp::Mul, CgType::Int) => ("mul i64", CgType::Int),
        (BinOp::Div, CgType::Int) => ("sdiv i64", CgType::Int),
        (BinOp::Rem, CgType::Int) => ("srem i64", CgType::Int),
        (BinOp::Add, CgType::Float) => ("fadd double", CgType::Float),
        (BinOp::Sub, CgType::Float) => ("fsub double", CgType::Float),
        (BinOp::Mul, CgType::Float) => ("fmul double", CgType::Float),
        (BinOp::Div, CgType::Float) => ("fdiv double", CgType::Float),
        (BinOp::Rem, CgType::Float) => ("frem double", CgType::Float),
        (BinOp::Eq, CgType::Int) => ("icmp eq i64", CgType::Bool),
        (BinOp::Ne, CgType::Int) => ("icmp ne i64", CgType::Bool),
        (BinOp::Lt, CgType::Int) => ("icmp slt i64", CgType::Bool),
        (BinOp::Gt, CgType::Int) => ("icmp sgt i64", CgType::Bool),
        (BinOp::Le, CgType::Int) => ("icmp sle i64", CgType::Bool),
        (BinOp::Ge, CgType::Int) => ("icmp sge i64", CgType::Bool),
        (BinOp::Eq, CgType::Bool) => ("icmp eq i1", CgType::Bool),
        (BinOp::Ne, CgType::Bool) => ("icmp ne i1", CgType::Bool),
        (BinOp::Eq, CgType::Float) => ("fcmp oeq double", CgType::Bool),
        (BinOp::Ne, CgType::Float) => ("fcmp one double", CgType::Bool),
        (BinOp::Lt, CgType::Float) => ("fcmp olt double", CgType::Bool),
        (BinOp::Gt, CgType::Float) => ("fcmp ogt double", CgType::Bool),
        (BinOp::Le, CgType::Float) => ("fcmp ole double", CgType::Bool),
        (BinOp::Ge, CgType::Float) => ("fcmp oge double", CgType::Bool),
        (op, ty) => return Err(format!("codegen: {op:?} is not supported for {ty:?} operands")),
    };
    let reg = em.fresh_reg();
    em.push(format!("  {reg} = {instr} {l}, {r}"));
    Ok((reg, result_ty))
}

/// `&&`/`||` — real branching, not a plain `and`/`or` instruction,
/// specifically to match the interpreter's short-circuit semantics
/// EXACTLY (see `plum-interp/src/lib.rs`'s own `Expr::Binary(BinOp::
/// And, ..)`/`Or` handling): the untaken side's code must never
/// execute, not just "the boolean result happens to be right." Always
/// produces an ordinary SSA value via its own internal merge block —
/// it never itself decides tail position (its caller, `codegen_value`,
/// is only ever invoked from a non-tail context).
fn codegen_and_or(op: BinOp, l: &Expr, r: &Expr, env: &Env, em: &mut Emitter, sigs: &Sigs) -> Result<(String, CgType), String> {
    let op_name = if op == BinOp::And { "&&" } else { "||" };
    let (l_reg, l_ty) = codegen_value(l, env, em, sigs)?;
    if l_ty != CgType::Bool {
        return Err(format!("`{op_name}` requires Bool operands, found {l_ty:?}"));
    }
    let short_label = em.fresh_label("sc_short");
    let rhs_label = em.fresh_label("sc_rhs");
    let merge_label = em.fresh_label("sc_merge");
    // `And`: false short-circuits (skip rhs); `Or`: true short-circuits.
    let short_value = if op == BinOp::And { "0" } else { "1" };
    if op == BinOp::And {
        em.push(format!("  br i1 {l_reg}, label %{rhs_label}, label %{short_label}"));
    } else {
        em.push(format!("  br i1 {l_reg}, label %{short_label}, label %{rhs_label}"));
    }

    em.start_block(&short_label);
    em.push(format!("  br label %{merge_label}"));
    let short_end_block = em.current_block.clone();

    em.start_block(&rhs_label);
    let (r_reg, r_ty) = codegen_value(r, env, em, sigs)?;
    if r_ty != CgType::Bool {
        return Err(format!("`{op_name}` requires Bool operands, found {r_ty:?}"));
    }
    em.push(format!("  br label %{merge_label}"));
    let rhs_end_block = em.current_block.clone();

    em.start_block(&merge_label);
    let phi_reg = em.fresh_reg();
    em.push(format!(
        "  {phi_reg} = phi i1 [ {short_value}, %{short_end_block} ], [ {r_reg}, %{rhs_end_block} ]"
    ));
    Ok((phi_reg, CgType::Bool))
}

/// Computes an ordinary SSA value for `expr` — used for every position
/// that is NEVER a tail position (operands, call arguments, `If`'s
/// `cond`, a `Let`'s `value`). `Let`/`If` themselves are still valid
/// here (an `if`/`let` can appear as an ordinary sub-expression, e.g.
/// `1 + if b { 2 } else { 3 }`) — delegated to `codegen_expr` with
/// `tail=false`, which is guaranteed to return `Some` in that mode.
fn codegen_value(expr: &Expr, env: &Env, em: &mut Emitter, sigs: &Sigs) -> Result<(String, CgType), String> {
    match expr {
        Expr::Int(n) => Ok((n.to_string(), CgType::Int)),
        Expr::Float(f) => Ok((format_double(*f), CgType::Float)),
        Expr::Bool(b) => Ok(((if *b { "1" } else { "0" }).to_string(), CgType::Bool)),
        Expr::Unit => Ok(("0".to_string(), CgType::Unit)),
        Expr::Var(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| format!("codegen: unbound variable {name:?}")),
        Expr::Unary(op, inner) => {
            let (reg, ty) = codegen_value(inner, env, em, sigs)?;
            match (op, ty) {
                (UnOp::Neg, CgType::Int) => {
                    let r = em.fresh_reg();
                    em.push(format!("  {r} = sub i64 0, {reg}"));
                    Ok((r, CgType::Int))
                }
                (UnOp::Neg, CgType::Float) => {
                    let r = em.fresh_reg();
                    em.push(format!("  {r} = fneg double {reg}"));
                    Ok((r, CgType::Float))
                }
                (UnOp::Not, CgType::Bool) => {
                    let r = em.fresh_reg();
                    em.push(format!("  {r} = xor i1 {reg}, 1"));
                    Ok((r, CgType::Bool))
                }
                (op, ty) => Err(format!("codegen: unary {op:?} is not supported for {ty:?}")),
            }
        }
        Expr::Binary(op, l, r) if *op == BinOp::And || *op == BinOp::Or => {
            codegen_and_or(op.clone(), l, r, env, em, sigs)
        }
        Expr::Binary(op, l, r) => {
            let (l_reg, l_ty) = codegen_value(l, env, em, sigs)?;
            let (r_reg, r_ty) = codegen_value(r, env, em, sigs)?;
            if l_ty != r_ty {
                return Err(format!("codegen: `{op:?}` operand type mismatch — {l_ty:?} vs {r_ty:?}"));
            }
            codegen_binop(op.clone(), l_reg, r_reg, l_ty, em)
        }
        Expr::Call { callee, args } => {
            let name = expect_direct_callee(callee)?.to_string();
            let sig = sigs
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("codegen: unknown function {name:?}"))?;
            let args_ir = codegen_call_args(args, env, em, sigs, &sig, &name)?;
            let reg = em.fresh_reg();
            em.push(format!("  {reg} = call {} @{name}({args_ir})", sig.ret.llvm_type()));
            Ok((reg, sig.ret))
        }
        Expr::Let { .. } | Expr::If { .. } => match codegen_expr(expr, env, em, sigs, false)? {
            Some(v) => Ok(v),
            None => unreachable!("codegen_expr with tail=false always returns Some"),
        },
        other => Err(format!("codegen does not yet support this construct: {other:?}")),
    }
}

/// See this module's doc comment for the full tail-position story.
pub(crate) fn codegen_expr(
    expr: &Expr,
    env: &Env,
    em: &mut Emitter,
    sigs: &Sigs,
    tail: bool,
) -> Result<Option<(String, CgType)>, String> {
    match expr {
        Expr::Let { name, value, body } => {
            let bound = codegen_value(value, env, em, sigs)?;
            let mut inner_env = env.clone();
            inner_env.insert(name.clone(), bound);
            codegen_expr(body, &inner_env, em, sigs, tail)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let (cond_reg, cond_ty) = codegen_value(cond, env, em, sigs)?;
            if cond_ty != CgType::Bool {
                return Err(format!("codegen: `if` condition must be Bool, found {cond_ty:?}"));
            }
            let then_label = em.fresh_label("then");
            let else_label = em.fresh_label("else");
            // Allocated up front (even though only used when `!tail`)
            // so both arms can reference it directly without needing a
            // second pass once both are known — see the (None, None)
            // vs (Some, Some) handling below for why this is safe to
            // allocate unconditionally (an allocated-but-unreferenced
            // label is just an unused Rust string, never emitted into
            // the IR at all when `tail` is true).
            let merge_label = em.fresh_label("merge");
            em.push(format!("  br i1 {cond_reg}, label %{then_label}, label %{else_label}"));

            em.start_block(&then_label);
            let then_result = codegen_expr(then_branch, env, em, sigs, tail)?;
            if then_result.is_some() {
                em.push(format!("  br label %{merge_label}"));
            }
            let then_end_block = em.current_block.clone();

            em.start_block(&else_label);
            let else_result = codegen_expr(else_branch, env, em, sigs, tail)?;
            if else_result.is_some() {
                em.push(format!("  br label %{merge_label}"));
            }
            let else_end_block = em.current_block.clone();

            match (then_result, else_result) {
                // Both arms share the same `tail` flag (inherited from
                // this `If` itself), so both always return `None`
                // (already emitted their own terminator) or both
                // always return `Some` (still need this `If` to merge
                // them) — never a mix.
                (None, None) => Ok(None),
                (Some((then_reg, then_ty)), Some((else_reg, else_ty))) => {
                    if then_ty != else_ty {
                        return Err(format!(
                            "codegen: `if` branches must agree on type, found {then_ty:?} and {else_ty:?}"
                        ));
                    }
                    em.start_block(&merge_label);
                    let phi_reg = em.fresh_reg();
                    em.push(format!(
                        "  {phi_reg} = phi {} [ {then_reg}, %{then_end_block} ], [ {else_reg}, %{else_end_block} ]",
                        then_ty.llvm_type()
                    ));
                    Ok(Some((phi_reg, then_ty)))
                }
                _ => unreachable!("both `if` arms share the same tail-ness"),
            }
        }
        Expr::Call { callee, args } if tail => {
            let name = expect_direct_callee(callee)?.to_string();
            let sig = sigs
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("codegen: unknown function {name:?}"))?;
            let args_ir = codegen_call_args(args, env, em, sigs, &sig, &name)?;
            let reg = em.fresh_reg();
            // The exact shape `musttail` requires: the call and the
            // `ret` are the ONLY two instructions, with nothing in
            // between — this is what gives LLVM's guaranteed,
            // portable tail-call elimination (the whole reason
            // DESIGN.md picked LLVM as the backend over compiling
            // through C, which has no such guarantee).
            em.push(format!("  {reg} = musttail call {} @{name}({args_ir})", sig.ret.llvm_type()));
            em.push(format!("  ret {} {reg}", sig.ret.llvm_type()));
            Ok(None)
        }
        _ => {
            let (reg, ty) = codegen_value(expr, env, em, sigs)?;
            if tail {
                em.push(format!("  ret {} {reg}", ty.llvm_type()));
                Ok(None)
            } else {
                Ok(Some((reg, ty)))
            }
        }
    }
}
