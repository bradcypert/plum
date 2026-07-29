//! The actual `ir::Expr` -> LLVM IR text walk. See lib.rs's `emit_program`
//! doc comment for supported scope. Everything here works over `String`
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
//! branches of an `If`/arms of a `Match` that are themselves in tail
//! position; a `Let`'s `body` and an `RcAnnotated`'s `rest` (never a
//! `Let`'s `value`, an `RcAnnotated`'s `target`, or a `Match`'s
//! `scrutinee`/guards). Nothing else is ever a tail position —
//! `Binary`/`Unary` operands and `Call` arguments are always evaluated
//! via `codegen_value` (implicitly `tail=false`).
//!
//! # Heap values (`Ctor`/`CtorReuse`/`RcAnnotated`/`Match`)
//!
//! Every heap cell — regardless of which Plum struct/enum-variant it
//! represents — shares ONE layout: `{ i64 refcount, i64 tag, i64
//! fields[N] }`, allocated via `@plum_alloc` (see `crate::runtime_ir`
//! for the four emitted runtime functions this all calls into). Every
//! field slot is a raw 64-bit word regardless of its OWN type —
//! `Int`/`Bool` stored directly (Bool zero-extended), `Float` via a
//! bit-preserving `bitcast` (not a numeric conversion), a nested heap
//! pointer via `ptrtoint`/`inttoptr` — see `store_field_word`/
//! `load_field_word`. This uniform-word scheme means codegen never
//! needs a distinct LLVM struct TYPE per Plum struct/enum: one generic
//! block shape works for everything, and "which fields are heap-
//! shaped" (`Ctx::tag_fields`) is the only per-tag information needed.
//!
//! `Match` dispatch and the runtime's own recursive-field-release
//! logic both use a plain sequential `icmp`+`br` chain, never an LLVM
//! `switch` — `Match`'s own semantics (arms tried in order, the SAME
//! tag may appear in more than one arm with different guards) don't
//! map cleanly onto `switch`'s one-label-per-case-value shape anyway,
//! and a chain is simplest to get right, consistent with how `If`/
//! short-circuit `&&`/`||` already work.

use crate::{CgType, FnSig};
use plum_ir::ir::{BinOp, Expr, MatchArm, RcOp, UnOp};
use std::collections::HashMap;

type Env = HashMap<String, (String, CgType)>;

/// The sentinel `MatchArm.tag` lowering uses for a catch-all arm
/// (`_`/bare-ident) mixed into an otherwise Ctor-tag-shaped match —
/// see `lower.rs`'s own `DEFAULT_ARM_TAG` doc comment for the full
/// "why a sentinel string, not a new IR field" reasoning. Duplicated
/// here rather than exported across the crate boundary, matching the
/// established precedent for this exact kind of cross-crate shape
/// constant elsewhere in this codebase (e.g. `plum-interp` keeps its
/// own copy rather than importing one from `plum-ir`).
const DEFAULT_ARM_TAG: &str = "0Default";

/// Everything `codegen_expr`/`codegen_value` need beyond the current
/// expression and local environment — bundled into one struct (rather
/// than three separate parameters threaded through every function)
/// once heap-value codegen needed two more tables alongside the
/// pre-existing function-signature one.
pub(crate) struct Ctx<'a> {
    pub(crate) sigs: &'a HashMap<String, FnSig>,
    /// The signature of the function CURRENTLY being emitted —
    /// constant for the whole body (unlike `tail`/the local `Env`),
    /// so it lives here rather than as a separate threaded parameter.
    /// Needed ONLY to decide whether a tail-position call can safely
    /// use `musttail`: LLVM requires the CALLER's own prototype to
    /// match the CALLEE's for `musttail` to be valid at all (a real
    /// constraint discovered via an actual `clang` compile failure —
    /// "cannot guarantee tail call due to mismatched parameter
    /// counts" — not something documented up front). A tail call to a
    /// function with a DIFFERENT signature than the current one (e.g.
    /// a zero-arg entry point tail-calling a two-arg accumulator
    /// function) falls back to an ordinary `call` + `ret` instead —
    /// still correct, just not a `musttail`-GUARANTEED elimination.
    pub(crate) caller_sig: &'a FnSig,
    /// Every known tag (struct name or enum variant name) -> its
    /// compile-time-interned small integer — see `crate::intern_tags`.
    pub(crate) tag_ids: &'a HashMap<String, i64>,
    /// Every known tag -> its fields' `CgType`s, in declared order —
    /// `crate::emit_program`'s caller derives this from `plum_types::
    /// TypeContext::struct_fields`/`variant`.
    pub(crate) tag_fields: &'a HashMap<String, Vec<CgType>>,
}

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
    /// short-circuit `&&`/`||`, `Match`) must capture `current_block`
    /// again AFTER codegen'ing a sub-expression that might itself have
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

fn codegen_call_args(args: &[Expr], env: &Env, em: &mut Emitter, ctx: &Ctx, sig: &FnSig, name: &str) -> Result<String, String> {
    if args.len() != sig.params.len() {
        return Err(format!("{name:?} expects {} argument(s), found {}", sig.params.len(), args.len()));
    }
    let mut parts = Vec::with_capacity(args.len());
    for (arg, expected_ty) in args.iter().zip(&sig.params) {
        let (reg, ty) = codegen_value(arg, env, em, ctx)?;
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
fn codegen_and_or(op: BinOp, l: &Expr, r: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let op_name = if op == BinOp::And { "&&" } else { "||" };
    let (l_reg, l_ty) = codegen_value(l, env, em, ctx)?;
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
    let (r_reg, r_ty) = codegen_value(r, env, em, ctx)?;
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

fn field_byte_offset(index: usize) -> i64 {
    // Header is 2 words (refcount, tag) = 16 bytes; each field slot is
    // one more 8-byte word after that.
    16 + (index as i64) * 8
}

/// Writes `value` (already computed, in its OWN native LLVM
/// representation — `i64`/`double`/`i1`/`ptr`) into field `index` of
/// the cell at `cell_ptr`, converting it to the uniform 64-bit word
/// representation every field slot uses — see this module's doc
/// comment.
fn store_field_word(em: &mut Emitter, cell_ptr: &str, index: usize, value: &str, ty: CgType) {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {cell_ptr}, i64 {}", field_byte_offset(index)));
    let word = match ty {
        CgType::Int => value.to_string(),
        CgType::Bool | CgType::Unit => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = zext i1 {value} to i64"));
            r
        }
        CgType::Float => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = bitcast double {value} to i64"));
            r
        }
        CgType::Heap => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = ptrtoint ptr {value} to i64"));
            r
        }
    };
    em.push(format!("  store i64 {word}, ptr {addr}"));
}

/// The inverse of `store_field_word` — reads field `index` of the cell
/// at `cell_ptr` back out, converting the raw word into `expected_ty`'s
/// own native LLVM representation.
fn load_field_word(em: &mut Emitter, cell_ptr: &str, index: usize, expected_ty: CgType) -> String {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {cell_ptr}, i64 {}", field_byte_offset(index)));
    let word = em.fresh_reg();
    em.push(format!("  {word} = load i64, ptr {addr}"));
    match expected_ty {
        CgType::Int => word,
        CgType::Bool | CgType::Unit => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = trunc i64 {word} to i1"));
            r
        }
        CgType::Float => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = bitcast i64 {word} to double"));
            r
        }
        CgType::Heap => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = inttoptr i64 {word} to ptr"));
            r
        }
    }
}

fn tag_field_types<'a>(ctx: &'a Ctx, tag: &str) -> Result<&'a [CgType], String> {
    ctx.tag_fields
        .get(tag)
        .map(|v| v.as_slice())
        .ok_or_else(|| format!("codegen: unknown tag {tag:?} (no struct/enum-variant declaration found)"))
}

fn tag_id(ctx: &Ctx, tag: &str) -> Result<i64, String> {
    ctx.tag_ids
        .get(tag)
        .copied()
        .ok_or_else(|| format!("codegen: unknown tag {tag:?} (no struct/enum-variant declaration found)"))
}

/// `Expr::Ctor{tag, fields}` — always an ordinary value (never itself a
/// tail position, same as any other allocation); shared by both the
/// plain-`Ctor` codegen path and `CtorReuse`'s own "refcount wasn't 1,
/// fall back to a fresh allocation" branch.
fn codegen_ctor_alloc(tag: &str, field_vals: &[(String, CgType)], em: &mut Emitter, ctx: &Ctx) -> Result<String, String> {
    let id = tag_id(ctx, tag)?;
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc(i64 {id}, i64 {})", field_vals.len()));
    for (i, (reg, ty)) in field_vals.iter().enumerate() {
        store_field_word(em, &cell, i, reg, *ty);
    }
    Ok(cell)
}

fn codegen_ctor_fields(tag: &str, fields: &[Expr], env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<Vec<(String, CgType)>, String> {
    let field_types = tag_field_types(ctx, tag)?.to_vec();
    if field_types.len() != fields.len() {
        return Err(format!(
            "codegen: constructor {tag:?} expects {} field(s), found {}",
            field_types.len(),
            fields.len()
        ));
    }
    let mut vals = Vec::with_capacity(fields.len());
    for (i, (fexpr, expected)) in fields.iter().zip(&field_types).enumerate() {
        let (reg, ty) = codegen_value(fexpr, env, em, ctx)?;
        if ty != *expected {
            return Err(format!("codegen: constructor {tag:?} field {i}: expected {expected:?}, found {ty:?}"));
        }
        vals.push((reg, ty));
    }
    Ok(vals)
}

/// Computes an ordinary SSA value for `expr` — used for every position
/// that is NEVER a tail position (operands, call arguments, `If`'s
/// `cond`, a `Let`'s `value`, `Match`'s `scrutinee`/guards, an
/// `RcAnnotated`'s `target` lookup). `Let`/`If`/`Match` themselves are
/// still valid here (e.g. `1 + if b { 2 } else { 3 }`) — delegated to
/// `codegen_expr` with `tail=false`, which is guaranteed to return
/// `Some` in that mode.
fn codegen_value(expr: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
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
            let (reg, ty) = codegen_value(inner, env, em, ctx)?;
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
        Expr::Binary(op, l, r) if *op == BinOp::And || *op == BinOp::Or => codegen_and_or(op.clone(), l, r, env, em, ctx),
        Expr::Binary(op, l, r) => {
            let (l_reg, l_ty) = codegen_value(l, env, em, ctx)?;
            let (r_reg, r_ty) = codegen_value(r, env, em, ctx)?;
            if l_ty != r_ty {
                return Err(format!("codegen: `{op:?}` operand type mismatch — {l_ty:?} vs {r_ty:?}"));
            }
            codegen_binop(op.clone(), l_reg, r_reg, l_ty, em)
        }
        Expr::Call { callee, args } => {
            let name = expect_direct_callee(callee)?.to_string();
            let sig = ctx.sigs.get(&name).cloned().ok_or_else(|| format!("codegen: unknown function {name:?}"))?;
            let args_ir = codegen_call_args(args, env, em, ctx, &sig, &name)?;
            let reg = em.fresh_reg();
            em.push(format!("  {reg} = call {} @{name}({args_ir})", sig.ret.llvm_type()));
            Ok((reg, sig.ret))
        }
        Expr::Ctor { tag, fields } => {
            let vals = codegen_ctor_fields(tag, fields, env, em, ctx)?;
            let cell = codegen_ctor_alloc(tag, &vals, em, ctx)?;
            Ok((cell, CgType::Heap))
        }
        Expr::CtorReuse { reuse_of, tag, fields } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            if old_ty != CgType::Heap {
                return Err(format!("codegen: internal error — CtorReuse target {reuse_of:?} is not heap-shaped"));
            }
            let field_vals = codegen_ctor_fields(tag, fields, env, em, ctx)?;
            let id = tag_id(ctx, tag)?;

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("reuse");
            let alloc_label = em.fresh_label("reuse_alloc_fresh");
            let merge_label = em.fresh_label("reuse_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            // Release whatever the OLD cell used to hold (recursively
            // dec any heap-shaped field) WITHOUT calling `free` — its
            // memory is about to be reused in place, not returned to
            // the allocator.
            em.push(format!("  call void @plum_release_fields(ptr {old_ptr})"));
            let tag_addr = em.fresh_reg();
            em.push(format!("  {tag_addr} = getelementptr i8, ptr {old_ptr}, i64 8"));
            em.push(format!("  store i64 {id}, ptr {tag_addr}"));
            for (i, (reg, ty)) in field_vals.iter().enumerate() {
                store_field_word(em, &old_ptr, i, reg, *ty);
            }
            em.push(format!("  store i64 1, ptr {old_ptr}"));
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = codegen_ctor_alloc(tag, &field_vals, em, ctx)?;
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {old_ptr}, %{reuse_label} ], [ {fresh}, %{alloc_label} ]"));
            Ok((result, CgType::Heap))
        }
        Expr::Let { .. } | Expr::If { .. } | Expr::Match { .. } | Expr::RcAnnotated { .. } => {
            match codegen_expr(expr, env, em, ctx, false)? {
                Some(v) => Ok(v),
                None => unreachable!("codegen_expr with tail=false always returns Some"),
            }
        }
        other => Err(format!("codegen does not yet support this construct: {other:?}")),
    }
}

/// Starts binding `arm` for `scrutinee_ptr` (already known to have
/// `arm`'s tag): returns a fresh copy of `env` for the arm's own scope
/// to extend. The `DEFAULT_ARM_TAG` catch-all case is special-cased
/// here in full — its (at most one) binding names the WHOLE scrutinee
/// value directly, not an extracted field, so there's nothing further
/// for the caller to bind. For an ordinary tag, this only validates
/// arity; the caller (`codegen_expr`'s `Match` case) does the actual
/// per-field extraction — see that code for why: it needs the field
/// TYPES to load each slot correctly, and already has `ctx` in scope
/// there too, so there's nothing this function could usefully do that
/// the caller doesn't already need to do itself.
fn bind_match_arm(arm: &MatchArm, scrutinee_ptr: &str, env: &Env, ctx: &Ctx) -> Result<Env, String> {
    let mut arm_env = env.clone();
    if arm.tag == DEFAULT_ARM_TAG {
        if let Some(name) = arm.bindings.first() {
            arm_env.insert(name.clone(), (scrutinee_ptr.to_string(), CgType::Heap));
        }
        return Ok(arm_env);
    }
    let field_types = tag_field_types(ctx, &arm.tag)?;
    if field_types.len() != arm.bindings.len() {
        return Err(format!(
            "codegen: match arm for {:?} expects {} binding(s), found {}",
            arm.tag,
            field_types.len(),
            arm.bindings.len()
        ));
    }
    Ok(arm_env)
}

/// See this module's doc comment for the full tail-position story.
pub(crate) fn codegen_expr(expr: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx, tail: bool) -> Result<Option<(String, CgType)>, String> {
    match expr {
        Expr::Let { name, value, body } => {
            let bound = codegen_value(value, env, em, ctx)?;
            let mut inner_env = env.clone();
            inner_env.insert(name.clone(), bound);
            codegen_expr(body, &inner_env, em, ctx, tail)
        }
        Expr::RcAnnotated { op, target, rest } => {
            let (reg, ty) = env.get(target).cloned().ok_or_else(|| format!("codegen: unbound variable {target:?}"))?;
            if ty != CgType::Heap {
                return Err(format!(
                    "codegen: internal error — RcAnnotated target {target:?} is not heap-shaped ({ty:?})"
                ));
            }
            match op {
                RcOp::Inc => em.push(format!("  call void @plum_rc_inc(ptr {reg})")),
                RcOp::Dec => em.push(format!("  call void @plum_rc_dec(ptr {reg})")),
            }
            codegen_expr(rest, env, em, ctx, tail)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let (cond_reg, cond_ty) = codegen_value(cond, env, em, ctx)?;
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
            let then_result = codegen_expr(then_branch, env, em, ctx, tail)?;
            if then_result.is_some() {
                em.push(format!("  br label %{merge_label}"));
            }
            let then_end_block = em.current_block.clone();

            em.start_block(&else_label);
            let else_result = codegen_expr(else_branch, env, em, ctx, tail)?;
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
        Expr::Match { scrutinee, arms } => {
            let (scrutinee_ptr, scrutinee_ty) = codegen_value(scrutinee, env, em, ctx)?;
            if scrutinee_ty != CgType::Heap {
                return Err(format!("codegen: `match` scrutinee must be a heap-shaped value, found {scrutinee_ty:?}"));
            }
            let tag_addr = em.fresh_reg();
            em.push(format!("  {tag_addr} = getelementptr i8, ptr {scrutinee_ptr}, i64 8"));
            let scrutinee_tag = em.fresh_reg();
            em.push(format!("  {scrutinee_tag} = load i64, ptr {tag_addr}"));

            let done_label = em.fresh_label("match_done");
            let mut non_tail_results: Vec<(String, CgType, String)> = Vec::new();

            for arm in arms {
                let next_label = em.fresh_label("arm_next");

                if arm.tag != DEFAULT_ARM_TAG {
                    let id = tag_id(ctx, &arm.tag)?;
                    let matched = em.fresh_reg();
                    em.push(format!("  {matched} = icmp eq i64 {scrutinee_tag}, {id}"));
                    let matched_label = em.fresh_label("arm_matched");
                    em.push(format!("  br i1 {matched}, label %{matched_label}, label %{next_label}"));
                    em.start_block(&matched_label);
                }

                let mut arm_env = bind_match_arm(arm, &scrutinee_ptr, env, ctx)?;
                if arm.tag != DEFAULT_ARM_TAG {
                    let field_types = tag_field_types(ctx, &arm.tag)?.to_vec();
                    for (i, (name, fty)) in arm.bindings.iter().zip(&field_types).enumerate() {
                        let val = load_field_word(em, &scrutinee_ptr, i, *fty);
                        if *fty == CgType::Heap {
                            em.push(format!("  call void @plum_rc_inc(ptr {val})"));
                        }
                        arm_env.insert(name.clone(), (val, *fty));
                    }
                }

                if let Some(guard) = &arm.guard {
                    let (greg, gty) = codegen_value(guard, &arm_env, em, ctx)?;
                    if gty != CgType::Bool {
                        return Err(format!("codegen: match guard must be Bool, found {gty:?}"));
                    }
                    let pass_label = em.fresh_label("arm_guard_pass");
                    em.push(format!("  br i1 {greg}, label %{pass_label}, label %{next_label}"));
                    em.start_block(&pass_label);
                }

                let body_result = codegen_expr(&arm.body, &arm_env, em, ctx, tail)?;
                if let Some((reg, ty)) = body_result {
                    em.push(format!("  br label %{done_label}"));
                    non_tail_results.push((reg, ty, em.current_block.clone()));
                }
                em.start_block(&next_label);
            }
            // Every arm's tag/guard check failed — `plum-types` already
            // proved match exhaustiveness before codegen ever runs, so
            // this is genuinely unreachable for a well-typed program,
            // not just "shouldn't happen."
            em.push("  unreachable");

            if tail {
                Ok(None)
            } else {
                em.start_block(&done_label);
                let ty = non_tail_results
                    .first()
                    .map(|(_, ty, _)| *ty)
                    .ok_or_else(|| "codegen: internal error — match produced no reachable result".to_string())?;
                let phi_reg = em.fresh_reg();
                let parts: Vec<String> =
                    non_tail_results.iter().map(|(reg, _, block)| format!("[ {reg}, %{block} ]")).collect();
                em.push(format!("  {phi_reg} = phi {} {}", ty.llvm_type(), parts.join(", ")));
                Ok(Some((phi_reg, ty)))
            }
        }
        Expr::Call { callee, args } if tail => {
            let name = expect_direct_callee(callee)?.to_string();
            let sig = ctx.sigs.get(&name).cloned().ok_or_else(|| format!("codegen: unknown function {name:?}"))?;
            let args_ir = codegen_call_args(args, env, em, ctx, &sig, &name)?;
            let reg = em.fresh_reg();
            // `musttail` is only VALID when the caller's own prototype
            // matches the callee's (a real LLVM constraint — "cannot
            // guarantee tail call due to mismatched parameter counts"
            // — found via an actual `clang` compile failure, not
            // documented up front; see `Ctx::caller_sig`'s doc
            // comment). Self-recursion always trivially qualifies;
            // mutual recursion only does when both functions happen to
            // share a signature. A tail call to a DIFFERENT-shaped
            // function falls back to an ordinary `call` + `ret` —
            // still correct, just not `musttail`-GUARANTEED to reuse
            // the stack frame (LLVM's optimizer may still do it as a
            // best-effort sibling call under `-O2`, just not at `-O0`
            // the way `musttail` promises).
            if *ctx.caller_sig == sig {
                // The exact shape `musttail` requires: the call and
                // the `ret` are the ONLY two instructions, with
                // nothing in between — this is what gives LLVM's
                // guaranteed, portable tail-call elimination (the
                // whole reason DESIGN.md picked LLVM as the backend
                // over compiling through C, which has no such
                // guarantee).
                em.push(format!("  {reg} = musttail call {} @{name}({args_ir})", sig.ret.llvm_type()));
            } else {
                em.push(format!("  {reg} = call {} @{name}({args_ir})", sig.ret.llvm_type()));
            }
            em.push(format!("  ret {} {reg}", sig.ret.llvm_type()));
            Ok(None)
        }
        _ => {
            let (reg, ty) = codegen_value(expr, env, em, ctx)?;
            if tail {
                em.push(format!("  ret {} {reg}", ty.llvm_type()));
                Ok(None)
            } else {
                Ok(Some((reg, ty)))
            }
        }
    }
}
