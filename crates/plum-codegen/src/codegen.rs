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
use plum_ir::ir::{BinOp, Expr, MatchArm, PrimTy, RcOp, UnOp};
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

/// `lower.rs`'s own `ARRAY_TAG` — duplicated here rather than shared
/// across the crate boundary, same established precedent as
/// `DEFAULT_ARM_TAG` above. Needed to special-case `Ctor{tag:
/// ARRAY_TAG, ..}` (a non-empty array literal) into the array-alloc
/// codegen path instead of the ordinary tag-lookup `Ctor` path — every
/// OTHER tag is unaffected.
const ARRAY_TAG: &str = "0Array";

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
    /// The name of the function CURRENTLY being emitted — needed ONLY
    /// so a string LITERAL's module-level global constant gets a
    /// program-wide-unique name (`@.str.<fn_name>.<N>` — see `Emitter::
    /// fresh_string_global`) without needing a separate cross-function
    /// counter: `fn_name` is already unique per function, and `N`
    /// (`Emitter`'s own counter) is unique WITHIN one function.
    pub(crate) fn_name: &'a str,
    /// Every array element `CgType` that needs its own `@plum_rc_dec_
    /// array_<mangled>` release function, discovered so far across the
    /// WHOLE program (not just the function currently being emitted) —
    /// see `crate::register_array_elem_type`/`emit_array_release_fns`.
    /// A shared `RefCell`, not a plain reference, because codegen
    /// genuinely needs to ADD to this set as it walks each function
    /// body (a fresh array literal/op can reveal an element type no
    /// signature or struct field ever mentioned) — see `crate::
    /// emit_program`'s own doc comment on why this is the least
    /// invasive way to thread that discovery through.
    pub(crate) needed_arrays: &'a std::cell::RefCell<HashMap<String, CgType>>,
}

/// Registers `elem` (and any type it recursively contains) into
/// `ctx.needed_arrays` — the codegen.rs-side counterpart to `crate::
/// register_array_elem_type`, called at every site that emits a call to
/// an array's element-release function (so the function being called
/// is GUARANTEED to actually get defined). Cheap to call redundantly
/// (a `HashMap::entry` no-op if already registered), so call sites
/// don't need to reason about whether some OTHER path already
/// registered the same type.
fn register_array_elem(ctx: &Ctx, elem: &CgType) {
    crate::register_array_elem_type(&mut ctx.needed_arrays.borrow_mut(), &CgType::Array(Box::new(elem.clone())));
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
    /// MODULE-LEVEL string-literal global constant declarations this
    /// function's body needed (`@.str.<fn_name>.<N> = private constant
    /// [K x i8] c"..."`) — collected separately from `lines` because a
    /// global definition can't legally appear INSIDE a `define { ... }`
    /// block; `lib.rs::emit_function` splices these in just BEFORE the
    /// function's own `define` line instead. See `fresh_string_global`.
    pub(crate) string_globals: Vec<String>,
}

impl Emitter {
    pub(crate) fn new() -> Self {
        Emitter {
            next_id: 0,
            lines: vec!["entry:".to_string()],
            current_block: "entry".to_string(),
            string_globals: Vec::new(),
        }
    }

    /// Emits a fresh, program-wide-unique module-level global constant
    /// holding `bytes` (a string literal's raw UTF-8 content — see
    /// `Ctx::fn_name`'s doc comment for the naming scheme) and returns
    /// its global name, ready to reference directly as a `ptr` operand
    /// (opaque pointers mean no `getelementptr`/`bitcast` is needed at
    /// the reference site — matching `plumc::emit_main`'s existing
    /// `@fmt` global precedent). Every byte is hex-escaped
    /// unconditionally (`\XX`), even printable ASCII — simpler and
    /// always correct, rather than selectively escaping only `"`/`\`/
    /// non-printable bytes.
    pub(crate) fn fresh_string_global(&mut self, fn_name: &str, bytes: &[u8]) -> String {
        let id = self.fresh_id();
        let name = format!("@.str.{fn_name}.{id}");
        let mut escaped = String::with_capacity(bytes.len() * 4);
        for b in bytes {
            escaped.push_str(&format!("\\{b:02X}"));
        }
        self.string_globals.push(format!(
            "{name} = private constant [{} x i8] c\"{escaped}\"",
            bytes.len()
        ));
        name
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

    /// The label of the block instructions are CURRENTLY being appended
    /// to — needed by `inc_copied_array_elements`'s loop, which (unlike
    /// every other multi-block helper here) is built as a standalone
    /// function that doesn't already have its caller's `current_block`
    /// in scope.
    pub(crate) fn current_block(&self) -> &str {
        &self.current_block
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
        if &ty != expected_ty {
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
        // `Str`/`Array` are both `ptr` at the LLVM level, same as
        // `Heap` — see `CgType::Str`/`Array`'s own doc comment for why
        // they're still distinct at the `CgType` level despite sharing
        // this exact store/load mechanism.
        CgType::Heap | CgType::Str | CgType::Array(_) => {
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
        CgType::Heap | CgType::Str | CgType::Array(_) => {
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
        store_field_word(em, &cell, i, reg, ty.clone());
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
        if &ty != expected {
            return Err(format!("codegen: constructor {tag:?} field {i}: expected {expected:?}, found {ty:?}"));
        }
        vals.push((reg, ty));
    }
    Ok(vals)
}

// --- arrays and strings ---
//
// See lib.rs's `CgType::Str`/`Array` doc comment and this crate's
// module-level design notes for the two new heap-cell layouts these
// helpers operate on: `{ i64 refcount, i64 len, i8 bytes[len], i8 '\0' }`
// for a string, `{ i64 refcount, i64 len, <elemTy word> elements[len] }`
// for an array (structurally identical to a `Ctor` cell, just with a
// RUNTIME-variable `len` instead of a fixed field count baked into
// `tag_fields`).

/// Converts an `EmptyArray` literal's `PrimTy` payload (see `ir::Expr::
/// EmptyArray`'s doc comment) into the `CgType` codegen actually works
/// with — a pure, structural, always-succeeding mapping (unlike
/// `plum_type_to_cg_type` in `plumc`, `PrimTy` is ALREADY a small,
/// closed set with no generics/type-variables left to reject).
fn prim_ty_to_cg_type(ty: &PrimTy) -> CgType {
    match ty {
        PrimTy::Int => CgType::Int,
        PrimTy::Float => CgType::Float,
        PrimTy::Bool => CgType::Bool,
        PrimTy::Str => CgType::Str,
        PrimTy::Array(inner) => CgType::Array(Box::new(prim_ty_to_cg_type(inner))),
        PrimTy::Heap => CgType::Heap,
    }
}

/// Reads an array/string cell's `len` field (byte offset 8, shared by
/// both layouts — see this section's own doc comment).
fn load_array_len(em: &mut Emitter, ptr: &str) -> String {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {ptr}, i64 8"));
    let len = em.fresh_reg();
    em.push(format!("  {len} = load i64, ptr {addr}"));
    len
}

/// `0 <= idx < len`, as a single `i1` SSA value — shared by every
/// bounds-checked array/string operation (`Index`, `.set()`, `.remove()`).
fn emit_bounds_ok(em: &mut Emitter, idx: &str, len: &str) -> String {
    let ge0 = em.fresh_reg();
    em.push(format!("  {ge0} = icmp sge i64 {idx}, 0"));
    let lt_len = em.fresh_reg();
    em.push(format!("  {lt_len} = icmp slt i64 {idx}, {len}"));
    let ok = em.fresh_reg();
    em.push(format!("  {ok} = and i1 {ge0}, {lt_len}"));
    ok
}

/// Emits a runtime-checked failure point: if `ok_reg` (an `i1`, true
/// meaning "check passed") is false, aborts via `@plum_abort` with
/// `message` instead of continuing — the array/string-op counterpart to
/// `Match`'s compile-time-provable `unreachable` (there's no way to
/// know an index/emptiness check's outcome until the program actually
/// runs). Leaves `em`'s current block as the "ok" continuation — every
/// instruction emitted by the caller AFTER calling this only ever runs
/// once the check has passed.
fn emit_runtime_check(em: &mut Emitter, ctx: &Ctx, ok_reg: &str, message: &str) {
    let fail_label = em.fresh_label("check_fail");
    let ok_label = em.fresh_label("check_ok");
    em.push(format!("  br i1 {ok_reg}, label %{ok_label}, label %{fail_label}"));

    em.start_block(&fail_label);
    let mut bytes = message.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes.push(0);
    let gname = em.fresh_string_global(ctx.fn_name, &bytes);
    em.push(format!("  call void @plum_abort(ptr {gname})"));
    em.push("  unreachable".to_string());

    em.start_block(&ok_label);
}

/// The runtime-index counterpart to `store_field_word` — used for an
/// array element slot, where the index is only known at RUNTIME (an SSA
/// register) rather than a compile-time `usize`. Same uniform-word
/// conversion story as `store_field_word` itself; the two aren't
/// literally merged into one function purely because their index
/// operand's TYPE differs (a compile-time constant folds directly into
/// the `getelementptr`'s own immediate, whereas a runtime index needs an
/// explicit `mul`/`add` first) — see this module's own doc comment for
/// why the STATIC-index versions are left as they were rather than
/// generalized to always compute the offset at runtime.
fn store_array_elem(em: &mut Emitter, array_ptr: &str, index_reg: &str, value: &str, ty: CgType) {
    let addr = array_elem_addr(em, array_ptr, index_reg);
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
        CgType::Heap | CgType::Str | CgType::Array(_) => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = ptrtoint ptr {value} to i64"));
            r
        }
    };
    em.push(format!("  store i64 {word}, ptr {addr}"));
}

/// The runtime-index counterpart to `load_field_word` — see
/// `store_array_elem`'s doc comment.
fn load_array_elem(em: &mut Emitter, array_ptr: &str, index_reg: &str, expected_ty: &CgType) -> String {
    let addr = array_elem_addr(em, array_ptr, index_reg);
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
        CgType::Heap | CgType::Str | CgType::Array(_) => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = inttoptr i64 {word} to ptr"));
            r
        }
    }
}

fn array_elem_addr(em: &mut Emitter, array_ptr: &str, index_reg: &str) -> String {
    let word_off = em.fresh_reg();
    em.push(format!("  {word_off} = mul i64 {index_reg}, 8"));
    let byte_off = em.fresh_reg();
    em.push(format!("  {byte_off} = add i64 {word_off}, 16"));
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {array_ptr}, i64 {byte_off}"));
    addr
}

/// Decrements the SINGLE element at `index_reg` of the array at `ptr`
/// (a no-op for a scalar `elem_ty`, which carries no refcount) — used
/// ONLY on a REUSE-in-place path (`ArraySetReuse`/`ArrayPopReuse`/
/// `ArrayRemoveReuse`'s own reuse branch) where the cell being mutated
/// is PROVABLY uniquely owned (refcount was 1): the element about to be
/// overwritten/dropped genuinely loses its only reference right here,
/// unlike the FRESH-allocation paths (see `inc_copied_array_elements`'s
/// doc comment for why THOSE need the opposite treatment — an INCrement,
/// not a decrement).
fn dec_array_element_at(em: &mut Emitter, ctx: &Ctx, ptr: &str, index_reg: &str, elem_ty: &CgType) {
    let Some(dec_fn) = crate::dec_fn_for(elem_ty) else {
        return;
    };
    if let CgType::Array(inner) = elem_ty {
        register_array_elem(ctx, inner);
    }
    let word = load_array_elem(em, ptr, index_reg, &CgType::Heap);
    em.push(format!("  call void {dec_fn}(ptr {word})"));
}

/// After copying `count_reg` elements into the array cell `dst`
/// (indices `0..count_reg`), INCrements every one of them that's heap-
/// shaped. Needed because a FRESH-allocation array op (`ArrayPush`/
/// `ArrayPop`/`ArraySet`/`ArrayRemove`'s non-reuse path) creates a
/// SECOND, independently-owned array cell that now shares every COPIED
/// element with the original — no upstream FBIP pass tracks this (it
/// only ever reasons about WHOLE heap-cell VARIABLES, via `insert_
/// refcount_ops`'s last-use analysis, never individual array SLOTS
/// after a raw `memcpy`-based bulk copy), so codegen has to account for
/// it here directly, or a shared heap-shaped element would end up
/// double-decremented once both arrays are eventually released — a
/// real use-after-free/double-free, not just an accepted leak.
/// `skip_index`, if given, names ONE destination index to leave
/// un-incremented — `ArraySet`'s fresh path (`codegen_array_set_fresh`)
/// copies the OLD element at the target index too (a side effect of
/// copying the whole buffer in one `memcpy`), but immediately overwrites
/// it with the new value right after, so incrementing it first would
/// leak a reference nothing ever decrements.
fn inc_copied_array_elements(em: &mut Emitter, dst: &str, count_reg: &str, elem_ty: &CgType, skip_index: Option<&str>) {
    if crate::dec_fn_for(elem_ty).is_none() {
        return; // scalar element type — nothing carries a refcount.
    }
    let entry_block = em.current_block().to_string();
    let check_label = em.fresh_label("inc_check");
    let body_label = em.fresh_label("inc_body");
    let action_label = em.fresh_label("inc_action");
    let cont_label = em.fresh_label("inc_cont");
    let after_label = em.fresh_label("inc_after");

    em.push(format!("  br label %{check_label}"));
    em.start_block(&check_label);
    let i = em.fresh_reg();
    let i_next = em.fresh_reg(); // name reserved now, assigned in `cont_label` below
    em.push(format!("  {i} = phi i64 [ 0, %{entry_block} ], [ {i_next}, %{cont_label} ]"));
    let cont_check = em.fresh_reg();
    em.push(format!("  {cont_check} = icmp slt i64 {i}, {count_reg}"));
    em.push(format!("  br i1 {cont_check}, label %{body_label}, label %{after_label}"));

    em.start_block(&body_label);
    if let Some(skip) = skip_index {
        let is_skip = em.fresh_reg();
        em.push(format!("  {is_skip} = icmp eq i64 {i}, {skip}"));
        em.push(format!("  br i1 {is_skip}, label %{cont_label}, label %{action_label}"));
    } else {
        em.push(format!("  br label %{action_label}"));
    }

    em.start_block(&action_label);
    let elem_ptr = load_array_elem(em, dst, &i, &CgType::Heap);
    em.push(format!("  call void @plum_rc_inc(ptr {elem_ptr})"));
    em.push(format!("  br label %{cont_label}"));

    em.start_block(&cont_label);
    em.push(format!("  {i_next} = add i64 {i}, 1"));
    em.push(format!("  br label %{check_label}"));

    em.start_block(&after_label);
}

/// `[e1, e2, ...]` (non-empty) — see `Ctor{tag: ARRAY_TAG, ..}`'s own
/// dispatch in `codegen_value`. Every element must codegen to the SAME
/// `CgType` (this backend has no type checker of its own to have
/// already proven that — `plum_types::infer` did, upstream — so it's
/// re-verified here structurally, the same "trust but verify against
/// what's actually reachable from this IR" stance every other codegen
/// arm takes).
fn codegen_array_literal(fields: &[Expr], env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    if fields.is_empty() {
        return Err(
            "codegen: internal error — an empty array literal reached the non-empty array-literal codegen path; \
             it should have lowered to `EmptyArray` instead (was `plum_types::Infer` run before lowering?)"
                .to_string(),
        );
    }
    let mut vals = Vec::with_capacity(fields.len());
    let mut elem_ty: Option<CgType> = None;
    for f in fields {
        let (reg, ty) = codegen_value(f, env, em, ctx)?;
        match &elem_ty {
            None => elem_ty = Some(ty.clone()),
            Some(expected) if *expected == ty => {}
            Some(expected) => {
                return Err(format!(
                    "codegen: array literal elements must share one type — expected {expected:?}, found {ty:?}"
                ))
            }
        }
        vals.push((reg, ty));
    }
    let elem_ty = elem_ty.expect("checked non-empty above");
    register_array_elem(ctx, &elem_ty);
    let len = vals.len();
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc_array(i64 {len})"));
    for (i, (reg, ty)) in vals.iter().enumerate() {
        store_field_word(em, &cell, i, reg, ty.clone());
    }
    Ok((cell, CgType::Array(Box::new(elem_ty))))
}

/// The FRESH-allocation half of `.push()` — shared by the ordinary
/// `ArrayPush` node and `ArrayPushReuse`'s own fresh-alloc fallback
/// branch (refcount wasn't 1), same relationship `codegen_ctor_alloc`
/// has to `Ctor`/`CtorReuse`.
fn codegen_array_push_fresh(src: &str, elem_ty: CgType, value_reg: &str, em: &mut Emitter, ctx: &Ctx) -> (String, CgType) {
    register_array_elem(ctx, &elem_ty);
    let old_len = load_array_len(em, src);
    let new_len = em.fresh_reg();
    em.push(format!("  {new_len} = add i64 {old_len}, 1"));
    let new_cell = em.fresh_reg();
    em.push(format!("  {new_cell} = call ptr @plum_alloc_array(i64 {new_len})"));
    let old_bytes = em.fresh_reg();
    em.push(format!("  {old_bytes} = getelementptr i8, ptr {src}, i64 16"));
    let new_bytes = em.fresh_reg();
    em.push(format!("  {new_bytes} = getelementptr i8, ptr {new_cell}, i64 16"));
    let copy_size = em.fresh_reg();
    em.push(format!("  {copy_size} = mul i64 {old_len}, 8"));
    let memcpy_r = em.fresh_reg();
    em.push(format!("  {memcpy_r} = call ptr @memcpy(ptr {new_bytes}, ptr {old_bytes}, i64 {copy_size})"));
    inc_copied_array_elements(em, &new_cell, &old_len, &elem_ty, None);
    store_array_elem(em, &new_cell, &old_len, value_reg, elem_ty.clone());
    (new_cell, CgType::Array(Box::new(elem_ty)))
}

/// The FRESH-allocation half of `.pop()` — see `codegen_array_push_
/// fresh`'s doc comment for the shared "fresh vs reuse" relationship.
/// The dropped LAST element needs no dec here: `src` (the ORIGINAL
/// array) survives this call unchanged and keeps holding it — only the
/// REUSE path (`ArrayPopReuse`, where `src`'s cell IS the one being
/// mutated, nothing else surviving) actually loses that reference.
fn codegen_array_pop_fresh(src: &str, elem_ty: CgType, em: &mut Emitter, ctx: &Ctx) -> (String, CgType) {
    register_array_elem(ctx, &elem_ty);
    let len = load_array_len(em, src);
    let ok = em.fresh_reg();
    em.push(format!("  {ok} = icmp sgt i64 {len}, 0"));
    emit_runtime_check(em, ctx, &ok, "cannot pop from an empty array");
    let new_len = em.fresh_reg();
    em.push(format!("  {new_len} = sub i64 {len}, 1"));
    let new_cell = em.fresh_reg();
    em.push(format!("  {new_cell} = call ptr @plum_alloc_array(i64 {new_len})"));
    let old_bytes = em.fresh_reg();
    em.push(format!("  {old_bytes} = getelementptr i8, ptr {src}, i64 16"));
    let new_bytes = em.fresh_reg();
    em.push(format!("  {new_bytes} = getelementptr i8, ptr {new_cell}, i64 16"));
    let copy_size = em.fresh_reg();
    em.push(format!("  {copy_size} = mul i64 {new_len}, 8"));
    let memcpy_r = em.fresh_reg();
    em.push(format!("  {memcpy_r} = call ptr @memcpy(ptr {new_bytes}, ptr {old_bytes}, i64 {copy_size})"));
    inc_copied_array_elements(em, &new_cell, &new_len, &elem_ty, None);
    (new_cell, CgType::Array(Box::new(elem_ty)))
}

/// The FRESH-allocation half of `.set()` — see `codegen_array_push_
/// fresh`'s doc comment. The OLD element at `idx_reg` needs no inc/dec
/// here: `src` still holds it afterward unchanged (this op only affects
/// the NEW array, which never references it at all — see `inc_copied_
/// array_elements`'s `skip_index` parameter). Only the REUSE path
/// (`ArraySetReuse`'s own reuse branch) actually replaces the ONLY
/// reference to it, which is what needs an explicit dec there instead.
fn codegen_array_set_fresh(src: &str, elem_ty: CgType, idx_reg: &str, value_reg: &str, em: &mut Emitter, ctx: &Ctx) -> (String, CgType) {
    register_array_elem(ctx, &elem_ty);
    let len = load_array_len(em, src);
    let ok = emit_bounds_ok(em, idx_reg, &len);
    emit_runtime_check(em, ctx, &ok, "array index out of bounds");
    let new_cell = em.fresh_reg();
    em.push(format!("  {new_cell} = call ptr @plum_alloc_array(i64 {len})"));
    let old_bytes = em.fresh_reg();
    em.push(format!("  {old_bytes} = getelementptr i8, ptr {src}, i64 16"));
    let new_bytes = em.fresh_reg();
    em.push(format!("  {new_bytes} = getelementptr i8, ptr {new_cell}, i64 16"));
    let copy_size = em.fresh_reg();
    em.push(format!("  {copy_size} = mul i64 {len}, 8"));
    let memcpy_r = em.fresh_reg();
    em.push(format!("  {memcpy_r} = call ptr @memcpy(ptr {new_bytes}, ptr {old_bytes}, i64 {copy_size})"));
    inc_copied_array_elements(em, &new_cell, &len, &elem_ty, Some(idx_reg));
    store_array_elem(em, &new_cell, idx_reg, value_reg, elem_ty.clone());
    (new_cell, CgType::Array(Box::new(elem_ty)))
}

/// The FRESH-allocation half of `.remove()` — see `codegen_array_push_
/// fresh`'s doc comment. Copies `[0, idx)` and `[idx+1, len)` into the
/// new, one-shorter cell (two separate `memcpy`s — the removed index
/// splits the copy into two contiguous runs, unlike every other array
/// op here, which only ever needs one). The dropped element at `idx_reg`
/// needs no dec here (`src` still holds it) — same reasoning as
/// `codegen_array_set_fresh`.
fn codegen_array_remove_fresh(src: &str, elem_ty: CgType, idx_reg: &str, em: &mut Emitter, ctx: &Ctx) -> (String, CgType) {
    register_array_elem(ctx, &elem_ty);
    let len = load_array_len(em, src);
    let ok = emit_bounds_ok(em, idx_reg, &len);
    emit_runtime_check(em, ctx, &ok, "array index out of bounds");
    let new_len = em.fresh_reg();
    em.push(format!("  {new_len} = sub i64 {len}, 1"));
    let new_cell = em.fresh_reg();
    em.push(format!("  {new_cell} = call ptr @plum_alloc_array(i64 {new_len})"));
    let old_bytes = em.fresh_reg();
    em.push(format!("  {old_bytes} = getelementptr i8, ptr {src}, i64 16"));
    let new_bytes = em.fresh_reg();
    em.push(format!("  {new_bytes} = getelementptr i8, ptr {new_cell}, i64 16"));

    let head_size = em.fresh_reg();
    em.push(format!("  {head_size} = mul i64 {idx_reg}, 8"));
    let memcpy1 = em.fresh_reg();
    em.push(format!("  {memcpy1} = call ptr @memcpy(ptr {new_bytes}, ptr {old_bytes}, i64 {head_size})"));

    let idx_plus1 = em.fresh_reg();
    em.push(format!("  {idx_plus1} = add i64 {idx_reg}, 1"));
    let tail_count = em.fresh_reg();
    em.push(format!("  {tail_count} = sub i64 {len}, {idx_plus1}"));
    let tail_size = em.fresh_reg();
    em.push(format!("  {tail_size} = mul i64 {tail_count}, 8"));
    let src_tail_off = em.fresh_reg();
    em.push(format!("  {src_tail_off} = mul i64 {idx_plus1}, 8"));
    let src_tail = em.fresh_reg();
    em.push(format!("  {src_tail} = getelementptr i8, ptr {old_bytes}, i64 {src_tail_off}"));
    let dst_tail_off = em.fresh_reg();
    em.push(format!("  {dst_tail_off} = mul i64 {idx_reg}, 8"));
    let dst_tail = em.fresh_reg();
    em.push(format!("  {dst_tail} = getelementptr i8, ptr {new_bytes}, i64 {dst_tail_off}"));
    let memcpy2 = em.fresh_reg();
    em.push(format!("  {memcpy2} = call ptr @memcpy(ptr {dst_tail}, ptr {src_tail}, i64 {tail_size})"));

    inc_copied_array_elements(em, &new_cell, &new_len, &elem_ty, None);
    (new_cell, CgType::Array(Box::new(elem_ty)))
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
        // A string LITERAL — `@plum_alloc_str` allocates a fresh cell,
        // then the constant bytes are `memcpy`'d in from a module-level
        // global (see `Emitter::fresh_string_global`). An empty literal
        // needs no copy at all (`@plum_alloc_str(0)` already leaves the
        // (empty) byte region and its trailing NUL correctly set up).
        Expr::Str(s) => {
            let bytes = s.as_bytes();
            let len = bytes.len();
            let cell = em.fresh_reg();
            em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {len})"));
            if !bytes.is_empty() {
                let gname = em.fresh_string_global(ctx.fn_name, bytes);
                let dst = em.fresh_reg();
                em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
                let copy_r = em.fresh_reg();
                em.push(format!("  {copy_r} = call ptr @memcpy(ptr {dst}, ptr {gname}, i64 {len})"));
            }
            Ok((cell, CgType::Str))
        }
        Expr::EmptyArray(prim_ty) => {
            let elem_ty = prim_ty_to_cg_type(prim_ty);
            register_array_elem(ctx, &elem_ty);
            let cell = em.fresh_reg();
            em.push(format!("  {cell} = call ptr @plum_alloc_array(i64 0)"));
            Ok((cell, CgType::Array(Box::new(elem_ty))))
        }
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
        // `tag == ARRAY_TAG` is a non-empty array literal — see this
        // module's array-literal section above — special-cased here
        // (duplicated-as-local-const, same as `DEFAULT_ARM_TAG` already
        // is) to route to the array-alloc path instead of the ordinary
        // tag-lookup `Ctor` path; every OTHER tag is unaffected.
        Expr::Ctor { tag, fields } if tag == ARRAY_TAG => codegen_array_literal(fields, env, em, ctx),
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
                store_field_word(em, &old_ptr, i, reg, ty.clone());
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
        // `arr[i]`/`s[i]` — genuinely shape-shared at the IR level (see
        // `Index`'s own doc comment: no static hint of Array vs Str at
        // the node itself) — dispatch is a simple match on `base`'s
        // already-known `CgType`. String indexing returns the raw BYTE
        // value as an `Int` (0..=255), matching `plum-interp`'s own
        // `Expr::Index`-on-`Str` semantics exactly (verified directly
        // against `crates/plum-interp/src/lib.rs`'s `heap::CellView::
        // Str` arm — see this crate's own tests).
        Expr::Index { base, index } => {
            let (base_reg, base_ty) = codegen_value(base, env, em, ctx)?;
            let (idx_reg, idx_ty) = codegen_value(index, env, em, ctx)?;
            if idx_ty != CgType::Int {
                return Err(format!("codegen: index must be Int, found {idx_ty:?}"));
            }
            match base_ty {
                CgType::Array(elem_ty) => {
                    let len = load_array_len(em, &base_reg);
                    let ok = emit_bounds_ok(em, &idx_reg, &len);
                    emit_runtime_check(em, ctx, &ok, "array index out of bounds");
                    let val = load_array_elem(em, &base_reg, &idx_reg, &elem_ty);
                    Ok((val, *elem_ty))
                }
                CgType::Str => {
                    let len = load_array_len(em, &base_reg);
                    let ok = emit_bounds_ok(em, &idx_reg, &len);
                    emit_runtime_check(em, ctx, &ok, "string index out of bounds");
                    let byte_off = em.fresh_reg();
                    em.push(format!("  {byte_off} = add i64 {idx_reg}, 16"));
                    let addr = em.fresh_reg();
                    em.push(format!("  {addr} = getelementptr i8, ptr {base_reg}, i64 {byte_off}"));
                    let byte = em.fresh_reg();
                    em.push(format!("  {byte} = load i8, ptr {addr}"));
                    let val = em.fresh_reg();
                    em.push(format!("  {val} = zext i8 {byte} to i64"));
                    Ok((val, CgType::Int))
                }
                other => Err(format!("codegen: indexing requires an Array or Str value, found {other:?}")),
            }
        }
        // `.len()` — same shape-shared dispatch as `Index`, on the same
        // `len` field (byte offset 8, identical in both cell layouts).
        Expr::ArrayLen { array } => {
            let (base_reg, base_ty) = codegen_value(array, env, em, ctx)?;
            match base_ty {
                CgType::Array(_) | CgType::Str => Ok((load_array_len(em, &base_reg), CgType::Int)),
                other => Err(format!("codegen: `.len()` requires an Array or Str value, found {other:?}")),
            }
        }
        Expr::ArrayPush { array, value } => {
            let (arr_reg, arr_ty) = codegen_value(array, env, em, ctx)?;
            let CgType::Array(elem_ty) = arr_ty else {
                return Err(format!("codegen: `.push()` requires an Array value, found {arr_ty:?}"));
            };
            let (val_reg, val_ty) = codegen_value(value, env, em, ctx)?;
            if val_ty != *elem_ty {
                return Err(format!("codegen: `.push()` value type mismatch — expected {elem_ty:?}, found {val_ty:?}"));
            }
            Ok(codegen_array_push_fresh(&arr_reg, *elem_ty, &val_reg, em, ctx))
        }
        Expr::ArrayPop { array } => {
            let (arr_reg, arr_ty) = codegen_value(array, env, em, ctx)?;
            let CgType::Array(elem_ty) = arr_ty else {
                return Err(format!("codegen: `.pop()` requires an Array value, found {arr_ty:?}"));
            };
            Ok(codegen_array_pop_fresh(&arr_reg, *elem_ty, em, ctx))
        }
        Expr::ArraySet { array, index, value } => {
            let (arr_reg, arr_ty) = codegen_value(array, env, em, ctx)?;
            let CgType::Array(elem_ty) = arr_ty else {
                return Err(format!("codegen: `.set()` requires an Array value, found {arr_ty:?}"));
            };
            let (idx_reg, idx_ty) = codegen_value(index, env, em, ctx)?;
            if idx_ty != CgType::Int {
                return Err(format!("codegen: `.set()` index must be Int, found {idx_ty:?}"));
            }
            let (val_reg, val_ty) = codegen_value(value, env, em, ctx)?;
            if val_ty != *elem_ty {
                return Err(format!("codegen: `.set()` value type mismatch — expected {elem_ty:?}, found {val_ty:?}"));
            }
            Ok(codegen_array_set_fresh(&arr_reg, *elem_ty, &idx_reg, &val_reg, em, ctx))
        }
        Expr::ArrayRemove { array, index } => {
            let (arr_reg, arr_ty) = codegen_value(array, env, em, ctx)?;
            let CgType::Array(elem_ty) = arr_ty else {
                return Err(format!("codegen: `.remove()` requires an Array value, found {arr_ty:?}"));
            };
            let (idx_reg, idx_ty) = codegen_value(index, env, em, ctx)?;
            if idx_ty != CgType::Int {
                return Err(format!("codegen: `.remove()` index must be Int, found {idx_ty:?}"));
            }
            Ok(codegen_array_remove_fresh(&arr_reg, *elem_ty, &idx_reg, em, ctx))
        }
        Expr::ArrayPushReuse { reuse_of, value } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            let CgType::Array(elem_ty) = old_ty else {
                return Err(format!("codegen: internal error — ArrayPushReuse target {reuse_of:?} is not an array"));
            };
            let (val_reg, val_ty) = codegen_value(value, env, em, ctx)?;
            if val_ty != *elem_ty {
                return Err(format!("codegen: `.push()` value type mismatch — expected {elem_ty:?}, found {val_ty:?}"));
            }
            register_array_elem(ctx, &elem_ty);

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("array_push_reuse");
            let alloc_label = em.fresh_label("array_push_fresh");
            let merge_label = em.fresh_label("array_push_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            let old_len = load_array_len(em, &old_ptr);
            let new_len = em.fresh_reg();
            em.push(format!("  {new_len} = add i64 {old_len}, 1"));
            let elems_bytes = em.fresh_reg();
            em.push(format!("  {elems_bytes} = mul i64 {new_len}, 8"));
            let new_size = em.fresh_reg();
            em.push(format!("  {new_size} = add i64 {elems_bytes}, 16"));
            let grown = em.fresh_reg();
            em.push(format!("  {grown} = call ptr @realloc(ptr {old_ptr}, i64 {new_size})"));
            // `realloc` doesn't preserve our OWN refcount bookkeeping
            // semantics — the bytes it copies over include whatever we
            // just wrote (`rc2`, i.e. 0) at offset 0, so it must be
            // explicitly restored to 1 here, same as `CtorReuse`'s own
            // reuse branch does.
            em.push(format!("  store i64 1, ptr {grown}"));
            let len_addr = em.fresh_reg();
            em.push(format!("  {len_addr} = getelementptr i8, ptr {grown}, i64 8"));
            em.push(format!("  store i64 {new_len}, ptr {len_addr}"));
            store_array_elem(em, &grown, &old_len, &val_reg, (*elem_ty).clone());
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_push_fresh(&old_ptr, (*elem_ty).clone(), &val_reg, em, ctx);
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_label} ], [ {fresh}, %{alloc_label} ]"));
            Ok((result, CgType::Array(elem_ty)))
        }
        Expr::ArrayPopReuse { reuse_of } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            let CgType::Array(elem_ty) = old_ty else {
                return Err(format!("codegen: internal error — ArrayPopReuse target {reuse_of:?} is not an array"));
            };
            register_array_elem(ctx, &elem_ty);

            let len = load_array_len(em, &old_ptr);
            let ok = em.fresh_reg();
            em.push(format!("  {ok} = icmp sgt i64 {len}, 0"));
            emit_runtime_check(em, ctx, &ok, "cannot pop from an empty array");
            let new_len = em.fresh_reg();
            em.push(format!("  {new_len} = sub i64 {len}, 1"));

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("array_pop_reuse");
            let alloc_label = em.fresh_label("array_pop_fresh");
            let merge_label = em.fresh_label("array_pop_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            // The dropped element (index `new_len`, the old last index)
            // is about to lose its ONLY reference (this cell is
            // provably uniquely owned here) — dec it BEFORE shrinking,
            // if heap-shaped.
            dec_array_element_at(em, ctx, &old_ptr, &new_len, &elem_ty);
            let elems_bytes = em.fresh_reg();
            em.push(format!("  {elems_bytes} = mul i64 {new_len}, 8"));
            let new_size = em.fresh_reg();
            em.push(format!("  {new_size} = add i64 {elems_bytes}, 16"));
            let grown = em.fresh_reg();
            em.push(format!("  {grown} = call ptr @realloc(ptr {old_ptr}, i64 {new_size})"));
            em.push(format!("  store i64 1, ptr {grown}"));
            let len_addr = em.fresh_reg();
            em.push(format!("  {len_addr} = getelementptr i8, ptr {grown}, i64 8"));
            em.push(format!("  store i64 {new_len}, ptr {len_addr}"));
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_pop_fresh(&old_ptr, (*elem_ty).clone(), em, ctx);
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_label} ], [ {fresh}, %{alloc_label} ]"));
            Ok((result, CgType::Array(elem_ty)))
        }
        Expr::ArraySetReuse { reuse_of, index, value } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            let CgType::Array(elem_ty) = old_ty else {
                return Err(format!("codegen: internal error — ArraySetReuse target {reuse_of:?} is not an array"));
            };
            let (idx_reg, idx_ty) = codegen_value(index, env, em, ctx)?;
            if idx_ty != CgType::Int {
                return Err(format!("codegen: `.set()` index must be Int, found {idx_ty:?}"));
            }
            let (val_reg, val_ty) = codegen_value(value, env, em, ctx)?;
            if val_ty != *elem_ty {
                return Err(format!("codegen: `.set()` value type mismatch — expected {elem_ty:?}, found {val_ty:?}"));
            }
            register_array_elem(ctx, &elem_ty);

            let len = load_array_len(em, &old_ptr);
            let ok = emit_bounds_ok(em, &idx_reg, &len);
            emit_runtime_check(em, ctx, &ok, "array index out of bounds");

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("array_set_reuse");
            let alloc_label = em.fresh_label("array_set_fresh");
            let merge_label = em.fresh_label("array_set_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            // The OVERWRITTEN element is about to lose its ONLY
            // reference (uniquely owned here) — dec it FIRST, if
            // heap-shaped, then overwrite — see this module's `dec_
            // array_element_at` doc comment for the full argument.
            dec_array_element_at(em, ctx, &old_ptr, &idx_reg, &elem_ty);
            store_array_elem(em, &old_ptr, &idx_reg, &val_reg, (*elem_ty).clone());
            em.push(format!("  store i64 1, ptr {old_ptr}"));
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_set_fresh(&old_ptr, (*elem_ty).clone(), &idx_reg, &val_reg, em, ctx);
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {old_ptr}, %{reuse_label} ], [ {fresh}, %{alloc_label} ]"));
            Ok((result, CgType::Array(elem_ty)))
        }
        Expr::ArrayRemoveReuse { reuse_of, index } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            let CgType::Array(elem_ty) = old_ty else {
                return Err(format!("codegen: internal error — ArrayRemoveReuse target {reuse_of:?} is not an array"));
            };
            let (idx_reg, idx_ty) = codegen_value(index, env, em, ctx)?;
            if idx_ty != CgType::Int {
                return Err(format!("codegen: `.remove()` index must be Int, found {idx_ty:?}"));
            }
            register_array_elem(ctx, &elem_ty);

            let len = load_array_len(em, &old_ptr);
            let ok = emit_bounds_ok(em, &idx_reg, &len);
            emit_runtime_check(em, ctx, &ok, "array index out of bounds");
            let new_len = em.fresh_reg();
            em.push(format!("  {new_len} = sub i64 {len}, 1"));

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("array_remove_reuse");
            let alloc_label = em.fresh_label("array_remove_fresh");
            let merge_label = em.fresh_label("array_remove_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            dec_array_element_at(em, ctx, &old_ptr, &idx_reg, &elem_ty);
            // Shift `[idx+1, len)` down onto `[idx, len-1)` IN PLACE —
            // these two regions can OVERLAP (adjacent, shifted by one
            // element), so this must use libc `memmove` (overlap-safe),
            // never `memcpy` (undefined behavior on overlapping
            // regions) — the one place in this whole backend that
            // distinction actually matters, since every OTHER copy here
            // is always between two DISTINCT, freshly-allocated cells.
            let base = em.fresh_reg();
            em.push(format!("  {base} = getelementptr i8, ptr {old_ptr}, i64 16"));
            let idx_plus1 = em.fresh_reg();
            em.push(format!("  {idx_plus1} = add i64 {idx_reg}, 1"));
            let tail_count = em.fresh_reg();
            em.push(format!("  {tail_count} = sub i64 {len}, {idx_plus1}"));
            let tail_size = em.fresh_reg();
            em.push(format!("  {tail_size} = mul i64 {tail_count}, 8"));
            let src_off = em.fresh_reg();
            em.push(format!("  {src_off} = mul i64 {idx_plus1}, 8"));
            let src_tail = em.fresh_reg();
            em.push(format!("  {src_tail} = getelementptr i8, ptr {base}, i64 {src_off}"));
            let dst_off = em.fresh_reg();
            em.push(format!("  {dst_off} = mul i64 {idx_reg}, 8"));
            let dst_tail = em.fresh_reg();
            em.push(format!("  {dst_tail} = getelementptr i8, ptr {base}, i64 {dst_off}"));
            let memmove_r = em.fresh_reg();
            em.push(format!("  {memmove_r} = call ptr @memmove(ptr {dst_tail}, ptr {src_tail}, i64 {tail_size})"));

            let elems_bytes = em.fresh_reg();
            em.push(format!("  {elems_bytes} = mul i64 {new_len}, 8"));
            let new_size = em.fresh_reg();
            em.push(format!("  {new_size} = add i64 {elems_bytes}, 16"));
            let grown = em.fresh_reg();
            em.push(format!("  {grown} = call ptr @realloc(ptr {old_ptr}, i64 {new_size})"));
            em.push(format!("  store i64 1, ptr {grown}"));
            let len_addr = em.fresh_reg();
            em.push(format!("  {len_addr} = getelementptr i8, ptr {grown}, i64 8"));
            em.push(format!("  store i64 {new_len}, ptr {len_addr}"));
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_remove_fresh(&old_ptr, (*elem_ty).clone(), &idx_reg, em, ctx);
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_label} ], [ {fresh}, %{alloc_label} ]"));
            Ok((result, CgType::Array(elem_ty)))
        }
        // `.concat()` — fresh path: a plain runtime-function call. See
        // `StrConcatReuse` for the reuse-in-place half.
        Expr::StrConcat { base, other } => {
            let (b_reg, b_ty) = codegen_value(base, env, em, ctx)?;
            if b_ty != CgType::Str {
                return Err(format!("codegen: `.concat()` requires a Str value, found {b_ty:?}"));
            }
            let (o_reg, o_ty) = codegen_value(other, env, em, ctx)?;
            if o_ty != CgType::Str {
                return Err(format!("codegen: `.concat()` argument must be Str, found {o_ty:?}"));
            }
            let result = em.fresh_reg();
            em.push(format!("  {result} = call ptr @plum_str_concat(ptr {b_reg}, ptr {o_reg})"));
            Ok((result, CgType::Str))
        }
        // The reuse-in-place half of `.concat()` — same refcount-gated
        // "grow in place if uniquely owned, else fall back to a fresh
        // copy" shape as `CtorReuse`/`ArrayPushReuse`, just growing a
        // STRING cell (via `@realloc`) instead of an array/struct one.
        Expr::StrConcatReuse { reuse_of, other } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            if old_ty != CgType::Str {
                return Err(format!("codegen: internal error — StrConcatReuse target {reuse_of:?} is not a Str"));
            }
            let (o_reg, o_ty) = codegen_value(other, env, em, ctx)?;
            if o_ty != CgType::Str {
                return Err(format!("codegen: `.concat()` argument must be Str, found {o_ty:?}"));
            }

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("str_concat_reuse");
            let alloc_label = em.fresh_label("str_concat_fresh");
            let merge_label = em.fresh_label("str_concat_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            let old_len = load_array_len(em, &old_ptr);
            let other_len = load_array_len(em, &o_reg);
            let new_len = em.fresh_reg();
            em.push(format!("  {new_len} = add i64 {old_len}, {other_len}"));
            let bytes_and_nul = em.fresh_reg();
            em.push(format!("  {bytes_and_nul} = add i64 {new_len}, 1"));
            let new_size = em.fresh_reg();
            em.push(format!("  {new_size} = add i64 {bytes_and_nul}, 16"));
            let grown = em.fresh_reg();
            em.push(format!("  {grown} = call ptr @realloc(ptr {old_ptr}, i64 {new_size})"));
            em.push(format!("  store i64 1, ptr {grown}"));
            let len_addr = em.fresh_reg();
            em.push(format!("  {len_addr} = getelementptr i8, ptr {grown}, i64 8"));
            em.push(format!("  store i64 {new_len}, ptr {len_addr}"));
            let tail_off = em.fresh_reg();
            em.push(format!("  {tail_off} = add i64 {old_len}, 16"));
            let tail_dst = em.fresh_reg();
            em.push(format!("  {tail_dst} = getelementptr i8, ptr {grown}, i64 {tail_off}"));
            let other_src = em.fresh_reg();
            em.push(format!("  {other_src} = getelementptr i8, ptr {o_reg}, i64 16"));
            let copy_r = em.fresh_reg();
            em.push(format!("  {copy_r} = call ptr @memcpy(ptr {tail_dst}, ptr {other_src}, i64 {other_len})"));
            let nul_off = em.fresh_reg();
            em.push(format!("  {nul_off} = add i64 {new_len}, 16"));
            let nul_dst = em.fresh_reg();
            em.push(format!("  {nul_dst} = getelementptr i8, ptr {grown}, i64 {nul_off}"));
            em.push(format!("  store i8 0, ptr {nul_dst}"));
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = em.fresh_reg();
            em.push(format!("  {fresh} = call ptr @plum_str_concat(ptr {old_ptr}, ptr {o_reg})"));
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_label} ], [ {fresh}, %{alloc_label} ]"));
            Ok((result, CgType::Str))
        }
        Expr::StrContains { base, needle } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.contains()` requires a Str value, found {bty:?}"));
            }
            let (n, nty) = codegen_value(needle, env, em, ctx)?;
            if nty != CgType::Str {
                return Err(format!("codegen: `.contains()` argument must be Str, found {nty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call i1 @plum_str_contains(ptr {b}, ptr {n})"));
            Ok((r, CgType::Bool))
        }
        Expr::StrStartsWith { base, prefix } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.starts_with()` requires a Str value, found {bty:?}"));
            }
            let (p, pty) = codegen_value(prefix, env, em, ctx)?;
            if pty != CgType::Str {
                return Err(format!("codegen: `.starts_with()` argument must be Str, found {pty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call i1 @plum_str_starts_with(ptr {b}, ptr {p})"));
            Ok((r, CgType::Bool))
        }
        Expr::StrEndsWith { base, suffix } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.ends_with()` requires a Str value, found {bty:?}"));
            }
            let (s, sty) = codegen_value(suffix, env, em, ctx)?;
            if sty != CgType::Str {
                return Err(format!("codegen: `.ends_with()` argument must be Str, found {sty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call i1 @plum_str_ends_with(ptr {b}, ptr {s})"));
            Ok((r, CgType::Bool))
        }
        // `x.to_string()` — dispatch on `base`'s STATIC `CgType` (a
        // stronger, compile-time version of the interpreter's
        // necessarily-dynamic runtime-value dispatch — see `ir::Expr::
        // ToString`'s own doc comment). `Int`/`Float` render via a
        // declared libc `@snprintf` into a stack buffer, then copied
        // into a fresh string cell; `Bool` via two static string
        // constants; `Str` via a genuine fresh COPY (never reuse-in-
        // place — `ToString`'s contract is "always produces a NEW
        // cell," matching `ir::Expr::ToString`'s own doc comment).
        Expr::ToString { base } => {
            let (reg, ty) = codegen_value(base, env, em, ctx)?;
            match ty {
                CgType::Int => {
                    let fmt = em.fresh_string_global(ctx.fn_name, b"%lld\0");
                    let buf = em.fresh_reg();
                    em.push(format!("  {buf} = alloca [32 x i8]"));
                    let n = em.fresh_reg();
                    em.push(format!(
                        "  {n} = call i32 (ptr, i64, ptr, ...) @snprintf(ptr {buf}, i64 32, ptr {fmt}, i64 {reg})"
                    ));
                    let len = em.fresh_reg();
                    em.push(format!("  {len} = sext i32 {n} to i64"));
                    let cell = em.fresh_reg();
                    em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {len})"));
                    let dst = em.fresh_reg();
                    em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
                    let copy_r = em.fresh_reg();
                    em.push(format!("  {copy_r} = call ptr @memcpy(ptr {dst}, ptr {buf}, i64 {len})"));
                    Ok((cell, CgType::Str))
                }
                CgType::Float => {
                    let fmt = em.fresh_string_global(ctx.fn_name, b"%f\0");
                    let buf = em.fresh_reg();
                    em.push(format!("  {buf} = alloca [64 x i8]"));
                    let n = em.fresh_reg();
                    em.push(format!(
                        "  {n} = call i32 (ptr, i64, ptr, ...) @snprintf(ptr {buf}, i64 64, ptr {fmt}, double {reg})"
                    ));
                    let len = em.fresh_reg();
                    em.push(format!("  {len} = sext i32 {n} to i64"));
                    let cell = em.fresh_reg();
                    em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {len})"));
                    let dst = em.fresh_reg();
                    em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
                    let copy_r = em.fresh_reg();
                    em.push(format!("  {copy_r} = call ptr @memcpy(ptr {dst}, ptr {buf}, i64 {len})"));
                    Ok((cell, CgType::Str))
                }
                CgType::Bool => {
                    let true_label = em.fresh_label("to_string_true");
                    let false_label = em.fresh_label("to_string_false");
                    let merge_label = em.fresh_label("to_string_merge");
                    em.push(format!("  br i1 {reg}, label %{true_label}, label %{false_label}"));

                    em.start_block(&true_label);
                    let true_bytes = b"true";
                    let true_cell = em.fresh_reg();
                    em.push(format!("  {true_cell} = call ptr @plum_alloc_str(i64 {})", true_bytes.len()));
                    let true_g = em.fresh_string_global(ctx.fn_name, true_bytes);
                    let true_dst = em.fresh_reg();
                    em.push(format!("  {true_dst} = getelementptr i8, ptr {true_cell}, i64 16"));
                    let true_copy = em.fresh_reg();
                    em.push(format!(
                        "  {true_copy} = call ptr @memcpy(ptr {true_dst}, ptr {true_g}, i64 {})",
                        true_bytes.len()
                    ));
                    em.push(format!("  br label %{merge_label}"));

                    em.start_block(&false_label);
                    let false_bytes = b"false";
                    let false_cell = em.fresh_reg();
                    em.push(format!("  {false_cell} = call ptr @plum_alloc_str(i64 {})", false_bytes.len()));
                    let false_g = em.fresh_string_global(ctx.fn_name, false_bytes);
                    let false_dst = em.fresh_reg();
                    em.push(format!("  {false_dst} = getelementptr i8, ptr {false_cell}, i64 16"));
                    let false_copy = em.fresh_reg();
                    em.push(format!(
                        "  {false_copy} = call ptr @memcpy(ptr {false_dst}, ptr {false_g}, i64 {})",
                        false_bytes.len()
                    ));
                    em.push(format!("  br label %{merge_label}"));

                    em.start_block(&merge_label);
                    let result = em.fresh_reg();
                    em.push(format!(
                        "  {result} = phi ptr [ {true_cell}, %{true_label} ], [ {false_cell}, %{false_label} ]"
                    ));
                    Ok((result, CgType::Str))
                }
                CgType::Str => {
                    let len = load_array_len(em, &reg);
                    let cell = em.fresh_reg();
                    em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {len})"));
                    let dst = em.fresh_reg();
                    em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
                    let src = em.fresh_reg();
                    em.push(format!("  {src} = getelementptr i8, ptr {reg}, i64 16"));
                    let copy_r = em.fresh_reg();
                    em.push(format!("  {copy_r} = call ptr @memcpy(ptr {dst}, ptr {src}, i64 {len})"));
                    Ok((cell, CgType::Str))
                }
                other => Err(format!("codegen: `.to_string()` is not supported for {other:?}")),
            }
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
        // Generalized from a hardcoded `Heap`-only check to a 3-way
        // `Heap`/`Str`/`Array(elem)` dispatch — `Inc` reuses `@plum_rc_
        // inc` unchanged for all three (increment logic never differs
        // by cell shape, only decrement/release does), `Dec` dispatches
        // to the shape-appropriate decrement function via `crate::
        // dec_fn_for` (the SAME lookup `plum_release_fields`'s own
        // per-field dispatch uses, in lib.rs — the two are guaranteed
        // to always agree on which function decrements which shape).
        Expr::RcAnnotated { op, target, rest } => {
            let (reg, ty) = env.get(target).cloned().ok_or_else(|| format!("codegen: unbound variable {target:?}"))?;
            match op {
                RcOp::Inc => {
                    if !matches!(ty, CgType::Heap | CgType::Str | CgType::Array(_)) {
                        return Err(format!(
                            "codegen: internal error — RcAnnotated target {target:?} is not heap-shaped ({ty:?})"
                        ));
                    }
                    em.push(format!("  call void @plum_rc_inc(ptr {reg})"));
                }
                RcOp::Dec => {
                    let dec_fn = crate::dec_fn_for(&ty).ok_or_else(|| {
                        format!("codegen: internal error — RcAnnotated target {target:?} is not heap-shaped ({ty:?})")
                    })?;
                    if let CgType::Array(elem) = &ty {
                        register_array_elem(ctx, elem);
                    }
                    em.push(format!("  call void {dec_fn}(ptr {reg})"));
                }
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
                        let val = load_field_word(em, &scrutinee_ptr, i, fty.clone());
                        if *fty == CgType::Heap {
                            em.push(format!("  call void @plum_rc_inc(ptr {val})"));
                        }
                        arm_env.insert(name.clone(), (val, fty.clone()));
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
                    .map(|(_, ty, _)| ty.clone())
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
