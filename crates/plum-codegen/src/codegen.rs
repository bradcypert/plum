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
use plum_ir::ir;
use plum_ir::ir::{BinOp, Expr, MatchArm, PrimTy, RcOp, SelectArm, UnOp};
use std::collections::{BTreeSet, HashMap, HashSet};

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
    /// Whether the function CURRENTLY being emitted is a closure
    /// body's own generated function (`emit_closure_body_fn`), rather
    /// than an ordinary top-level function/`@plum_init_globals`/spawn
    /// entry. A closure body's REAL LLVM prototype always has an
    /// implicit leading `ptr %env` parameter that `caller_sig` above
    /// does NOT count (`caller_sig` only ever records the closure's
    /// own DECLARED params) — so `caller_sig == callee_sig` can be
    /// TRUE even though the real parameter counts differ by one,
    /// letting an invalid `musttail call` through that `clang`/LLVM
    /// then rejects (a real bug, found via an actual compile failure:
    /// `cannot guarantee tail call due to mismatched parameter
    /// counts`, from a closure passed to `.fold()` whose body tail-
    /// called a top-level function with the same declared shape).
    /// `musttail` is unconditionally disallowed from a closure body —
    /// see `codegen_call`'s own use of this field — falling back to an
    /// ordinary `call` + `ret`, still correct, just not `musttail`-
    /// guaranteed.
    pub(crate) is_closure_body: bool,
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
    /// A per-PROGRAM (not per-function) monotonic counter used to name
    /// every closure-literal-site-generated function uniquely
    /// (`@closure$<fn_name>$<K>`) — see `codegen_closure_literal`'s doc
    /// comment. Global rather than per-enclosing-function purely
    /// because it's simpler to thread one shared counter than to reset
    /// it at every `emit_function` call AND every nested closure body's
    /// own `Ctx` (a closure literal nested inside another closure's
    /// body still needs a globally-unique name); `fn_name` is still
    /// folded into the generated name for human readability even though
    /// `K` alone would already be enough for uniqueness.
    pub(crate) closure_counter: &'a std::cell::RefCell<usize>,
    /// Every closure-literal-site-generated function's/release
    /// function's/trampoline's full LLVM `define { ... }` text,
    /// collected across the WHOLE program — spliced into `emit_
    /// program`'s output alongside `emit_array_release_fns`'s own
    /// per-element-type functions. Parallel to how string-literal
    /// globals are already collected per-function (`Emitter::
    /// string_globals`) — this is the program-wide analogue, since a
    /// closure literal's generated function is a top-level `define`,
    /// not something that can live inside `%v<N>`-style function-body
    /// text.
    pub(crate) closure_defs: &'a std::cell::RefCell<Vec<String>>,
    /// Bare-top-level-function-name -> its already-generated
    /// `@trampoline$<name>` function's name — memoized so referencing
    /// the same top-level function as a VALUE more than once (`let f =
    /// someFn; let g = someFn; ...`) only ever generates ONE trampoline
    /// definition, not one per reference (each reference still
    /// allocates its OWN fresh closure CELL at its own use site,
    /// exactly like any other closure literal would — only the
    /// (stateless) trampoline FUNCTION TEXT itself is shared/memoized).
    /// See `codegen_bare_fn_value`.
    pub(crate) trampolines: &'a std::cell::RefCell<HashMap<String, String>>,
    /// Set to `true` the first time codegen actually walks a `Spawn`/
    /// `TaskJoin` node — read by `lib.rs::emit_program` AFTER every
    /// function body has been emitted to decide whether the spawn
    /// runtime (`pthread_create`/`pthread_join` declarations, the
    /// `@plum_deepcopy_*` functions) and the whole-program closure/task-
    /// field rejection check are needed at all. Same shared-`RefCell`-
    /// behind-`&Ctx` shape as `needed_arrays`, for the same reason.
    pub(crate) needs_spawn_runtime: &'a std::cell::RefCell<bool>,
    /// Set to `true` the first time codegen actually walks a `Channel`/
    /// `ChannelSend`/`ChannelRecv`/`Select` node — the channel
    /// counterpart to `needs_spawn_runtime` immediately above, gated
    /// independently (see `lib.rs::emit_program`'s own doc comment on
    /// why): a program using channels but no `spawn` still needs the
    /// channel-queue runtime (and the shared deep-copy runtime) but not
    /// `pthread_create`/`pthread_join`, and vice versa.
    pub(crate) needs_channel_runtime: &'a std::cell::RefCell<bool>,
    /// Set to `true` the first time codegen actually walks a `Read
    /// FileRaw`/`WriteFileRaw` node — the file-I/O counterpart to
    /// `needs_spawn_runtime`/`needs_channel_runtime`, gated
    /// independently for the same reason: a program that never
    /// touches a file pays zero cost (no `@fopen`/`@fread`/etc.
    /// declares at all).
    pub(crate) needs_file_io_runtime: &'a std::cell::RefCell<bool>,
    /// Every declared `extern "C"` function, keyed by name — built ONCE
    /// in `lib.rs::emit_program` from `program.externs` (already the
    /// complete, explicit list — no reactive discovery needed, unlike
    /// `needed_arrays`/`trampolines`/etc.) and threaded through
    /// unchanged, matching `sigs`/`tag_fields`'s own "built once,
    /// referenced read-only everywhere" shape. Looked up by
    /// `codegen_extern_call` to recover an `ExternCall` node's full
    /// declared signature (`ir::Expr::ExternCall` itself only carries
    /// the callee NAME and argument expressions, not its types).
    pub(crate) externs: &'a HashMap<String, ir::ExternFn>,
    /// Bare-top-level-function-name -> its already-generated
    /// `@c_trampoline$<name>` function's name — the C-callback
    /// counterpart to `trampolines`, kept in a SEPARATE table rather
    /// than reusing it: an ordinary closure trampoline always has a
    /// leading `ptr %env` parameter (part of every Plum closure's own
    /// calling convention), while a C-callback trampoline has NONE at
    /// all — a real C API has no way to supply one. The same target
    /// function referenced BOTH as an ordinary higher-order Plum value
    /// AND as a callback argument in the same program needs two
    /// DIFFERENT generated functions; conflating the two tables under
    /// one key would risk silently reusing the wrong shape for one of
    /// the two call sites — a genuine calling-convention bug, not just
    /// a naming collision. See `codegen_callback_arg`/`emit_c_callback_
    /// trampoline_fn`.
    pub(crate) c_callback_trampolines: &'a std::cell::RefCell<HashMap<String, String>>,
    /// Every top-level `Global`'s name -> its concrete `CgType` — built
    /// ONCE in `lib.rs::emit_program` (mirroring `sigs`'s own "built
    /// once, referenced read-only everywhere" shape) from the caller's
    /// (`plumc`'s) own parallel derivation off `Infer::infer_program`'s
    /// `types` map. This is the THIRD, and last, tier of `Expr::Var`
    /// resolution (`env` -> `sigs` -> `globals` -> error) — see
    /// `codegen_value`'s `Expr::Var` arm. Deliberately NEVER consulted
    /// by anything that populates an `Env`: a global is always resolved
    /// through THIS table, never inserted as an `env` entry anywhere,
    /// which is exactly what makes `free_vars_scoped` need zero changes
    /// to correctly exclude a global from a closure's capture set (see
    /// that function's own `Expr::Var` arm — a name only becomes a
    /// capture candidate if `env.contains_key(name)`) — a global has a
    /// fixed, whole-program-lifetime address and needs no capture/
    /// snapshot at all, exactly like a bare function reference already
    /// doesn't.
    pub(crate) globals: &'a HashMap<String, CgType>,
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

    /// Reserves a line slot at the CURRENT end of `lines`, returning its
    /// index for a later `patch_line` call — needed ONLY by `For`
    /// codegen's loop-header phis: a phi's operand for the "coming from
    /// the body" edge is the body's FINAL register, which isn't known
    /// until after the body (and the header's own `icmp`/`br`, which
    /// must textually precede the body since it's what jumps INTO it)
    /// has already been emitted. `If`/`Match` never hit this — their
    /// merge block is entered exactly once, strictly after both value-
    /// producing paths are already fully emitted, so their phi operands
    /// are always known before the phi line itself needs to be pushed.
    /// The reserved line's TEXT is a placeholder (never emitted as-is —
    /// `patch_line` must be called before this `Emitter`'s `lines` are
    /// read by anything downstream) purely so the header's OWN
    /// instructions (the `icmp`/`br` that come after it) still land at
    /// the correct positions relative to it.
    fn reserve_line(&mut self) -> usize {
        let idx = self.lines.len();
        self.lines.push(String::new());
        idx
    }

    /// Fills in a line previously reserved by `reserve_line` — see that
    /// method's doc comment.
    fn patch_line(&mut self, idx: usize, line: impl Into<String>) {
        self.lines[idx] = line.into();
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
    // `Str` `==`/`!=` — handled BEFORE the generic `(op, ty)` match below
    // because it needs a `call`-shaped instruction (`@plum_str_eq`), not
    // a single binary op, so it doesn't fit that match's tuple-returning
    // arms. Any other `BinOp` on `CgType::Str` (ordering comparisons,
    // `+`, etc.) falls through to the generic match's `Err` fallback,
    // unchanged — Str only ever supported `Eq`/`Ne` at the type-checker
    // level in the first place.
    if ty == CgType::Str && (op == BinOp::Eq || op == BinOp::Ne) {
        let reg = em.fresh_reg();
        em.push(format!("  {reg} = call i1 @plum_str_eq(ptr {l}, ptr {r})"));
        if op == BinOp::Ne {
            let negated = em.fresh_reg();
            em.push(format!("  {negated} = xor i1 {reg}, 1"));
            return Ok((negated, CgType::Bool));
        }
        return Ok((reg, CgType::Bool));
    }
    // Struct/enum (`Heap`) and array `==`/`!=` — the exact same "call-
    // shaped instruction, not a bare `icmp`" precedent as `Str` above,
    // dispatching to `@plum_struct_eq` (one generic function for every
    // struct/enum shape) or `@plum_array_eq_<mangled>` (one per
    // distinct element type, see `crate::eq_fn_for`/`emit_array_eq_
    // fns`). Tuples are NOT handled here — `crate::eq_fn_for` has no
    // `Tuple` case because `CgType` itself has no `Tuple` variant (see
    // `plumc::plum_type_to_cg_type`'s own doc comment) — so a tuple
    // `==` still falls through to this function's generic `Err`
    // fallback below, unchanged; `satisfies_bound` now rejects it
    // earlier, at type-checking time, so that fallback should never
    // actually be reached in a well-typed program.
    if matches!(ty, CgType::Heap | CgType::Array(_)) && (op == BinOp::Eq || op == BinOp::Ne) {
        if let Some(eq_fn) = crate::eq_fn_for(&ty) {
            let reg = em.fresh_reg();
            em.push(format!("  {reg} = call i1 {eq_fn}(ptr {l}, ptr {r})"));
            if op == BinOp::Ne {
                let negated = em.fresh_reg();
                em.push(format!("  {negated} = xor i1 {reg}, 1"));
                return Ok((negated, CgType::Bool));
            }
            return Ok((reg, CgType::Bool));
        }
    }
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = inttoptr i64 {word} to ptr"));
            r
        }
    }
}

/// The closure-cell counterpart to `field_byte_offset` — a closure
/// cell's header is 3 words (`refcount`, `code_ptr`, `release_fn_ptr`
/// = 24 bytes), one word wider than `Ctor`/array cells (16 bytes), so
/// captured-field slots start at byte 24, not 16 — see `emit_runtime`'s
/// closure-cell-layout doc comment for why this extra header word is
/// genuinely necessary (release can't be resolved from the static
/// `CgType` alone, unlike every other heap shape). A REAL, not
/// cosmetic, offset difference — reusing `field_byte_offset` here would
/// silently read/write into the `release_fn_ptr` word instead of the
/// first actual capture.
fn closure_field_byte_offset(index: usize) -> i64 {
    24 + (index as i64) * 8
}

/// The closure-capture counterpart to `store_field_word` — same
/// uniform-word conversion, just at `closure_field_byte_offset`'s
/// wider-header offset instead of `field_byte_offset`'s.
fn store_closure_capture(em: &mut Emitter, cell_ptr: &str, index: usize, value: &str, ty: CgType) {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {cell_ptr}, i64 {}", closure_field_byte_offset(index)));
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = ptrtoint ptr {value} to i64"));
            r
        }
    };
    em.push(format!("  store i64 {word}, ptr {addr}"));
}

/// The closure-capture counterpart to `load_field_word` — see
/// `store_closure_capture`'s doc comment.
fn load_closure_capture(em: &mut Emitter, cell_ptr: &str, index: usize, expected_ty: CgType) -> String {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {cell_ptr}, i64 {}", closure_field_byte_offset(index)));
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
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
        PrimTy::Unit => CgType::Unit,
        PrimTy::Str => CgType::Str,
        PrimTy::Array(inner) => CgType::Array(Box::new(prim_ty_to_cg_type(inner))),
        PrimTy::Heap => CgType::Heap,
        PrimTy::Closure(params, ret) => {
            CgType::Closure(params.iter().map(prim_ty_to_cg_type).collect(), Box::new(prim_ty_to_cg_type(ret)))
        }
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
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

// --- closures ---
//
// See lib.rs's `CgType::Closure` doc comment and `emit_runtime`'s own
// closure-cell-layout doc comment for the `{ i64 refcount, i64
// code_ptr, i64 release_fn_ptr, i64 captured[N] }` shape these helpers
// operate on.

/// Collects every free variable of a closure literal's `body` — every
/// `Var(name)` reachable from `env` (the closure's CREATING scope) that
/// ISN'T shadowed by something introduced WITHIN `body` itself (the
/// closure's own params, a nested `Let`/`Match` binding/`For` loop
/// variable/nested closure's own params, ...). This is a purely
/// structural walk (mirroring `fbip.rs`'s own `expr_mentions_var`'s
/// exhaustive-`Expr`-variant shape) — deliberately conservative in the
/// SAME direction that module already established for `For`/`Closure`
/// bodies (treats every non-shadowed mention as a genuine capture
/// candidate, never tries to prove a mention is dead code). Returns a
/// `BTreeSet` (sorted by name) rather than a `HashSet` — this ORDER
/// becomes the capture cell's field-index assignment, and needs to be
/// reproducible across runs for deterministic `.ll` output, matching
/// `intern_tags`'s own established "sort for reproducibility" precedent.
fn free_vars(expr: &Expr, env: &Env, out: &mut BTreeSet<String>) {
    free_vars_scoped(expr, env, &HashSet::new(), out);
}

fn free_vars_scoped(expr: &Expr, env: &Env, local: &HashSet<String>, out: &mut BTreeSet<String>) {
    let candidate = |name: &str, local: &HashSet<String>, out: &mut BTreeSet<String>| {
        if !local.contains(name) && env.contains_key(name) {
            out.insert(name.to_string());
        }
    };
    match expr {
        Expr::Var(name) => candidate(name, local, out),
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) | Expr::Channel => {}
        Expr::Unary(_, e) | Expr::AsCStr(e) => free_vars_scoped(e, env, local, out),
        Expr::Binary(_, l, r) => {
            free_vars_scoped(l, env, local, out);
            free_vars_scoped(r, env, local, out);
        }
        Expr::Let { name, value, body } => {
            free_vars_scoped(value, env, local, out);
            let mut inner = local.clone();
            inner.insert(name.clone());
            free_vars_scoped(body, env, &inner, out);
        }
        Expr::If { cond, then_branch, else_branch } => {
            free_vars_scoped(cond, env, local, out);
            free_vars_scoped(then_branch, env, local, out);
            free_vars_scoped(else_branch, env, local, out);
        }
        Expr::Call { callee, args } => {
            free_vars_scoped(callee, env, local, out);
            for a in args {
                free_vars_scoped(a, env, local, out);
            }
        }
        Expr::ExternCall { args, .. } => {
            for a in args {
                free_vars_scoped(a, env, local, out);
            }
        }
        Expr::Ctor { fields, .. } => {
            for f in fields {
                free_vars_scoped(f, env, local, out);
            }
        }
        Expr::CtorReuse { reuse_of, fields, .. } => {
            candidate(reuse_of, local, out);
            for f in fields {
                free_vars_scoped(f, env, local, out);
            }
        }
        Expr::Match { scrutinee, arms } => {
            free_vars_scoped(scrutinee, env, local, out);
            for arm in arms {
                let mut inner = local.clone();
                for b in &arm.bindings {
                    inner.insert(b.clone());
                }
                if let Some(g) = &arm.guard {
                    free_vars_scoped(g, env, &inner, out);
                }
                free_vars_scoped(&arm.body, env, &inner, out);
            }
        }
        Expr::RcAnnotated { target, rest, .. } => {
            candidate(target, local, out);
            free_vars_scoped(rest, env, local, out);
        }
        Expr::For { var, start, end, body } => {
            free_vars_scoped(start, env, local, out);
            free_vars_scoped(end, env, local, out);
            let mut inner = local.clone();
            inner.insert(var.clone());
            free_vars_scoped(body, env, &inner, out);
        }
        Expr::Closure { params, body, .. } => {
            let mut inner = local.clone();
            for p in params {
                inner.insert(p.clone());
            }
            free_vars_scoped(body, env, &inner, out);
        }
        Expr::Assign { name, value, rest } => {
            candidate(name, local, out);
            free_vars_scoped(value, env, local, out);
            free_vars_scoped(rest, env, local, out);
        }
        Expr::Spawn { block } => free_vars_scoped(block, env, local, out),
        Expr::TaskJoin { task } => free_vars_scoped(task, env, local, out),
        Expr::ChannelSend { sender, value } => {
            free_vars_scoped(sender, env, local, out);
            free_vars_scoped(value, env, local, out);
        }
        Expr::ChannelRecv { receiver } => free_vars_scoped(receiver, env, local, out),
        Expr::Select { arms } => {
            for arm in arms {
                free_vars_scoped(&arm.receiver, env, local, out);
                free_vars_scoped(&arm.body, env, local, out);
            }
        }
        Expr::Index { base, index } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(index, env, local, out);
        }
        Expr::ArrayLen { array } | Expr::ArrayPop { array } => free_vars_scoped(array, env, local, out),
        Expr::ArrayPush { array, value } => {
            free_vars_scoped(array, env, local, out);
            free_vars_scoped(value, env, local, out);
        }
        Expr::ArraySet { array, index, value } => {
            free_vars_scoped(array, env, local, out);
            free_vars_scoped(index, env, local, out);
            free_vars_scoped(value, env, local, out);
        }
        Expr::ArrayRemove { array, index } => {
            free_vars_scoped(array, env, local, out);
            free_vars_scoped(index, env, local, out);
        }
        Expr::ArrayPushReuse { reuse_of, value } => {
            candidate(reuse_of, local, out);
            free_vars_scoped(value, env, local, out);
        }
        Expr::ArrayPopReuse { reuse_of } => candidate(reuse_of, local, out),
        Expr::ArraySetReuse { reuse_of, index, value } => {
            candidate(reuse_of, local, out);
            free_vars_scoped(index, env, local, out);
            free_vars_scoped(value, env, local, out);
        }
        Expr::ArrayRemoveReuse { reuse_of, index } => {
            candidate(reuse_of, local, out);
            free_vars_scoped(index, env, local, out);
        }
        Expr::StrConcat { base, other } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(other, env, local, out);
        }
        Expr::StrConcatReuse { reuse_of, other } => {
            candidate(reuse_of, local, out);
            free_vars_scoped(other, env, local, out);
        }
        Expr::StrRunes { base } | Expr::StrTrim { base } | Expr::StrToUpper { base } | Expr::StrToLower { base } | Expr::ToString { base } => {
            free_vars_scoped(base, env, local, out)
        }
        Expr::StrTrimReuse { reuse_of } | Expr::StrToUpperReuse { reuse_of } | Expr::StrToLowerReuse { reuse_of } => {
            candidate(reuse_of, local, out)
        }
        Expr::StrSplit { base, sep } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(sep, env, local, out);
        }
        Expr::StrContains { base, needle } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(needle, env, local, out);
        }
        Expr::StrStartsWith { base, prefix } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(prefix, env, local, out);
        }
        Expr::StrEndsWith { base, suffix } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(suffix, env, local, out);
        }
        Expr::StrReplace { base, from, to } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(from, env, local, out);
            free_vars_scoped(to, env, local, out);
        }
        Expr::StrReplaceReuse { reuse_of, from, to } => {
            candidate(reuse_of, local, out);
            free_vars_scoped(from, env, local, out);
            free_vars_scoped(to, env, local, out);
        }
        Expr::RefNew { value } => free_vars_scoped(value, env, local, out),
        Expr::RefGet { base } => free_vars_scoped(base, env, local, out),
        Expr::RefSet { base, value } => {
            free_vars_scoped(base, env, local, out);
            free_vars_scoped(value, env, local, out);
        }
        Expr::ReadFileRaw { path } => free_vars_scoped(path, env, local, out),
        Expr::WriteFileRaw { path, contents } => {
            free_vars_scoped(path, env, local, out);
            free_vars_scoped(contents, env, local, out);
        }
        Expr::PanicRaw { message } => free_vars_scoped(message, env, local, out),
    }
}

/// Finds every outer-scope name a `For` loop's `body` reassigns via
/// `Assign` — the write-target counterpart to `free_vars`/`free_vars_
/// scoped` above (which collect READS), needed so `codegen_expr`'s
/// `For` arm knows which names must get their own loop-header phi (see
/// this module's `Expr::For` codegen doc comment). Uses the SAME
/// `Let`/`For`/`Match`-introduces-shadowing rule `free_vars_scoped`
/// already established: an `Assign` to a name that some INNER `Let`/
/// `For`/`Match` binding already shadows refers to that inner binding,
/// not an outer loop-carried one, so it's correctly excluded here too.
///
/// `Expr::Closure` is a deliberate HARD STOP — this does NOT recurse
/// into a closure's body at all (not even to track its params as
/// further shadowing, since nothing inside it is examined in the first
/// place). This is a structural necessity, not a shortcut: a closure
/// compiles to its own top-level `define` with a wholly fresh `Env`
/// populated only from byval captures loaded at CALL time, and it can
/// escape and be called after the loop that created it has already
/// finished (even after the enclosing function has returned) — there is
/// no point in program order where "write this closure's `Assign` back
/// into the loop's phi" could have a coherent meaning. An `Assign`
/// inside a closure body behaves exactly like it already does for any
/// non-loop closure (an ordinary reassignment within THAT closure's own
/// call-time `Env`) — not a new gap, just a boundary this loop-carried
/// mechanism doesn't reach across. By contrast, nested `for` loops are
/// NOT a stop point (recursed into normally) — this is what makes a
/// nested-loop shared accumulator work for free: each loop level's own
/// `For` codegen independently calls `assigned_vars` on its OWN body,
/// so an accumulator reassigned inside a doubly-nested loop is
/// discovered — and gets its own independent header phi — at BOTH
/// levels.
fn assigned_vars(expr: &Expr) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    assigned_vars_scoped(expr, &HashSet::new(), &mut out);
    out
}

fn assigned_vars_scoped(expr: &Expr, local: &HashSet<String>, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) | Expr::Channel | Expr::Var(_) => {}
        Expr::Unary(_, e) | Expr::AsCStr(e) => assigned_vars_scoped(e, local, out),
        Expr::Binary(_, l, r) => {
            assigned_vars_scoped(l, local, out);
            assigned_vars_scoped(r, local, out);
        }
        Expr::Let { name, value, body } => {
            assigned_vars_scoped(value, local, out);
            let mut inner = local.clone();
            inner.insert(name.clone());
            assigned_vars_scoped(body, &inner, out);
        }
        Expr::If { cond, then_branch, else_branch } => {
            assigned_vars_scoped(cond, local, out);
            assigned_vars_scoped(then_branch, local, out);
            assigned_vars_scoped(else_branch, local, out);
        }
        Expr::Call { callee, args } => {
            assigned_vars_scoped(callee, local, out);
            for a in args {
                assigned_vars_scoped(a, local, out);
            }
        }
        Expr::ExternCall { args, .. } => {
            for a in args {
                assigned_vars_scoped(a, local, out);
            }
        }
        Expr::Ctor { fields, .. } => {
            for f in fields {
                assigned_vars_scoped(f, local, out);
            }
        }
        Expr::CtorReuse { fields, .. } => {
            for f in fields {
                assigned_vars_scoped(f, local, out);
            }
        }
        Expr::Match { scrutinee, arms } => {
            assigned_vars_scoped(scrutinee, local, out);
            for arm in arms {
                let mut inner = local.clone();
                for b in &arm.bindings {
                    inner.insert(b.clone());
                }
                if let Some(g) = &arm.guard {
                    assigned_vars_scoped(g, &inner, out);
                }
                assigned_vars_scoped(&arm.body, &inner, out);
            }
        }
        Expr::RcAnnotated { rest, .. } => assigned_vars_scoped(rest, local, out),
        Expr::For { start, end, var, body } => {
            assigned_vars_scoped(start, local, out);
            assigned_vars_scoped(end, local, out);
            let mut inner = local.clone();
            inner.insert(var.clone());
            assigned_vars_scoped(body, &inner, out);
        }
        // Hard stop — see this function's own doc comment.
        Expr::Closure { .. } => {}
        Expr::Assign { name, value, rest } => {
            if !local.contains(name) {
                out.insert(name.clone());
            }
            assigned_vars_scoped(value, local, out);
            assigned_vars_scoped(rest, local, out);
        }
        Expr::Spawn { block } => assigned_vars_scoped(block, local, out),
        Expr::TaskJoin { task } => assigned_vars_scoped(task, local, out),
        Expr::ChannelSend { sender, value } => {
            assigned_vars_scoped(sender, local, out);
            assigned_vars_scoped(value, local, out);
        }
        Expr::ChannelRecv { receiver } => assigned_vars_scoped(receiver, local, out),
        Expr::Select { arms } => {
            for arm in arms {
                assigned_vars_scoped(&arm.receiver, local, out);
                assigned_vars_scoped(&arm.body, local, out);
            }
        }
        Expr::Index { base, index } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(index, local, out);
        }
        Expr::ArrayLen { array } | Expr::ArrayPop { array } => assigned_vars_scoped(array, local, out),
        Expr::ArrayPush { array, value } => {
            assigned_vars_scoped(array, local, out);
            assigned_vars_scoped(value, local, out);
        }
        Expr::ArraySet { array, index, value } => {
            assigned_vars_scoped(array, local, out);
            assigned_vars_scoped(index, local, out);
            assigned_vars_scoped(value, local, out);
        }
        Expr::ArrayRemove { array, index } => {
            assigned_vars_scoped(array, local, out);
            assigned_vars_scoped(index, local, out);
        }
        Expr::ArrayPushReuse { value, .. } => assigned_vars_scoped(value, local, out),
        Expr::ArrayPopReuse { .. } => {}
        Expr::ArraySetReuse { index, value, .. } => {
            assigned_vars_scoped(index, local, out);
            assigned_vars_scoped(value, local, out);
        }
        Expr::ArrayRemoveReuse { index, .. } => assigned_vars_scoped(index, local, out),
        Expr::StrConcat { base, other } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(other, local, out);
        }
        Expr::StrConcatReuse { other, .. } => assigned_vars_scoped(other, local, out),
        Expr::StrRunes { base } | Expr::StrTrim { base } | Expr::StrToUpper { base } | Expr::StrToLower { base } | Expr::ToString { base } => {
            assigned_vars_scoped(base, local, out)
        }
        Expr::StrTrimReuse { .. } | Expr::StrToUpperReuse { .. } | Expr::StrToLowerReuse { .. } => {}
        Expr::StrSplit { base, sep } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(sep, local, out);
        }
        Expr::StrContains { base, needle } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(needle, local, out);
        }
        Expr::StrStartsWith { base, prefix } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(prefix, local, out);
        }
        Expr::StrEndsWith { base, suffix } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(suffix, local, out);
        }
        Expr::StrReplace { base, from, to } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(from, local, out);
            assigned_vars_scoped(to, local, out);
        }
        Expr::StrReplaceReuse { from, to, .. } => {
            assigned_vars_scoped(from, local, out);
            assigned_vars_scoped(to, local, out);
        }
        Expr::RefNew { value } => assigned_vars_scoped(value, local, out),
        Expr::RefGet { base } => assigned_vars_scoped(base, local, out),
        Expr::RefSet { base, value } => {
            assigned_vars_scoped(base, local, out);
            assigned_vars_scoped(value, local, out);
        }
        Expr::ReadFileRaw { path } => assigned_vars_scoped(path, local, out),
        Expr::WriteFileRaw { path, contents } => {
            assigned_vars_scoped(path, local, out);
            assigned_vars_scoped(contents, local, out);
        }
        Expr::PanicRaw { message } => assigned_vars_scoped(message, local, out),
    }
}

/// Converts a resolved `PrimTy` (see `ir::Expr::Closure`'s doc comment)
/// into the `CgType` codegen actually works with — the closure-typing
/// counterpart to `prim_ty_to_cg_type`... wait, this IS `prim_ty_to_
/// cg_type` (defined above); closure param/return types reuse it
/// directly, no separate conversion needed since `PrimTy::Closure` was
/// added specifically so this one function covers both empty-array and
/// closure typing uniformly.
fn resolve_closure_sig(
    param_types: &Option<Vec<PrimTy>>,
    ret_type: &Option<PrimTy>,
    n_params: usize,
) -> Result<(Vec<CgType>, CgType), String> {
    let param_types = param_types.as_ref().ok_or_else(|| {
        "codegen: internal error — a closure literal reached codegen without resolved param types (was \
         `plum_types::Infer`/`plum_ir::lower::LoweringContext::with_closure_types` run before lowering?)"
            .to_string()
    })?;
    let ret_type = ret_type.as_ref().ok_or_else(|| {
        "codegen: internal error — a closure literal reached codegen without a resolved return type".to_string()
    })?;
    if param_types.len() != n_params {
        return Err(format!(
            "codegen: internal error — closure literal has {n_params} param(s) but {} resolved param type(s)",
            param_types.len()
        ));
    }
    Ok((param_types.iter().map(prim_ty_to_cg_type).collect(), prim_ty_to_cg_type(ret_type)))
}

/// Generates the per-closure-literal-site LLVM function `@closure$<fn>
/// $<K>(ptr %env, params...) -> ret`: loads each capture back out of
/// `%env` (the closure's OWN cell pointer at call time) by index into a
/// FRESH `Env` — real lexical scoping, NOT inherited from the enclosing
/// function, matching a genuine separate `define` — then adds the
/// closure's own params, then codegens `body` exactly like an ordinary
/// function (tail position, its own `ret`).
fn emit_closure_body_fn(
    fn_name: &str,
    params: &[String],
    param_types: &[CgType],
    ret_type: &CgType,
    captures: &[(String, CgType)],
    body: &Expr,
    ctx: &Ctx,
) -> Result<String, String> {
    let sig = FnSig { params: param_types.to_vec(), ret: ret_type.clone() };
    let mut em = Emitter::new();
    let mut env: Env = HashMap::new();
    for (i, (name, ty)) in captures.iter().enumerate() {
        let val = load_closure_capture(&mut em, "%env", i, ty.clone());
        env.insert(name.clone(), (val, ty.clone()));
    }
    let mut param_decls = vec!["ptr %env".to_string()];
    for (name, ty) in params.iter().zip(param_types) {
        env.insert(name.clone(), (format!("%{name}"), ty.clone()));
        param_decls.push(format!("{} %{name}", ty.llvm_type()));
    }
    let inner_ctx = Ctx {
        sigs: ctx.sigs,
        caller_sig: &sig,
        is_closure_body: true,
        tag_ids: ctx.tag_ids,
        tag_fields: ctx.tag_fields,
        fn_name,
        needed_arrays: ctx.needed_arrays,
        closure_counter: ctx.closure_counter,
        closure_defs: ctx.closure_defs,
        trampolines: ctx.trampolines,
        needs_spawn_runtime: ctx.needs_spawn_runtime,
        needs_channel_runtime: ctx.needs_channel_runtime,
        needs_file_io_runtime: ctx.needs_file_io_runtime,
        externs: ctx.externs,
        c_callback_trampolines: ctx.c_callback_trampolines,
        globals: ctx.globals,
    };
    let (result, _) = codegen_expr(body, &env, &mut em, &inner_ctx, true)?;
    if result.is_some() {
        return Err(format!(
            "internal codegen error: closure {fn_name:?}'s body did not terminate with a `ret` in tail position"
        ));
    }
    let mut out = String::new();
    for g in &em.string_globals {
        out.push_str(g);
        out.push('\n');
    }
    out.push_str(&format!("define {} @{}({}) {{\n", ret_type.llvm_type(), fn_name, param_decls.join(", ")));
    for line in &em.lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(out)
}

/// Generates the paired `@closure_release$<fn>$<K>(ptr %cell)`: for
/// each heap-shaped capture, load and call its type-appropriate dec
/// function (matching `plum_release_fields`'s own "release fields,
/// don't free the cell" contract — `@plum_rc_dec_closure`, in lib.rs,
/// is what actually `free`s the cell afterward). Built as plain text
/// (no `Emitter` needed — every capture slot is independent, no control
/// flow), same style as `emit_array_release_fns`.
///
/// `self_slot`, if given, names the ONE capture index that's a
/// self-reference (the closure's own cell, stored into its own capture
/// slot — see `codegen_closure_literal`'s doc comment) — its dec is
/// skipped here for exactly the same reason its INC was skipped at
/// capture time: that slot was never incremented in the first place, so
/// decrementing it here would UNDER-count the cell's true refcount by
/// one (a real correctness bug caught by this crate's own tests, not a
/// hypothetical one — decrementing a slot that was never incremented
/// can, depending on timing, either free the cell too early or corrupt
/// its refcount while OTHER references are still live).
fn emit_closure_release_fn(fn_name: &str, captures: &[(String, CgType)], self_slot: Option<usize>, ctx: &Ctx) -> String {
    let mut out = String::new();
    out.push_str(&format!("define void @{fn_name}(ptr %cell) {{\n"));
    out.push_str("entry:\n");
    for (i, (_, ty)) in captures.iter().enumerate() {
        if self_slot == Some(i) {
            continue;
        }
        let Some(dec_fn) = crate::dec_fn_for(ty) else {
            continue;
        };
        if let CgType::Array(elem) = ty {
            register_array_elem(ctx, elem);
        }
        let offset = closure_field_byte_offset(i);
        out.push_str(&format!(
            "  %addr{i} = getelementptr i8, ptr %cell, i64 {offset}\n  \
             %word{i} = load i64, ptr %addr{i}\n  \
             %ptr{i} = inttoptr i64 %word{i} to ptr\n  \
             call void {dec_fn}(ptr %ptr{i})\n"
        ));
    }
    out.push_str("  ret void\n}\n");
    out
}

/// Codegens a closure LITERAL into `(cell_ptr, CgType::Closure(...))` —
/// shared by both the ordinary case (`codegen_value`'s own `Closure`
/// arm, `self_bind: None`) and the self-referential LOCAL case
/// (`codegen_expr`'s `Let` arm, `self_bind: Some(name)`) — see
/// DESIGN.md's "Self-referential closures" section for the surface
/// semantics this matches (self-recursion only, no mutual recursion
/// between separately-declared closures).
///
/// 1. Free-variable analysis against `env`, extended with a placeholder
///    `(self_bind, closure_cg_type)` entry FIRST when self-referential,
///    so a genuine self-reference is treated as an ordinary capture
///    candidate.
/// 2. Generates the per-literal-site function + release function (see
///    `emit_closure_body_fn`/`emit_closure_release_fn`), appended to
///    `ctx.closure_defs`.
/// 3. `%cell = call ptr @plum_alloc_closure(i64 N)` — the cell's
///    address is known immediately, BEFORE any capture is stored.
/// 4. Stores `code_ptr`/`release_fn_ptr` (raw `ptrtoint`, bypassing
///    `store_closure_capture`'s CgType dispatch — these aren't
///    Plum-typed captures).
/// 5. Stores each capture — including `self_bind` itself, if present
///    (its value being stored IS `%cell`, which is why step 3 must
///    precede this) — via `store_closure_capture` + `@plum_rc_inc` if
///    heap-shaped, EXCEPT the self-capture slot's inc, which is
///    DELIBERATELY skipped: incrementing it would create a reference
///    cycle the cell could never reach refcount zero to escape — a
///    genuine, deliberate, DOCUMENTED leak, matching this codebase's
///    established "accepted leak over unsoundness" precedent (already
///    used identically for `For`/`Closure`/`Spawn` body captures in
///    `fbip.rs`).
#[allow(clippy::too_many_arguments)]
fn codegen_closure_literal(
    params: &[String],
    param_types: &Option<Vec<PrimTy>>,
    ret_type: &Option<PrimTy>,
    body: &Expr,
    env: &Env,
    em: &mut Emitter,
    ctx: &Ctx,
    self_bind: Option<&str>,
) -> Result<(String, CgType), String> {
    let (param_cg_types, ret_cg_type) = resolve_closure_sig(param_types, ret_type, params.len())?;
    let closure_ty = CgType::Closure(param_cg_types.clone(), Box::new(ret_cg_type.clone()));

    // Step 1: free-variable analysis.
    let mut analysis_env = env.clone();
    if let Some(name) = self_bind {
        analysis_env.insert(name.to_string(), ("%__self_placeholder".to_string(), closure_ty.clone()));
    }
    let mut free = BTreeSet::new();
    free_vars(body, &analysis_env, &mut free);
    for p in params {
        free.remove(p);
    }
    let captures: Vec<(String, CgType)> = free
        .into_iter()
        .map(|name| {
            let ty = analysis_env[&name].1.clone();
            (name, ty)
        })
        .collect();

    // Step 2: generate the per-literal-site function + release fn.
    let k = {
        let mut c = ctx.closure_counter.borrow_mut();
        let k = *c;
        *c += 1;
        k
    };
    let fn_name = format!("closure${}${}", ctx.fn_name, k);
    let closure_def = emit_closure_body_fn(&fn_name, params, &param_cg_types, &ret_cg_type, &captures, body, ctx)?;
    ctx.closure_defs.borrow_mut().push(closure_def);

    let self_slot = self_bind.and_then(|name| captures.iter().position(|(n, _)| n == name));
    let release_fn_name = if captures.is_empty() {
        // No captures at all — the SAME shared no-op release every
        // zero-capture trampoline closure uses; no need to generate a
        // trivial, identical-every-time release function per site.
        "plum_closure_release_noop".to_string()
    } else {
        let release_name = format!("closure_release${}${}", ctx.fn_name, k);
        let release_def = emit_closure_release_fn(&release_name, &captures, self_slot, ctx);
        ctx.closure_defs.borrow_mut().push(release_def);
        release_name
    };

    // Step 3+4: allocate + populate code_ptr/release_fn_ptr.
    let n = captures.len();
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc_closure(i64 {n})"));
    let code_word = em.fresh_reg();
    em.push(format!("  {code_word} = ptrtoint ptr @{fn_name} to i64"));
    let code_addr = em.fresh_reg();
    em.push(format!("  {code_addr} = getelementptr i8, ptr {cell}, i64 8"));
    em.push(format!("  store i64 {code_word}, ptr {code_addr}"));
    let release_word = em.fresh_reg();
    em.push(format!("  {release_word} = ptrtoint ptr @{release_fn_name} to i64"));
    let release_addr = em.fresh_reg();
    em.push(format!("  {release_addr} = getelementptr i8, ptr {cell}, i64 16"));
    em.push(format!("  store i64 {release_word}, ptr {release_addr}"));

    // Step 5: store each capture. `self_bind`'s own slot (if present)
    // is bound to `cell` directly — `cell`'s address is already known
    // at this point (step 3 above), which is exactly why steps 3/4 had
    // to happen BEFORE this loop.
    for (i, (name, ty)) in captures.iter().enumerate() {
        let val_reg = if self_bind == Some(name.as_str()) {
            cell.clone()
        } else {
            env.get(name)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {name:?} (closure capture)"))?
                .0
        };
        store_closure_capture(em, &cell, i, &val_reg, ty.clone());
        if self_bind != Some(name.as_str()) && crate::dec_fn_for(ty).is_some() {
            em.push(format!("  call void @plum_rc_inc(ptr {val_reg})"));
        }
    }

    Ok((cell, closure_ty))
}

/// Synthesizes (once per distinct function name referenced this way
/// across the whole program — see `Ctx::trampolines`) a trampoline
/// `@trampoline$<name>(ptr %env_unused, params...) -> ret` that just
/// calls `@<name>` directly and returns its result, then allocates a
/// FRESH zero-capture closure cell wrapping it at THIS use site (every
/// reference gets its own cell, matching ordinary closure-literal
/// semantics — only the trampoline function TEXT itself is memoized).
/// The trivial release (no captures) reuses the shared `@plum_closure_
/// release_noop` — see `emit_runtime`'s doc comment on that function.
fn codegen_bare_fn_value(name: &str, sig: &FnSig, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let trampoline_name = {
        let mut memo = ctx.trampolines.borrow_mut();
        if let Some(existing) = memo.get(name) {
            existing.clone()
        } else {
            let tramp = format!("trampoline${name}");
            let def = emit_trampoline_fn(&tramp, name, sig);
            ctx.closure_defs.borrow_mut().push(def);
            memo.insert(name.to_string(), tramp.clone());
            tramp
        }
    };
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc_closure(i64 0)"));
    let code_word = em.fresh_reg();
    em.push(format!("  {code_word} = ptrtoint ptr @{trampoline_name} to i64"));
    let code_addr = em.fresh_reg();
    em.push(format!("  {code_addr} = getelementptr i8, ptr {cell}, i64 8"));
    em.push(format!("  store i64 {code_word}, ptr {code_addr}"));
    let release_word = em.fresh_reg();
    em.push(format!("  {release_word} = ptrtoint ptr @plum_closure_release_noop to i64"));
    let release_addr = em.fresh_reg();
    em.push(format!("  {release_addr} = getelementptr i8, ptr {cell}, i64 16"));
    em.push(format!("  store i64 {release_word}, ptr {release_addr}"));
    Ok((cell, CgType::Closure(sig.params.clone(), Box::new(sig.ret.clone()))))
}

fn emit_trampoline_fn(trampoline_name: &str, target_name: &str, sig: &FnSig) -> String {
    let mut param_decls = vec!["ptr %env".to_string()];
    let mut call_args = Vec::new();
    for (i, ty) in sig.params.iter().enumerate() {
        param_decls.push(format!("{} %p{i}", ty.llvm_type()));
        call_args.push(format!("{} %p{i}", ty.llvm_type()));
    }
    format!(
        "define {} @{}({}) {{\nentry:\n  %r = call {} @{}({})\n  ret {} %r\n}}\n",
        sig.ret.llvm_type(),
        trampoline_name,
        param_decls.join(", "),
        sig.ret.llvm_type(),
        target_name,
        call_args.join(", "),
        sig.ret.llvm_type(),
    )
}

// --- FFI: extern calls, CStr, C callbacks ---
//
// Scope: scalar (`Int`/`Float`/`Bool`) + `CStr` + C-callback parameters/
// returns — struct-by-value marshaling (the real System V ABI) is
// separate, deferred follow-up work (`crate::extern_type_to_llvm`'s own
// doc comment). Unlike the interpreter (`plum-interp`, which needs
// `libffi` because a call frame's signature is only known at Plum
// RUNTIME), an `ExternFn`'s signature is fully known at codegen time —
// an `ExternCall` compiles to an ordinary, statically-typed LLVM `call`
// instruction to a `declare`d symbol, exactly like this backend already
// calls `malloc`/`pthread_create`/`memcpy`. The one genuinely subtle
// piece is `Bool` marshaling: C's `int` is `i32`-wide (this backend's
// own `CgType::Bool` is `i1`), so every crossing needs an EXPLICIT
// conversion, never a bitcast/truncation pretending the two are the
// same width.

/// Converts an already-computed Plum value (`reg`, `ty`) into the
/// operand text (`"<llvmty> <reg>"`) an `ExternCall`'s argument list
/// needs for `param_ty`. `Bool` is the one case that isn't a direct
/// pass-through: `zext i1 to i32` — going INTO a C call, Plum's `i1`
/// must be widened to C's `int` representation. `CStr`/`Str` are
/// deliberately DISTINCT `CgType`s that both happen to already be `ptr`
/// at the LLVM level — an extern `CStr` parameter only ever accepts a
/// `CgType::CStr` value (produced by `.as_cstr()`), never a raw
/// `CgType::Str` directly, matching `plum-types`' own "no implicit
/// `Str`->`CStr` coercion" restriction (see `ir::ExternType::Str`'s own
/// doc comment) — re-verified here structurally, the same "trust but
/// verify against what's actually reachable from this IR" stance every
/// other codegen arm in this module takes.
/// The pieces `marshal_arg_to_c` combines into one operand string — split
/// out so `build_c_struct_value` (a struct-by-value argument's scalar
/// LEAF fields) can reuse the exact same conversion logic while still
/// needing the LLVM type and value SEPARATELY (an `insertvalue`'s second
/// operand is `<ty2> <val2>`, not a pre-joined string the way an
/// ordinary call's argument list is) — genuine reuse, not a parallel
/// reimplementation of the one load-bearing case here (`Bool`'s `zext i1
/// to i32`).
fn marshal_scalar_to_c_parts(reg: &str, ty: &CgType, param_ty: &ir::ExternType, em: &mut Emitter) -> Result<(String, String), String> {
    match (param_ty, ty) {
        (ir::ExternType::Int, CgType::Int) => Ok(("i64".to_string(), reg.to_string())),
        (ir::ExternType::Float, CgType::Float) => Ok(("double".to_string(), reg.to_string())),
        (ir::ExternType::Bool, CgType::Bool) => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = zext i1 {reg} to i32"));
            Ok(("i32".to_string(), r))
        }
        (ir::ExternType::Str, CgType::CStr) => Ok(("ptr".to_string(), reg.to_string())),
        (expected, found) => {
            Err(format!("codegen: extern call argument type mismatch — expected {expected:?}, found {found:?}"))
        }
    }
}

fn marshal_arg_to_c(reg: &str, ty: &CgType, param_ty: &ir::ExternType, em: &mut Emitter) -> Result<String, String> {
    let (llvm_ty, val) = marshal_scalar_to_c_parts(reg, ty, param_ty, em)?;
    Ok(format!("{llvm_ty} {val}"))
}

/// Ctor cell -> LLVM aggregate value — the argument-marshaling direction
/// of struct-by-value FFI (the sibling to `codegen_ctor_alloc`, which
/// builds an ordinary heap `Ctor` cell rather than a C-ABI aggregate).
/// `cell_ptr` is an already-computed `Heap`-typed SSA register (the
/// struct's own Ctor cell, or — for a recursive call — a NESTED struct
/// field's cell pointer, since Plum structs are always heap-boxed, never
/// stored inline even as a field of another struct). `tag`/`field_types`
/// come straight from the `ir::ExternType::Struct(tag, field_types)`
/// node driving this call; `field_types`' declared order is trusted to
/// match `ctx.tag_fields[tag]`'s own order without re-verifying it
/// structurally (both derive from the SAME struct declaration — see
/// `plum_ir::lower::resolve_extern_type_inner`'s struct-name lookup —
/// so this can only diverge on an internal-error-shaped bug, checked via
/// a length comparison below rather than trusted blindly).
///
/// Builds the aggregate up field-by-field from `undef` via `insertvalue`
/// — a scalar (`Int`/`Float`/`Bool`) leaf field is read out of the cell
/// via `load_field_word` (Plum's native `i64`/`double`/`i1`
/// representation) then narrowed to its real C width via `marshal_
/// scalar_to_c_parts` (the exact same `Bool` `zext i1 to i32` scalar-
/// argument conversion `marshal_arg_to_c` itself uses, genuine reuse —
/// see that function's own doc comment). A nested struct field's slot
/// holds a pointer to ANOTHER heap cell (never an inline sub-struct on
/// the Plum side) — recursed into via this same function, and the
/// resulting sub-aggregate `insertvalue`d into the parent as a real
/// value, never passed as a pointer at the C boundary. No refcount/
/// ownership discharge is emitted here — confirmed against the
/// interpreter's own `write_value_into_buffer` (`plum-interp/src/
/// lib.rs`), which never touches the heap's refcount for a struct
/// argument either; the cell's lifecycle stays governed by FBIP's
/// ordinary last-use analysis, same as any other `Heap`-typed extern-
/// call argument.
///
/// Returns `(llvm_type_string, value_register)` — e.g. `("%struct.
/// Point", "%v7")` — matching `marshal_scalar_to_c_parts`'s own
/// `(llvm_ty, val)` order (NOT `codegen_value`'s `(reg, ty)` order),
/// since both feed the exact same "type value" operand-formatting need
/// (an `insertvalue`'s second operand, or a call's argument list) and a
/// caller destructuring the result of whichever ONE of the two actually
/// ran for a given field (see the `match` in this function's own body,
/// where a nested-struct field's recursive call and a scalar field's
/// `marshal_scalar_to_c_parts` call must be interchangeable) must not
/// have to remember which shape it's holding.
fn build_c_struct_value(
    cell_ptr: &str,
    tag: &str,
    field_types: &[ir::ExternType],
    em: &mut Emitter,
    ctx: &Ctx,
) -> Result<(String, String), String> {
    let plum_field_types = tag_field_types(ctx, tag)?.to_vec();
    if plum_field_types.len() != field_types.len() {
        return Err(format!(
            "codegen: internal error — extern struct {tag:?} declares {} field(s) at the FFI boundary but \
             {} in its Plum struct declaration; these must always agree (both are derived from the same \
             struct declaration)",
            field_types.len(),
            plum_field_types.len()
        ));
    }
    let struct_llvm_ty = format!("%struct.{tag}");
    let mut agg = "undef".to_string();
    for (i, (plum_ty, c_ty)) in plum_field_types.iter().zip(field_types).enumerate() {
        let (field_llvm_ty, field_val) = match c_ty {
            ir::ExternType::Struct(nested_tag, nested_field_types) => {
                let nested_ptr = load_field_word(em, cell_ptr, i, CgType::Heap);
                build_c_struct_value(&nested_ptr, nested_tag, nested_field_types, em, ctx)?
            }
            ir::ExternType::Int | ir::ExternType::Float | ir::ExternType::Bool => {
                let plum_val = load_field_word(em, cell_ptr, i, plum_ty.clone());
                marshal_scalar_to_c_parts(&plum_val, plum_ty, c_ty, em)?
            }
            // Rejected before this node is ever produced — see
            // `plum_types::context::check_ffi_safe`'s "extern struct
            // type ...: CStr is only supported as a top-level extern
            // parameter/return type, not inside a struct field" /
            // "...not nested inside a struct field..." restrictions
            // (independently duplicated in `plum_ir::lower::resolve_
            // extern_type_inner`, the check that actually runs before
            // codegen ever sees this IR) — a struct field's `ExternType`
            // can only ever be `Int`/`Float`/`Bool`/`Struct` by the time
            // it reaches here.
            ir::ExternType::Str | ir::ExternType::Callback { .. } => unreachable!(
                "a CStr/Callback-typed struct field is rejected by plum_types::context::check_ffi_safe \
                 (and plum_ir::lower::resolve_extern_type_inner) before codegen ever runs"
            ),
        };
        let next = em.fresh_reg();
        em.push(format!("  {next} = insertvalue {struct_llvm_ty} {agg}, {field_llvm_ty} {field_val}, {i}"));
        agg = next;
    }
    Ok((struct_llvm_ty, agg))
}

/// LLVM aggregate value -> Ctor cell — the return-marshaling direction
/// of struct-by-value FFI (the sibling to `build_c_struct_value`).
/// `agg` is an already-computed SSA register holding the C-ABI
/// aggregate value (e.g. straight off a `call %struct.Point @fn(...)`
/// or — for a recursive call — an `extractvalue`d nested sub-aggregate).
/// Allocates a FRESH cell via `@plum_alloc` (the same allocator, and the
/// same already-interned tag id lookup, every ordinary `Ctor`
/// construction uses — see `codegen_ctor_alloc`), then `extractvalue`s
/// each field back out and `store_field_word`s it in.
///
/// **The one genuinely nontrivial design call**: a `Bool`-mapped field
/// gets `icmp ne i32 .., 0` (C's "any nonzero is true" convention) —
/// NOT a bare `zext i32 to i64` — before being handed to `store_field_
/// word`. `store_field_word`'s own `Bool` arm demands a genuinely
/// normalized `i1` operand (it does `zext i1 .. to i64` internally); a
/// raw `zext i32 to i64` would instead silently preserve a real C `int`
/// value like `2` as the WORD `2` — which, if that word is later
/// re-interpreted as an `i1` anywhere (e.g. `load_field_word` reading
/// this same field back with `trunc i64 .. to i1`), truncates to its low
/// bit and reads `2` (`0b10`) as `false`. This is exactly the same bug
/// `codegen_extern_call`'s own ordinary scalar `Bool`-return handling
/// already avoids via `icmp ne`, for the identical underlying reason —
/// see that function's own doc comment.
///
/// A nested struct field's `extractvalue`d result is itself a nested
/// AGGREGATE (still inline inside the parent — never a pointer, since a
/// C-ABI by-value struct never contains a pointer to itself), so this
/// recurses to build a fresh Ctor cell for it FIRST, then stores a
/// pointer to that fresh cell into the parent's own slot (mirroring how
/// every other nested-heap-value field is stored, via `store_field_
/// word`'s `Heap` arm).
fn build_ctor_from_c_struct(agg: &str, tag: &str, field_types: &[ir::ExternType], em: &mut Emitter, ctx: &Ctx) -> Result<String, String> {
    let plum_field_types = tag_field_types(ctx, tag)?.to_vec();
    if plum_field_types.len() != field_types.len() {
        return Err(format!(
            "codegen: internal error — extern struct {tag:?} declares {} field(s) at the FFI boundary but \
             {} in its Plum struct declaration; these must always agree (both are derived from the same \
             struct declaration)",
            field_types.len(),
            plum_field_types.len()
        ));
    }
    let struct_llvm_ty = format!("%struct.{tag}");
    let id = tag_id(ctx, tag)?;
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc(i64 {id}, i64 {})", plum_field_types.len()));
    for (i, (plum_ty, c_ty)) in plum_field_types.iter().zip(field_types).enumerate() {
        match c_ty {
            ir::ExternType::Struct(nested_tag, nested_field_types) => {
                let extracted = em.fresh_reg();
                em.push(format!("  {extracted} = extractvalue {struct_llvm_ty} {agg}, {i}"));
                let nested_cell = build_ctor_from_c_struct(&extracted, nested_tag, nested_field_types, em, ctx)?;
                store_field_word(em, &cell, i, &nested_cell, plum_ty.clone());
            }
            ir::ExternType::Bool => {
                let raw = em.fresh_reg();
                em.push(format!("  {raw} = extractvalue {struct_llvm_ty} {agg}, {i}"));
                // C's "any nonzero is true" convention — see this
                // function's own doc comment for why this MUST be
                // `icmp ne`, not a bare `zext`.
                let b = em.fresh_reg();
                em.push(format!("  {b} = icmp ne i32 {raw}, 0"));
                store_field_word(em, &cell, i, &b, plum_ty.clone());
            }
            ir::ExternType::Int | ir::ExternType::Float => {
                let extracted = em.fresh_reg();
                em.push(format!("  {extracted} = extractvalue {struct_llvm_ty} {agg}, {i}"));
                store_field_word(em, &cell, i, &extracted, plum_ty.clone());
            }
            // See `build_c_struct_value`'s matching arm's doc comment —
            // same shared upstream check, same dead-code-by-construction
            // reasoning.
            ir::ExternType::Str | ir::ExternType::Callback { .. } => unreachable!(
                "a CStr/Callback-typed struct field is rejected by plum_types::context::check_ffi_safe \
                 (and plum_ir::lower::resolve_extern_type_inner) before codegen ever runs"
            ),
        }
    }
    Ok(cell)
}

/// `sqrt(2.0)` (inside `unsafe { .. }`) — a call to a DECLARED `extern
/// "C"` function, resolved once here against `ctx.externs` (`ir::Expr::
/// ExternCall` itself only carries the callee name and argument
/// expressions, not its declared types). Each argument is marshaled per
/// its declared `ExternType` (`marshal_arg_to_c` for a scalar/`CStr`;
/// `codegen_callback_arg` — special-cased BEFORE `codegen_value` ever
/// runs on it — for a `Callback`-typed slot, see that function's own
/// doc comment for why). Return marshaling: a `Bool` return uses `icmp
/// ne i32 .., 0` — NOT `trunc` — deliberately: C's "any nonzero value is
/// true" convention differs from a naive "truncate to the low bit"
/// reading, which would silently misread e.g. a `2` as `false`. A `Str`
/// (`CStr` on the Plum side) return is NULL-checked via the same
/// `@plum_abort` runtime-check mechanism `emit_runtime_check` already
/// uses for array bounds checks, then `strlen`'d and copied into a
/// FRESH `@plum_alloc_str` cell — the original C pointer is
/// intentionally never freed (unknown provenance: might be `malloc`'d,
/// might be static, might be something else entirely), matching the
/// interpreter's own documented v1 leak-avoidance tradeoff exactly (see
/// `plum_interp::Interpreter::eval`'s matching `ExternCall` case).
fn codegen_extern_call(name: &str, args: &[Expr], env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let extern_fn = ctx
        .externs
        .get(name)
        .cloned()
        .ok_or_else(|| format!("codegen: unknown extern function {name:?}"))?;
    if extern_fn.param_types.len() != args.len() {
        return Err(format!(
            "codegen: extern function {name:?} expects {} argument(s), found {}",
            extern_fn.param_types.len(),
            args.len()
        ));
    }
    let mut call_arg_parts = Vec::with_capacity(args.len());
    for (arg_expr, param_ty) in args.iter().zip(&extern_fn.param_types) {
        let part = match param_ty {
            ir::ExternType::Callback { params: cb_params, ret: cb_ret } => {
                codegen_callback_arg(arg_expr, cb_params, cb_ret.as_deref(), ctx)?
            }
            ir::ExternType::Int | ir::ExternType::Float | ir::ExternType::Bool | ir::ExternType::Str => {
                let (reg, ty) = codegen_value(arg_expr, env, em, ctx)?;
                marshal_arg_to_c(&reg, &ty, param_ty, em)?
            }
            ir::ExternType::Struct(sname, field_types) => {
                let (reg, ty) = codegen_value(arg_expr, env, em, ctx)?;
                if ty != CgType::Heap {
                    return Err(format!(
                        "codegen: extern function {name:?}: struct argument {sname:?} expected a Heap-typed \
                         Ctor value, found {ty:?}"
                    ));
                }
                let (llvm_ty, val) = build_c_struct_value(&reg, sname, field_types, em, ctx)?;
                format!("{llvm_ty} {val}")
            }
        };
        call_arg_parts.push(part);
    }
    let call_args_str = call_arg_parts.join(", ");
    match &extern_fn.ret_type {
        None => {
            em.push(format!("  call void @{name}({call_args_str})"));
            Ok(("0".to_string(), CgType::Unit))
        }
        Some(ir::ExternType::Int) => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = call i64 @{name}({call_args_str})"));
            Ok((r, CgType::Int))
        }
        Some(ir::ExternType::Float) => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = call double @{name}({call_args_str})"));
            Ok((r, CgType::Float))
        }
        // C's "any nonzero is true" convention — `icmp ne i32 .., 0`,
        // NOT `trunc i32 .. to i1` (which would only look at the LOW
        // bit, silently misreading e.g. a `2` as `false`). This is the
        // return-direction counterpart to `marshal_arg_to_c`'s `zext`.
        Some(ir::ExternType::Bool) => {
            let raw = em.fresh_reg();
            em.push(format!("  {raw} = call i32 @{name}({call_args_str})"));
            let b = em.fresh_reg();
            em.push(format!("  {b} = icmp ne i32 {raw}, 0"));
            Ok((b, CgType::Bool))
        }
        Some(ir::ExternType::Str) => {
            let raw = em.fresh_reg();
            em.push(format!("  {raw} = call ptr @{name}({call_args_str})"));
            let is_null = em.fresh_reg();
            em.push(format!("  {is_null} = icmp eq ptr {raw}, null"));
            let not_null = em.fresh_reg();
            em.push(format!("  {not_null} = xor i1 {is_null}, 1"));
            emit_runtime_check(em, ctx, &not_null, &format!("extern function {name:?} returned a null string pointer"));
            let len = em.fresh_reg();
            em.push(format!("  {len} = call i64 @strlen(ptr {raw})"));
            let cell = em.fresh_reg();
            em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {len})"));
            let dst = em.fresh_reg();
            em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
            let memcpy_r = em.fresh_reg();
            em.push(format!("  {memcpy_r} = call ptr @memcpy(ptr {dst}, ptr {raw}, i64 {len})"));
            Ok((cell, CgType::Str))
        }
        // Rejected before this node is ever produced — see `plum_types::
        // context::check_ffi_safe`'s matching restriction (a callback
        // type is only ever meaningful as a PARAMETER, never a return
        // type) — mirrors `plum_interp::Interpreter::eval`'s identical
        // `unreachable!` on this exact case.
        Some(ir::ExternType::Callback { .. }) => {
            unreachable!("a callback return type is rejected at type-checking time, before this node is produced")
        }
        // A by-value struct return needs no NULL/runtime check the way
        // `Str`'s does — an aggregate return isn't a nullable pointer,
        // there's nothing to check — so this goes straight from the raw
        // C call to `build_ctor_from_c_struct`'s extract-and-rebuild.
        Some(ir::ExternType::Struct(sname, field_types)) => {
            let struct_llvm_ty = format!("%struct.{sname}");
            let raw = em.fresh_reg();
            em.push(format!("  {raw} = call {struct_llvm_ty} @{name}({call_args_str})"));
            let cell = build_ctor_from_c_struct(&raw, sname, field_types, em, ctx)?;
            Ok((cell, CgType::Heap))
        }
    }
}

/// `s.as_cstr()` — validates `s` (a `CgType::Str`) has no embedded NUL
/// byte (via a declared libc `@memchr`, the SAME "reach for a real libc
/// primitive over a hand-rolled loop" precedent `@strlen` above already
/// follows), then produces a FRESH, independently-owned `CgType::CStr`
/// value: `malloc`s a `len+1`-byte buffer, `memcpy`s the string's bytes
/// into it, and manually stores a trailing NUL.
///
/// # Why a fresh allocation is REQUIRED, not an optimization left on the table
///
/// `@plum_alloc_str` already reserves one trailing NUL byte past every
/// string cell's own `len` (see `lib.rs::emit_runtime`'s string-runtime
/// doc comment) — so it's tempting to just alias a pointer into `str_cell
/// + 16` instead of copying at all. That shortcut would be UNSOUND, not
/// merely a missed optimization: `plum_ir::fbip`'s `mark_last_uses`
/// treats `AsCStr`'s inner expression as an ORDINARY heap-consuming
/// occurrence (see `fbip.rs`'s own `Expr::AsCStr` arm) — meaning THIS
/// function is the ONLY place that ever discharges the incoming `Str`'s
/// refcount ownership; no separate `Dec` is emitted anywhere else for a
/// `Str` wrapped in `.as_cstr()`. That means this function MUST call
/// `@plum_rc_dec_str` on the original cell after copying — and the
/// moment it does, an ALIASED pointer into that same cell would dangle
/// the instant the dec drops the refcount to zero and frees it (the
/// common case whenever this was the `Str`'s last use). A fresh
/// `malloc`+`memcpy`, entirely independent of the original cell's
/// lifetime, is the only sound design given `fbip`'s existing ownership
/// contract.
fn codegen_as_cstr(inner: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (str_reg, str_ty) = codegen_value(inner, env, em, ctx)?;
    if str_ty != CgType::Str {
        return Err(format!("codegen: `.as_cstr()` requires a Str value, found {str_ty:?}"));
    }
    let len = load_array_len(em, &str_reg);
    let bytes_ptr = em.fresh_reg();
    em.push(format!("  {bytes_ptr} = getelementptr i8, ptr {str_reg}, i64 16"));

    // Embedded-NUL validation: a C string ends at the first NUL byte, so
    // an embedded one would silently truncate the string once it
    // crosses the FFI boundary — checked eagerly, right here, matching
    // the interpreter's own `.as_cstr()` semantics exactly.
    let memchr_r = em.fresh_reg();
    em.push(format!("  {memchr_r} = call ptr @memchr(ptr {bytes_ptr}, i32 0, i64 {len})"));
    let no_embedded_nul = em.fresh_reg();
    em.push(format!("  {no_embedded_nul} = icmp eq ptr {memchr_r}, null"));
    emit_runtime_check(em, ctx, &no_embedded_nul, "`.as_cstr()`: string contains an embedded null byte");

    let alloc_size = em.fresh_reg();
    em.push(format!("  {alloc_size} = add i64 {len}, 1"));
    let buf = em.fresh_reg();
    em.push(format!("  {buf} = call ptr @malloc(i64 {alloc_size})"));
    let memcpy_r = em.fresh_reg();
    em.push(format!("  {memcpy_r} = call ptr @memcpy(ptr {buf}, ptr {bytes_ptr}, i64 {len})"));
    let nul_addr = em.fresh_reg();
    em.push(format!("  {nul_addr} = getelementptr i8, ptr {buf}, i64 {len}"));
    em.push(format!("  store i8 0, ptr {nul_addr}"));

    // The mandatory ownership discharge — see this function's own doc
    // comment. MUST reference `str_reg`, the ORIGINAL cell — never
    // `buf`, the fresh unrefcounted `CStr` buffer, which carries no
    // refcount at all to discharge.
    em.push(format!("  call void @plum_rc_dec_str(ptr {str_reg})"));

    Ok((buf, CgType::CStr))
}

/// `strerror(errno)`, copied into a fresh Plum `Str` cell — the OS
/// error message half of `__FileIoResult`'s `payload` field on any
/// `read_file_raw`/`write_file_raw` failure. The exact same "raw C
/// string ptr -> fresh Plum `Str`" sequence extern `CStr`-return
/// codegen already performs (`codegen_extern_call`'s `Some(ir::
/// ExternType::Str)` arm), just without that arm's null-check-then-
/// abort half — a `Result` needs to keep going on failure, not abort.
fn codegen_errno_string(em: &mut Emitter) -> (String, CgType) {
    let errno_ptr = em.fresh_reg();
    em.push(format!("  {errno_ptr} = call ptr @__errno_location()"));
    let errno_val = em.fresh_reg();
    em.push(format!("  {errno_val} = load i32, ptr {errno_ptr}"));
    let msg = em.fresh_reg();
    em.push(format!("  {msg} = call ptr @strerror(i32 {errno_val})"));
    let len = em.fresh_reg();
    em.push(format!("  {len} = call i64 @strlen(ptr {msg})"));
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {len})"));
    let dst = em.fresh_reg();
    em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
    let copy_r = em.fresh_reg();
    em.push(format!("  {copy_r} = call ptr @memcpy(ptr {dst}, ptr {msg}, i64 {len})"));
    (cell, CgType::Str)
}

/// `read_file_raw(path)` — see `ir::Expr::ReadFileRaw`'s own doc
/// comment for the overall design (why this builds a `__FileIoResult`
/// struct directly rather than a `Result`/`Ok`/`Err` value itself).
/// Every Plum `Str` cell already carries a guaranteed trailing NUL byte
/// (`@plum_alloc_str`'s own unconditional behavior — see `lib.rs`'s
/// "Cell layout" doc comment), so `path`'s bytes are passed DIRECTLY to
/// `@fopen` as a C string — no `.as_cstr()` copy/RC-dec dance needed
/// here (that dance protects ordinary Plum-level values from a fresh-
/// copy/consume mismatch; not needed for an internal, same-function-
/// scope read of an already-live value). `@fseek`+`@ftell`+`@rewind`
/// size the file before allocating its `Str` cell in one shot, so
/// `@fread` writes directly into the final cell's own byte region —
/// no separate copy.
fn codegen_read_file_raw(path: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (path_reg, path_ty) = codegen_value(path, env, em, ctx)?;
    if path_ty != CgType::Str {
        return Err(format!("codegen: `read_file_raw` requires a Str path, found {path_ty:?}"));
    }
    *ctx.needs_file_io_runtime.borrow_mut() = true;

    let path_cstr = em.fresh_reg();
    em.push(format!("  {path_cstr} = getelementptr i8, ptr {path_reg}, i64 16"));
    let fp = em.fresh_reg();
    em.push(format!("  {fp} = call ptr @fopen(ptr {path_cstr}, ptr @plum_file_mode_rb)"));
    let is_null = em.fresh_reg();
    em.push(format!("  {is_null} = icmp eq ptr {fp}, null"));

    let fail_label = em.fresh_label("read_file_fail");
    let ok_label = em.fresh_label("read_file_ok");
    let merge_label = em.fresh_label("read_file_merge");
    em.push(format!("  br i1 {is_null}, label %{fail_label}, label %{ok_label}"));

    em.start_block(&fail_label);
    let (err_reg, _) = codegen_errno_string(em);
    let fail_result = codegen_ctor_alloc("__FileIoResult", &[("0".to_string(), CgType::Bool), (err_reg, CgType::Str)], em, ctx)?;
    let fail_block = em.current_block().to_string();
    em.push(format!("  br label %{merge_label}"));

    em.start_block(&ok_label);
    let seek_r = em.fresh_reg();
    em.push(format!("  {seek_r} = call i32 @fseek(ptr {fp}, i64 0, i32 2)"));
    let size = em.fresh_reg();
    em.push(format!("  {size} = call i64 @ftell(ptr {fp})"));
    em.push(format!("  call void @rewind(ptr {fp})"));
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc_str(i64 {size})"));
    let dst = em.fresh_reg();
    em.push(format!("  {dst} = getelementptr i8, ptr {cell}, i64 16"));
    let nread = em.fresh_reg();
    em.push(format!("  {nread} = call i64 @fread(ptr {dst}, i64 1, i64 {size}, ptr {fp})"));
    let close_r = em.fresh_reg();
    em.push(format!("  {close_r} = call i32 @fclose(ptr {fp})"));
    let ok_result = codegen_ctor_alloc("__FileIoResult", &[("1".to_string(), CgType::Bool), (cell, CgType::Str)], em, ctx)?;
    let ok_block = em.current_block().to_string();
    em.push(format!("  br label %{merge_label}"));

    em.start_block(&merge_label);
    let result = em.fresh_reg();
    em.push(format!("  {result} = phi ptr [ {fail_result}, %{fail_block} ], [ {ok_result}, %{ok_block} ]"));
    Ok((result, CgType::Heap))
}

/// `write_file_raw(path, contents)` — the write-side sibling of
/// `codegen_read_file_raw`; always truncates + creates (`fopen(path,
/// "wb")`, matching `std::fs::write`'s own behavior on the interpreter
/// side). `contents`'s bytes are read DIRECTLY from its existing `Str`
/// cell (offset 16, same guaranteed-NUL-terminated layout `path` uses)
/// — no copy needed for `@fwrite` either.
fn codegen_write_file_raw(path: &Expr, contents: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (path_reg, path_ty) = codegen_value(path, env, em, ctx)?;
    if path_ty != CgType::Str {
        return Err(format!("codegen: `write_file_raw` requires a Str path, found {path_ty:?}"));
    }
    let (contents_reg, contents_ty) = codegen_value(contents, env, em, ctx)?;
    if contents_ty != CgType::Str {
        return Err(format!("codegen: `write_file_raw` requires Str contents, found {contents_ty:?}"));
    }
    *ctx.needs_file_io_runtime.borrow_mut() = true;

    let path_cstr = em.fresh_reg();
    em.push(format!("  {path_cstr} = getelementptr i8, ptr {path_reg}, i64 16"));
    let fp = em.fresh_reg();
    em.push(format!("  {fp} = call ptr @fopen(ptr {path_cstr}, ptr @plum_file_mode_wb)"));
    let is_null = em.fresh_reg();
    em.push(format!("  {is_null} = icmp eq ptr {fp}, null"));

    let fail_label = em.fresh_label("write_file_fail");
    let ok_label = em.fresh_label("write_file_ok");
    let merge_label = em.fresh_label("write_file_merge");
    em.push(format!("  br i1 {is_null}, label %{fail_label}, label %{ok_label}"));

    em.start_block(&fail_label);
    let (err_reg, _) = codegen_errno_string(em);
    let fail_result = codegen_ctor_alloc("__FileIoResult", &[("0".to_string(), CgType::Bool), (err_reg, CgType::Str)], em, ctx)?;
    let fail_block = em.current_block().to_string();
    em.push(format!("  br label %{merge_label}"));

    em.start_block(&ok_label);
    let len = load_array_len(em, &contents_reg);
    let src = em.fresh_reg();
    em.push(format!("  {src} = getelementptr i8, ptr {contents_reg}, i64 16"));
    let nwritten = em.fresh_reg();
    em.push(format!("  {nwritten} = call i64 @fwrite(ptr {src}, i64 1, i64 {len}, ptr {fp})"));
    let close_r = em.fresh_reg();
    em.push(format!("  {close_r} = call i32 @fclose(ptr {fp})"));
    let empty_cell = em.fresh_reg();
    em.push(format!("  {empty_cell} = call ptr @plum_alloc_str(i64 0)"));
    let ok_result = codegen_ctor_alloc("__FileIoResult", &[("1".to_string(), CgType::Bool), (empty_cell, CgType::Str)], em, ctx)?;
    let ok_block = em.current_block().to_string();
    em.push(format!("  br label %{merge_label}"));

    em.start_block(&merge_label);
    let result = em.fresh_reg();
    em.push(format!("  {result} = phi ptr [ {fail_result}, %{fail_block} ], [ {ok_result}, %{ok_block} ]"));
    Ok((result, CgType::Heap))
}

/// `panic_raw(msg)` — see `ir::Expr::PanicRaw`'s own doc comment.
/// DELIBERATELY does NOT emit `unreachable`, unlike `emit_runtime_
/// check`'s own fail branch: that helper is used as a plain statement
/// with a SEPARATE "ok" continuation block reached only via a
/// conditional branch, never itself required to produce an SSA value.
/// `PanicRaw` is an ordinary expression, reachable through `codegen_
/// value` (e.g. as an `if`/`else` branch's tail value, merged via a
/// `phi` with the OTHER branch) — ending its own block in `unreachable`
/// would make it impossible to `br label %merge` into that phi at all,
/// and `@plum_abort` isn't marked `noreturn`, so nothing requires it.
/// Instead this just calls `@plum_abort` as an ordinary (non-
/// terminator) instruction and returns a placeholder `Unit` value
/// (`"0"`, exactly matching `Expr::Unit`'s own codegen) — dead code in
/// practice (`@plum_abort` itself calls `exit(1)`), but ordinary, well-
/// formed LLVM IR requiring zero special-casing in `If`/`Match`
/// codegen's own merge-point machinery.
fn codegen_panic_raw(message: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (msg_reg, msg_ty) = codegen_value(message, env, em, ctx)?;
    if msg_ty != CgType::Str {
        return Err(format!("codegen: `panic_raw` requires a Str message, found {msg_ty:?}"));
    }
    let msg_cstr = em.fresh_reg();
    em.push(format!("  {msg_cstr} = getelementptr i8, ptr {msg_reg}, i64 16"));
    em.push(format!("  call void @plum_abort(ptr {msg_cstr})"));
    Ok(("0".to_string(), CgType::Unit))
}

/// A `Callback`-typed `ExternCall` ARGUMENT — special-cased in
/// `codegen_extern_call`'s own arg loop, matched against the raw
/// argument EXPRESSION (never routed through `codegen_value` at all):
/// `plum_types::Infer`'s own callback-argument check (shared by both the
/// interpreter and codegen pipelines — see this module's own top-of-
/// section doc comment) has already proven, before this ever runs, that
/// a `Callback`-typed argument is a bare top-level function name, never
/// a closure literal or local variable — so this doesn't need to
/// re-derive that itself, only look the name up.
///
/// Generates (once per distinct target name, memoized in `Ctx::
/// c_callback_trampolines` — see that field's own doc comment for why
/// it's a SEPARATE table from `Ctx::trampolines`) an env-free trampoline
/// `@c_trampoline$<name>(cParams...) -> cRet` and references its symbol
/// DIRECTLY as the call argument — no `ptrtoint`/`inttoptr` round-trip
/// needed at all (simpler than the ordinary closure-value case in
/// `codegen_bare_fn_value`, which genuinely needs to store a code
/// address as a WORD inside a heap cell; here there's no intermediate
/// storage step — a bare LLVM function symbol reference is already a
/// valid `ptr`-typed call argument on its own).
fn codegen_callback_arg(
    arg_expr: &Expr,
    cb_params: &[ir::ExternType],
    cb_ret: Option<&ir::ExternType>,
    ctx: &Ctx,
) -> Result<String, String> {
    let Expr::Var(name) = arg_expr else {
        return Err(format!(
            "codegen: a callback argument must be a bare top-level function name, found {arg_expr:?} — matching \
             `plum_types::Infer`'s own restriction (only a bare function name, never a closure literal or local \
             variable, may be passed where a callback is expected)"
        ));
    };
    let sig = ctx
        .sigs
        .get(name)
        .cloned()
        .ok_or_else(|| format!("codegen: unknown function {name:?} (callback argument)"))?;
    let trampoline_name = {
        let mut memo = ctx.c_callback_trampolines.borrow_mut();
        if let Some(existing) = memo.get(name) {
            existing.clone()
        } else {
            let tramp = format!("c_trampoline${name}");
            let def = emit_c_callback_trampoline_fn(&tramp, name, &sig, cb_params, cb_ret)?;
            ctx.closure_defs.borrow_mut().push(def);
            memo.insert(name.to_string(), tramp.clone());
            tramp
        }
    };
    Ok(format!("ptr @{trampoline_name}"))
}

/// The C-callback counterpart to `emit_trampoline_fn` — generates
/// `@c_trampoline$<name>(cParams...) -> cRet` with NO leading `ptr %env`
/// parameter at all (the defining structural difference from an
/// ordinary closure trampoline — see `Ctx::c_callback_trampolines`'s own
/// doc comment for why a real C API has no way to supply one). Converts
/// each incoming C-ABI parameter to its Plum representation (a `Bool`
/// parameter: `icmp ne i32 %pN, 0`, the SAME "any nonzero is true"
/// widening `codegen_extern_call`'s own `Bool`-return handling uses, not
/// a `trunc`), calls the target Plum function directly, then converts
/// the result back to the C-ABI shape (a `Bool` return: `zext i1 %r to
/// i32`) before `ret`.
fn emit_c_callback_trampoline_fn(
    trampoline_name: &str,
    target_name: &str,
    sig: &FnSig,
    cb_params: &[ir::ExternType],
    cb_ret: Option<&ir::ExternType>,
) -> Result<String, String> {
    if cb_params.len() != sig.params.len() {
        return Err(format!(
            "codegen: callback target {target_name:?} has {} parameter(s), but the extern callback signature \
             expects {}",
            sig.params.len(),
            cb_params.len()
        ));
    }
    let mut param_decls = Vec::with_capacity(cb_params.len());
    let mut call_args = Vec::with_capacity(cb_params.len());
    let mut body = String::new();
    for (i, (cb_ty, plum_ty)) in cb_params.iter().zip(&sig.params).enumerate() {
        let c_llvm = crate::extern_type_to_llvm(cb_ty)?;
        let pname = format!("%p{i}");
        param_decls.push(format!("{c_llvm} {pname}"));
        match (cb_ty, plum_ty) {
            (ir::ExternType::Bool, CgType::Bool) => {
                body.push_str(&format!("  %conv{i} = icmp ne i32 {pname}, 0\n"));
                call_args.push(format!("i1 %conv{i}"));
            }
            (ir::ExternType::Int, CgType::Int) => call_args.push(format!("i64 {pname}")),
            (ir::ExternType::Float, CgType::Float) => call_args.push(format!("double {pname}")),
            (found, expected) => {
                return Err(format!(
                    "codegen: callback {target_name:?} parameter {i} type mismatch — extern callback declares \
                     {found:?}, function {target_name:?} has {expected:?}"
                ))
            }
        }
    }
    let (c_ret_llvm, tail): (&str, String) = match (cb_ret, &sig.ret) {
        (None, CgType::Unit) => ("void", "  ret void\n".to_string()),
        (Some(ir::ExternType::Int), CgType::Int) => ("i64", "  ret i64 %r\n".to_string()),
        (Some(ir::ExternType::Float), CgType::Float) => ("double", "  ret double %r\n".to_string()),
        (Some(ir::ExternType::Bool), CgType::Bool) => {
            ("i32", "  %rz = zext i1 %r to i32\n  ret i32 %rz\n".to_string())
        }
        (found, expected) => {
            return Err(format!(
                "codegen: callback {target_name:?} return type mismatch — extern callback declares {found:?}, \
                 function {target_name:?} returns {expected:?}"
            ))
        }
    };
    body.push_str(&format!("  %r = call {} @{target_name}({})\n", sig.ret.llvm_type(), call_args.join(", ")));
    body.push_str(&tail);
    Ok(format!("define {c_ret_llvm} @{trampoline_name}({}) {{\nentry:\n{body}}}\n", param_decls.join(", ")))
}

// --- spawn / join ---
//
// Task cell layout: `{ i64 joined, i64 pthread_id }` — a plain, bare
// 16-byte `@malloc`'d block, deliberately NOT allocated via `@plum_
// alloc` and NOT refcounted at all (see `CgType::Task`'s own doc
// comment in lib.rs — `dec_fn_for` returns `None` for it, matching
// `plum_ir::fbip`'s `is_syntactically_heap` never treating a `spawn`-
// bound name as heap-tracked). Byte offset 0 holds a `joined` flag
// (`.join()`'s own double-join guard), byte offset 8 the raw
// `pthread_t` value returned by `pthread_create` — see `emit_spawn_
// runtime`'s (lib.rs) doc comment for why `pthread_t` is stored
// directly as an `i64` rather than through any indirection.
//
// A spawned block's free variables are deep-copied (never `plum_rc_
// inc`'d — see `deep_copy_capture`'s own doc comment for the exact
// happens-before argument) into a plain, HEADER-FREE "spawn-args"
// block (`spawn_arg_byte_offset`/`store_spawn_arg`/`load_spawn_arg`,
// the `spawn` counterpart to `closure_field_byte_offset`/`store_
// closure_capture`/`load_closure_capture` — header-free here since,
// unlike a closure cell, nothing ever refcounts or releases a spawn-
// args block: the ENTRY function that reads it back out is the sole
// owner, and it's `free`'d once that function returns).

/// The spawn-args-block counterpart to `field_byte_offset`/`closure_
/// field_byte_offset` — no header at all (see this section's own doc
/// comment), so capture slot `index` starts directly at byte `index*8`.
fn spawn_arg_byte_offset(index: usize) -> i64 {
    (index as i64) * 8
}

/// The spawn-args counterpart to `store_field_word`/`store_closure_
/// capture` — same uniform-word conversion, at `spawn_arg_byte_
/// offset`'s header-free offset.
fn store_spawn_arg(em: &mut Emitter, block_ptr: &str, index: usize, value: &str, ty: CgType) {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {block_ptr}, i64 {}", spawn_arg_byte_offset(index)));
    store_word(em, &addr, value, ty);
}

/// The spawn-args counterpart to `load_field_word`/`load_closure_
/// capture` — see `store_spawn_arg`'s doc comment.
fn load_spawn_arg(em: &mut Emitter, block_ptr: &str, index: usize, expected_ty: CgType) -> String {
    let addr = em.fresh_reg();
    em.push(format!("  {addr} = getelementptr i8, ptr {block_ptr}, i64 {}", spawn_arg_byte_offset(index)));
    load_word(em, &addr, expected_ty)
}

/// Writes `value` into the single 64-bit word AT `addr` directly (no
/// per-field offset computation — the caller already has the exact
/// address), converting it to the same uniform word representation
/// `store_field_word` uses. Factored out from `store_field_word`
/// purely so `store_spawn_arg` and the spawn entry function's own
/// result-boxing (`emit_spawn_entry_fn`) can share the conversion logic
/// without going through a per-index offset computation that doesn't
/// apply to either of them (a spawn-args slot's address is already
/// computed via `spawn_arg_byte_offset`; the result box is a single
/// bare word with no offset at all).
fn store_word(em: &mut Emitter, addr: &str, value: &str, ty: CgType) {
    let word = value_to_word(em, value, ty);
    em.push(format!("  store i64 {word}, ptr {addr}"));
}

/// The bare value -> word conversion `store_word`/`store_field_word`/
/// `store_closure_capture`/`store_array_elem` all share, factored out
/// (rather than each computing the `store`'s own address separately) so
/// a channel `send`'s payload word — which has no ADDRESS at all, only
/// an i64 function ARGUMENT to `@plum_channel_send` — can reuse the
/// exact same conversion (see `codegen_channel_send`). Never itself
/// emits the `store` instruction.
fn value_to_word(em: &mut Emitter, value: &str, ty: CgType) -> String {
    match ty {
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = ptrtoint ptr {value} to i64"));
            r
        }
    }
}

/// The inverse of `store_word` — see that function's doc comment.
fn load_word(em: &mut Emitter, addr: &str, expected_ty: CgType) -> String {
    let word = em.fresh_reg();
    em.push(format!("  {word} = load i64, ptr {addr}"));
    word_to_value(em, &word, expected_ty)
}

/// The bare word -> value conversion `load_word`/`load_field_word`/
/// `load_closure_capture`/`load_array_elem` all share — see `value_to_
/// word`'s doc comment for why this is factored out separately (a
/// channel `recv`'s payload word comes back as an i64 RETURN VALUE from
/// `@plum_channel_recv`/`@plum_channel_try_recv`, not something loaded
/// from an address at all — see `codegen_channel_recv`/`codegen_select`).
fn word_to_value(em: &mut Emitter, word: &str, expected_ty: CgType) -> String {
    match expected_ty {
        CgType::Int => word.to_string(),
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
        CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) | CgType::Task(_) | CgType::Sender(_) | CgType::Receiver(_) | CgType::CStr => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = inttoptr i64 {word} to ptr"));
            r
        }
    }
}

/// Whether a value of `ty` can ever legally cross a `spawn` boundary —
/// `false` for a `Closure` or `Task` ANYWHERE inside `ty` (including
/// nested inside an `Array`), matching the interpreter's own `to_
/// portable` restriction exactly (`plum_interp::Interpreter::to_
/// portable`'s `Value::Closure`/`Value::Function`/`Value::Task` arms).
/// Checked recursively through `Array` — NOT through `Heap` (a struct/
/// enum pointer is opaque at a capture site; that case is instead
/// caught conservatively, whole-program, by `check_no_closure_or_task_
/// fields` in lib.rs) — because an array's element type IS statically
/// known here (unlike a struct's fields), so a `spawn` capturing an
/// `Array[Closure]`-typed free variable is just as real a bug as
/// capturing a bare closure directly: `deep_copy_capture` would
/// otherwise emit a call to a per-element deep-copy function for a
/// `Closure`/`Task` element that doesn't (and can't) meaningfully
/// exist — see `crate::deepcopy_fn_for`'s own doc comment for why that
/// function's `Closure`/`Task` arm is allowed to be a structurally
/// unreachable stub rather than a real implementation.
fn crosses_thread_boundary(ty: &CgType) -> bool {
    match ty {
        // `CStr` joins `Closure`/`Task` here for the same reason it
        // joins them in `crate::check_no_closure_or_task_fields` (see
        // that function's own doc comment): a raw, unowned,
        // unsynchronized C pointer aliased across two threads is
        // strictly worse than either of those, not merely equally bad —
        // there's no shared-ownership protocol backing it at all.
        CgType::Closure(..) | CgType::Task(_) | CgType::CStr => true,
        CgType::Array(elem) => crosses_thread_boundary(elem),
        // A `Sender`/`Receiver` explicitly CAN cross a thread boundary
        // (that's the whole point of a channel) — unlike a closure's
        // captured environment or a task handle, both of which are tied
        // to the thread that created them, a channel handle is just a
        // verbatim pointer to a shared, mutex-guarded queue any thread
        // can safely operate on. See `deep_copy_capture`'s own
        // `Sender`/`Receiver` arm for the "copy the pointer, never a
        // deep copy" mechanism this enables.
        CgType::Sender(_) | CgType::Receiver(_) => false,
        CgType::Int | CgType::Float | CgType::Bool | CgType::Unit | CgType::Heap | CgType::Str => false,
    }
}

/// Deep-copies a captured free variable's value (already computed, in
/// `reg`) into a form the spawned thread can safely own outright — a
/// pure, READ-ONLY snapshot of `reg`'s cell (`@plum_deepcopy_heap`/
/// `@plum_deepcopy_str`/`@plum_deepcopy_array_<mangled>`, all emitted
/// once per program by `lib.rs::emit_spawn_runtime`/`emit_deepcopy_
/// array_fns`), never a `plum_rc_inc` on the original. This is the
/// crux correctness point of this whole feature: the SOURCE thread may
/// still hold and use its own live reference to `reg` after this
/// `spawn` returns (`plum_ir::fbip` forces `live_after=true` for
/// exactly this reason — see `Expr::Spawn`'s own handling in fbip.rs).
/// If the original pointer crossed unchanged, both threads could end up
/// concurrently `plum_rc_inc`/`plum_rc_dec`-ing the SAME cell's non-
/// atomic refcount word — genuine undefined behavior, exactly what
/// DESIGN.md's "Implementation blocker: heap ownership across tasks"
/// deep-copy decision exists to prevent. A scalar (`Int`/`Float`/
/// `Bool`/`Unit`) needs no copy at all — its "word" already IS the
/// whole value, nothing aliased between threads either way.
fn deep_copy_capture(em: &mut Emitter, ctx: &Ctx, reg: &str, ty: &CgType) -> String {
    match ty {
        CgType::Heap => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_deepcopy_heap(ptr {reg})"));
            r
        }
        CgType::Str => {
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_deepcopy_str(ptr {reg})"));
            r
        }
        CgType::Array(elem) => {
            register_array_elem(ctx, elem);
            let mangled = elem.mangled();
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_deepcopy_array_{mangled}(ptr {reg})"));
            r
        }
        // Guaranteed unreachable — `crosses_thread_boundary` already
        // rejected any capture that could get here — but the match
        // still needs to be total.
        CgType::Closure(..) | CgType::Task(_) | CgType::CStr => reg.to_string(),
        // A `Sender`/`Receiver` crosses VERBATIM — never a deep copy.
        // Both ends must keep pointing at the SAME shared queue struct,
        // or the channel would silently split into two mutually-
        // invisible halves the moment one crossed a thread boundary —
        // a real correctness bug, not just wasted work (see this
        // module's `crosses_thread_boundary` doc comment). Reused
        // identically by both `spawn`'s capture crossing AND `.send()`'s
        // own value crossing (`codegen_channel_send`) — a channel sent
        // over another channel is exactly this case.
        CgType::Sender(_) | CgType::Receiver(_) => reg.to_string(),
        CgType::Int | CgType::Float | CgType::Bool | CgType::Unit => reg.to_string(),
    }
}

/// Generates the per-`spawn`-literal-site thread-entry function
/// `@spawn$<fn>$<K>(ptr %args) -> ptr` — the exact `pthread_create` C
/// ABI (`void *(*)(void *)`, no wrapping needed since a spawned block
/// takes no surface parameters of its own; `%args` is the header-free
/// spawn-args block `codegen_spawn_literal` builds). Loads each capture
/// back out by index (mirroring `emit_closure_body_fn`'s capture-
/// loading loop, just against `load_spawn_arg`'s header-free offset
/// instead of `load_closure_capture`'s), codegens `block` with
/// `tail=false` (deliberately — its result needs to be BOXED, not
/// directly `ret`'d, so there is no `ret <ty> %v` this could otherwise
/// share with an ordinary function body), then boxes the result in a
/// tiny separate `malloc`'d one-word cell and returns its pointer — the
/// exact `void*` `pthread_join` hands back to the joiner. Returns the
/// generated function TEXT alongside the block's own discovered
/// `CgType` (there's no separate return-type annotation on `ir::Expr::
/// Spawn` — unlike a closure literal, which carries `ret_type: Option
/// <PrimTy>` — so the result type is simply whatever `codegen_expr`
/// itself determines while generating this function's body, exactly
/// like an ordinary top-level function's untyped `ir::Function` body).
///
/// KNOWN, DELIBERATE, ACCEPTED LEAK: a heap-shaped capture's deep copy
/// is never explicitly released once `block` is done with it — `plum_
/// ir::fbip`'s `mark_last_uses` forces `live_after=true` for the whole
/// recursive walk into a `Spawn`'s `block` (see that function's own
/// `Expr::Spawn` arm), so NO `RcAnnotated::Dec` is ever emitted for a
/// captured name's uses inside `block` at all — the exact same
/// suppression `Closure` bodies already get (matching THEIR own
/// already-accepted "a captured heap value's precise last-use inside a
/// closure body isn't tracked either" gap). Unlike a closure, there's
/// no cell-level release mechanism to eventually catch it either (a
/// closure's captures get released when the closure CELL itself hits
/// refcount zero; a spawn's deep-copied captures live only in this
/// entry function's own registers, which simply vanish when it
/// returns). Correctly fixing this would need real last-use analysis
/// INSIDE a spawned block (distinguishing "captured value used, then
/// genuinely done with it" from "captured value returned/aliased into
/// the result," e.g. `spawn { p }` — where releasing `p` unconditionally
/// after computing the block's result would free the very cell about
/// to be boxed and returned, a real use-after-free) — real, currently-
/// absent work, not attempted here. Matches this codebase's own
/// established "accepted leak over unsoundness" precedent (`Assign`/
/// `For`/closure-body captures already documented the same way
/// elsewhere) — a leak, never a double-free/use-after-free/data race.
fn emit_spawn_entry_fn(fn_name: &str, captures: &[(String, CgType)], block: &Expr, ctx: &Ctx) -> Result<(String, CgType), String> {
    let mut em = Emitter::new();
    let mut env: Env = HashMap::new();
    for (i, (name, ty)) in captures.iter().enumerate() {
        let val = load_spawn_arg(&mut em, "%args", i, ty.clone());
        env.insert(name.clone(), (val, ty.clone()));
    }
    // The spawn-args block itself (see this section's module doc
    // comment) is `free`d here, immediately after every capture has
    // been loaded out of it — safe because every loaded value is
    // already its own independent word/pointer in a register by this
    // point (freeing the BLOCK that used to hold a pointer's bytes
    // never affects whatever that pointer itself still points to).
    // Never allocated at all when there were zero captures, so nothing
    // to free in that case either (see `codegen_spawn_literal`).
    if !captures.is_empty() {
        em.push("  call void @free(ptr %args)".to_string());
    }
    // `caller_sig` only matters for deciding `musttail` eligibility on a
    // tail-position `Call` (see `Ctx::caller_sig`'s own doc comment) —
    // irrelevant here since `block` is always codegen'd with
    // `tail=false` below, so its own tail-position `Call`s (if any)
    // never reach `codegen_expr`'s `tail=true` arm at all. A throwaway
    // placeholder signature is therefore always safe.
    let dummy_sig = FnSig { params: vec![], ret: CgType::Unit };
    let inner_ctx = Ctx {
        sigs: ctx.sigs,
        caller_sig: &dummy_sig,
        is_closure_body: false,
        tag_ids: ctx.tag_ids,
        tag_fields: ctx.tag_fields,
        fn_name,
        needed_arrays: ctx.needed_arrays,
        closure_counter: ctx.closure_counter,
        closure_defs: ctx.closure_defs,
        trampolines: ctx.trampolines,
        needs_spawn_runtime: ctx.needs_spawn_runtime,
        needs_channel_runtime: ctx.needs_channel_runtime,
        needs_file_io_runtime: ctx.needs_file_io_runtime,
        externs: ctx.externs,
        c_callback_trampolines: ctx.c_callback_trampolines,
        globals: ctx.globals,
    };
    let (result, _) = codegen_expr(block, &env, &mut em, &inner_ctx, false)?;
    let (reg, ty) = result.ok_or_else(|| {
        "internal codegen error: codegen_expr with tail=false should always return Some (spawn block)".to_string()
    })?;

    let box_ptr = em.fresh_reg();
    em.push(format!("  {box_ptr} = call ptr @malloc(i64 8)"));
    store_word(&mut em, &box_ptr, &reg, ty.clone());
    em.push(format!("  ret ptr {box_ptr}"));

    let mut out = String::new();
    for g in &em.string_globals {
        out.push_str(g);
        out.push('\n');
    }
    out.push_str(&format!("define ptr @{fn_name}(ptr %args) {{\n"));
    for line in &em.lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");
    Ok((out, ty))
}

/// Codegens a `spawn { block }` literal into `(task_cell_ptr, CgType::
/// Task(result_ty))`. Free-variable analysis mirrors `codegen_closure_
/// literal`'s (`free_vars` against the current `env`), but every
/// capture is DEEP-COPIED (`deep_copy_capture`) into a plain, header-
/// free "spawn-args" block rather than stored with a `plum_rc_inc` the
/// way a closure capture is — see `deep_copy_capture`'s own doc comment
/// for the exact correctness argument. A closure/task-typed (possibly
/// nested inside an array) free variable is a clear, immediate `Err`
/// here (`crosses_thread_boundary`), matching the interpreter's own
/// restriction. `pthread_create`'s out-parameter (`pthread_t *thread`)
/// needs a real memory address to write into — `alloca i64` (this
/// backend's existing, established precedent for a C-ABI-mandated
/// scratch slot, see e.g. `.to_string()`'s `snprintf` buffer).
fn codegen_spawn_literal(block: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    *ctx.needs_spawn_runtime.borrow_mut() = true;

    let mut free = BTreeSet::new();
    free_vars(block, env, &mut free);
    let mut captures: Vec<(String, CgType, String)> = Vec::with_capacity(free.len());
    for name in free {
        let (reg, ty) = env
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("codegen: unbound variable {name:?} (spawn capture)"))?;
        if crosses_thread_boundary(&ty) {
            return Err(format!(
                "codegen: `spawn` cannot capture {name:?} — its type ({ty:?}) can't cross a thread boundary \
                 (a closure's captured environment and a task handle are both tied to the thread that \
                 created them, so there's nothing meaningful to deep-copy), matching the interpreter's own \
                 restriction (see `plum_interp::Interpreter::to_portable`)"
            ));
        }
        captures.push((name, ty, reg));
    }

    let k = {
        let mut c = ctx.closure_counter.borrow_mut();
        let k = *c;
        *c += 1;
        k
    };
    let fn_name = format!("spawn${}${}", ctx.fn_name, k);
    let capture_tys: Vec<(String, CgType)> = captures.iter().map(|(n, t, _)| (n.clone(), t.clone())).collect();
    let (entry_def, result_ty) = emit_spawn_entry_fn(&fn_name, &capture_tys, block, ctx)?;
    ctx.closure_defs.borrow_mut().push(entry_def);

    let args_ptr = if captures.is_empty() {
        "null".to_string()
    } else {
        let ab = em.fresh_reg();
        em.push(format!("  {ab} = call ptr @malloc(i64 {})", captures.len() * 8));
        for (i, (_, ty, reg)) in captures.iter().enumerate() {
            let copied = deep_copy_capture(em, ctx, reg, ty);
            store_spawn_arg(em, &ab, i, &copied, ty.clone());
        }
        ab
    };

    // `pthread_create`'s own `i32` return value is deliberately not
    // checked (an out-of-threads/out-of-memory failure) — matching
    // this backend's existing precedent of never checking `@malloc`'s
    // return either (see e.g. `@plum_alloc`'s own body in lib.rs);
    // both are treated as "the process is already in real trouble,"
    // not a recoverable Plum-level condition.
    let tid_slot = em.fresh_reg();
    em.push(format!("  {tid_slot} = alloca i64"));
    let created = em.fresh_reg();
    em.push(format!("  {created} = call i32 @pthread_create(ptr {tid_slot}, ptr null, ptr @{fn_name}, ptr {args_ptr})"));
    let tid = em.fresh_reg();
    em.push(format!("  {tid} = load i64, ptr {tid_slot}"));

    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @malloc(i64 16)"));
    em.push(format!("  store i64 0, ptr {cell}"));
    let tid_addr = em.fresh_reg();
    em.push(format!("  {tid_addr} = getelementptr i8, ptr {cell}, i64 8"));
    em.push(format!("  store i64 {tid}, ptr {tid_addr}"));

    Ok((cell, CgType::Task(Box::new(result_ty))))
}

/// Codegens `task.join()` — see this section's own doc comment for the
/// task cell layout. The double-join guard (`emit_runtime_check`, the
/// SAME abort-on-failure mechanism array/string bounds checks already
/// use) reads then immediately overwrites the `joined` flag; ordering
/// between the check and the mark doesn't matter for THREAD safety
/// (task handles are never `Send`-able to begin with — see `crosses_
/// spawn_boundary` — so only ONE thread ever holds a given handle,
/// exactly like the interpreter's `TaskHandle`). `pthread_join` is a
/// POSIX-guaranteed happens-before synchronization point: everything
/// the child thread did (including its own `emit_spawn_entry_fn`-
/// generated final `store` into the result box) is guaranteed visible
/// to this thread once `pthread_join` returns, and the child has
/// already TERMINATED by then — so there is no window where the two
/// threads could concurrently touch the result box or anything it
/// points to. This is exactly why `.join()` needs NO second deep-copy
/// on the way out (unlike the interpreter, which is forced into one
/// purely by its own separate-heap-per-task structure — codegen's
/// shared, thread-safe `malloc` arena has no such constraint): the
/// result is simply adopted directly.
fn codegen_task_join(task: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (task_reg, task_ty) = codegen_value(task, env, em, ctx)?;
    let CgType::Task(result_ty) = task_ty else {
        return Err(format!("codegen: `.join()` requires a Task value, found {task_ty:?}"));
    };
    *ctx.needs_spawn_runtime.borrow_mut() = true;

    let joined = em.fresh_reg();
    em.push(format!("  {joined} = load i64, ptr {task_reg}"));
    let not_joined = em.fresh_reg();
    em.push(format!("  {not_joined} = icmp eq i64 {joined}, 0"));
    emit_runtime_check(em, ctx, &not_joined, "task already joined");
    em.push(format!("  store i64 1, ptr {task_reg}"));

    let tid_addr = em.fresh_reg();
    em.push(format!("  {tid_addr} = getelementptr i8, ptr {task_reg}, i64 8"));
    let tid = em.fresh_reg();
    em.push(format!("  {tid} = load i64, ptr {tid_addr}"));

    let retval_slot = em.fresh_reg();
    em.push(format!("  {retval_slot} = alloca ptr"));
    let jr = em.fresh_reg();
    em.push(format!("  {jr} = call i32 @pthread_join(i64 {tid}, ptr {retval_slot})"));
    let box_ptr = em.fresh_reg();
    em.push(format!("  {box_ptr} = load ptr, ptr {retval_slot}"));

    let result = load_word(em, &box_ptr, (*result_ty).clone());
    em.push(format!("  call void @free(ptr {box_ptr})"));
    em.push(format!("  call void @free(ptr {task_reg})"));

    Ok((result, *result_ty))
}

// --- channels / select ---
//
// See lib.rs's `emit_channel_runtime` doc comment for the shared
// channel-queue struct's/queue node's exact byte layout and the full
// multi-producer correctness argument (every mutation of `head`/`tail`/
// a node's `next` happens strictly under the SAME struct-embedded
// mutex, in every one of `@plum_channel_send`/`@plum_channel_recv`/
// `@plum_channel_try_recv` — a lost update is structurally impossible).
//
// DOCUMENTED, DELIBERATE GAP: none of `send`/`recv`/`select` ever
// detect disconnection — `send` always succeeds (the queue has no
// notion of "no receivers left"), and `recv`/`select` block
// (potentially forever) if nothing is ever sent, rather than
// replicating the interpreter's `Arc`/`Drop`-based disconnect errors
// (`plum_interp::Interpreter`'s `SenderHandle`/`ReceiverHandle`).
// Scope confirmed with the user for this chunk — real disconnect
// detection would need actual refcounting on `Sender`/`Receiver`
// (tracking how many of each are still alive), deliberately deferred
// as explicit follow-up work, matching `Task`'s own already-accepted
// capture-leak gap.

/// `channel[T]()` — allocates the ONE shared, permanently-leaked queue
/// struct (`@plum_channel_new`) and wraps it in an ordinary 2-field
/// tuple cell `(Sender, Receiver)`, tag `"2Tuple"` (the same synthetic
/// tag `lower.rs`'s `tuple_tag(2)` gives EVERY size-2 tuple — see
/// `plumc::codegen_cli`'s own doc comment on how it registers this
/// specific tag's `CgType` fields for a program that uses `channel[T]
/// ()`, and the documented single-element-type-per-program limit this
/// implies). The `Sender`/`Receiver` values are the LITERALLY SAME
/// pointer to the queue struct — no separate allocation per end, no
/// `Arc`-style indirection: codegen's shared `malloc` arena has no
/// analogue to the interpreter's need for an owned, `Clone`-able
/// handle (see `CgType::Sender`/`Receiver`'s own doc comment in
/// lib.rs).
fn codegen_channel_literal(em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    *ctx.needs_channel_runtime.borrow_mut() = true;
    let q = em.fresh_reg();
    em.push(format!("  {q} = call ptr @plum_channel_new()"));
    let id = tag_id(ctx, "2Tuple")?;
    let cell = em.fresh_reg();
    em.push(format!("  {cell} = call ptr @plum_alloc(i64 {id}, i64 2)"));
    let sender_addr = em.fresh_reg();
    em.push(format!("  {sender_addr} = getelementptr i8, ptr {cell}, i64 16"));
    let sender_word = em.fresh_reg();
    em.push(format!("  {sender_word} = ptrtoint ptr {q} to i64"));
    em.push(format!("  store i64 {sender_word}, ptr {sender_addr}"));
    let receiver_addr = em.fresh_reg();
    em.push(format!("  {receiver_addr} = getelementptr i8, ptr {cell}, i64 24"));
    em.push(format!("  store i64 {sender_word}, ptr {receiver_addr}"));
    Ok((cell, CgType::Heap))
}

/// `tx.send(v)` — deep-copies `v` (reusing `deep_copy_capture`
/// VERBATIM, the SAME correctness argument as a `spawn` capture, if
/// anything MORE important here: multiple concurrent senders could
/// otherwise race on the SAME source cell's non-atomic refcount word
/// with no synchronization at all — see `deep_copy_capture`'s own doc
/// comment), then `@plum_channel_send`s the resulting word. A closure/
/// task-typed (possibly array-nested) `v` is a clear, immediate `Err`
/// here (`crosses_thread_boundary`, the SAME check `spawn` capture
/// rejection uses), matching the interpreter's own restriction.
/// Evaluates to `Unit`, matching `ir::Expr::ChannelSend`'s own doc
/// comment.
fn codegen_channel_send(sender: &Expr, value: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (sender_reg, sender_ty) = codegen_value(sender, env, em, ctx)?;
    let CgType::Sender(inner_ty) = sender_ty else {
        return Err(format!("codegen: `.send()` requires a Sender value, found {sender_ty:?}"));
    };
    let (val_reg, val_ty) = codegen_value(value, env, em, ctx)?;
    if crosses_thread_boundary(&val_ty) {
        return Err(format!(
            "codegen: `.send()` cannot send {val_ty:?} — its captured environment (a closure) or task handle \
             is tied to the thread that created it and can't cross a thread boundary, matching the \
             interpreter's own restriction (see `plum_interp::Interpreter::to_portable`)"
        ));
    }
    if val_ty != *inner_ty {
        return Err(format!(
            "codegen: `.send()` argument type mismatch — this channel carries {inner_ty:?}, found {val_ty:?}"
        ));
    }
    *ctx.needs_channel_runtime.borrow_mut() = true;
    let copied = deep_copy_capture(em, ctx, &val_reg, &val_ty);
    let word = value_to_word(em, &copied, val_ty);
    em.push(format!("  call void @plum_channel_send(ptr {sender_reg}, i64 {word})"));
    Ok(("0".to_string(), CgType::Unit))
}

/// `rx.recv()` — a REAL blocking wait (`@plum_channel_recv`'s own
/// `pthread_cond_wait` loop, see `emit_channel_runtime`'s doc comment),
/// not a busy-poll: a single channel has a real condvar to block on,
/// unlike `select`'s cross-channel poll. No second deep-copy on the way
/// out — once a node is popped off the queue (under the mutex), only
/// this call ever touches its payload word again, the same clean
/// ownership-transfer argument `.join()` already established (see
/// `codegen_task_join`'s own doc comment), now re-verified to hold
/// per-node even with multiple concurrent senders (see this section's
/// module doc comment).
fn codegen_channel_recv(receiver: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx) -> Result<(String, CgType), String> {
    let (recv_reg, recv_ty) = codegen_value(receiver, env, em, ctx)?;
    let CgType::Receiver(inner_ty) = recv_ty else {
        return Err(format!("codegen: `.recv()` requires a Receiver value, found {recv_ty:?}"));
    };
    *ctx.needs_channel_runtime.borrow_mut() = true;
    let word = em.fresh_reg();
    em.push(format!("  {word} = call i64 @plum_channel_recv(ptr {recv_reg})"));
    let value = word_to_value(em, &word, (*inner_ty).clone());
    Ok((value, *inner_ty))
}

/// `select { pattern = expr => body, ... }` — busy-polls every arm's
/// already-evaluated receiver in FIXED index order via `@plum_channel_
/// try_recv` (non-blocking), matching the interpreter's exact
/// `Interpreter::eval`'s `Select` algorithm: a full sweep with nothing
/// ready sleeps 1ms (`@usleep(1000)`, matching the interpreter's own
/// `Duration::from_millis(1)`) then retries from arm 0 — no
/// `Disconnected` case at all, since codegen's queues never signal
/// disconnection (see this section's module doc comment): `select`
/// genuinely spins forever if every arm's channel is dead. Every arm's
/// `receiver` expr is evaluated exactly ONCE, up front (matching
/// `ir::Expr::Select`'s own doc comment), never re-evaluated on a later
/// poll sweep. Reuses `Match`'s exact arm-binding pattern (`"__select_
/// recv"` bound into a fresh per-arm `Env`, mirroring `bind_match_arm`)
/// and, since `plum-types` already guarantees every arm's body shares
/// one result type, `Match`'s exact `phi`+`merge_envs` result-merging
/// scheme (see `Expr::Match`'s own codegen in `codegen_expr` for the
/// direct precedent this mirrors). A hand-built ZERO-arm `Select`
/// (already rejected by `plum_types` before codegen ever runs — see
/// this module's own top-level doc comment) is a defensive `Err`, never
/// a panic — there's no arm 0 to even evaluate a receiver for.
fn codegen_select(
    arms: &[SelectArm],
    env: &Env,
    em: &mut Emitter,
    ctx: &Ctx,
    tail: bool,
) -> Result<(Option<(String, CgType)>, Env), String> {
    if arms.is_empty() {
        return Err("codegen: internal error — `select` has zero arms (already rejected by plum_types before codegen)".to_string());
    }
    *ctx.needs_channel_runtime.borrow_mut() = true;

    // Every arm's receiver is evaluated exactly once, up front.
    let mut handles: Vec<(String, CgType)> = Vec::with_capacity(arms.len());
    for arm in arms {
        let (reg, ty) = codegen_value(&arm.receiver, env, em, ctx)?;
        let CgType::Receiver(inner_ty) = ty else {
            return Err(format!("codegen: `select` arm requires a Receiver value, found {ty:?}"));
        };
        handles.push((reg, *inner_ty));
    }
    // One `alloca` per arm, reused across every poll sweep — `@plum_
    // channel_try_recv`'s out-parameter needs a real memory address to
    // write a popped word into.
    let out_slots: Vec<String> = (0..arms.len())
        .map(|_| {
            let slot = em.fresh_reg();
            em.push(format!("  {slot} = alloca i64"));
            slot
        })
        .collect();

    let poll_label = em.fresh_label("select_poll");
    let sweep_done_label = em.fresh_label("select_sweep_done");
    let done_label = em.fresh_label("select_done");
    em.push(format!("  br label %{poll_label}"));
    em.start_block(&poll_label);

    let mut matched_labels = Vec::with_capacity(arms.len());
    for (i, (handle_reg, _)) in handles.iter().enumerate() {
        let matched_label = em.fresh_label("select_matched");
        let next_label = if i + 1 < handles.len() { em.fresh_label("select_check") } else { sweep_done_label.clone() };
        let ok = em.fresh_reg();
        em.push(format!("  {ok} = call i1 @plum_channel_try_recv(ptr {handle_reg}, ptr {})", out_slots[i]));
        em.push(format!("  br i1 {ok}, label %{matched_label}, label %{next_label}"));
        matched_labels.push(matched_label.clone());
        if i + 1 < handles.len() {
            em.start_block(&next_label);
        }
    }

    em.start_block(&sweep_done_label);
    em.push("  call i32 @usleep(i32 1000)".to_string());
    em.push(format!("  br label %{poll_label}"));

    let mut non_tail_results: Vec<(String, CgType, Env, String)> = Vec::new();
    for (i, arm) in arms.iter().enumerate() {
        em.start_block(&matched_labels[i]);
        let (_, inner_ty) = &handles[i];
        let word = em.fresh_reg();
        em.push(format!("  {word} = load i64, ptr {}", out_slots[i]));
        let value = word_to_value(em, &word, inner_ty.clone());

        let mut arm_env = env.clone();
        let prior = arm_env.get("__select_recv").cloned();
        arm_env.insert("__select_recv".to_string(), (value, inner_ty.clone()));

        let (body_result, body_env) = codegen_expr(&arm.body, &arm_env, em, ctx, tail)?;
        let restored_env = restore_shadowed(body_env, "__select_recv", prior);
        if let Some((reg, ty)) = body_result {
            em.push(format!("  br label %{done_label}"));
            non_tail_results.push((reg, ty, restored_env, em.current_block.clone()));
        }
    }

    if tail {
        Ok((None, env.clone()))
    } else {
        em.start_block(&done_label);
        let ty = non_tail_results
            .first()
            .map(|(_, ty, _, _)| ty.clone())
            .ok_or_else(|| "codegen: internal error — select produced no reachable result".to_string())?;
        let phi_reg = em.fresh_reg();
        let parts: Vec<String> = non_tail_results.iter().map(|(reg, _, _, block)| format!("[ {reg}, %{block} ]")).collect();
        em.push(format!("  {phi_reg} = phi {} {}", ty.llvm_type(), parts.join(", ")));
        let branches: Vec<(Env, String)> = non_tail_results.into_iter().map(|(_, _, e, block)| (e, block)).collect();
        let merged_env = merge_envs(&branches, em);
        Ok((Some((phi_reg, ty)), merged_env))
    }
}

/// Shared callee-resolution + call-emission logic for BOTH `codegen_
/// value`'s plain (non-tail) `Call` handling and `codegen_expr`'s own
/// TAIL-position `Call` handling. Two paths:
///
/// - DIRECT: `callee` is a bare `Var(name)` where `name` names a known
///   top-level function (`ctx.sigs`) AND ISN'T SHADOWED in `env` — a
///   local variable sharing a top-level function's own name must route
///   through the INDIRECT path instead, since `env`, not `ctx.sigs`, is
///   authoritative for what a bare identifier resolves to. `musttail`
///   is used here (only) when `allow_musttail` (i.e. this call is in
///   tail position) AND the caller/callee signatures match exactly —
///   identical to this codegen's pre-existing direct-call behavior.
/// - INDIRECT: `codegen_value(callee)` must yield a `CgType::Closure`;
///   loads the code-ptr word, `inttoptr`s it to a `ptr`, and emits an
///   ORDINARY (never `musttail`) indirect `call`, passing the
///   environment pointer (the closure cell itself) as an implicit
///   first argument ahead of the ordinary args. `musttail` is
///   deliberately NEVER attempted here — the existing `musttail`
///   mechanism compares two known, NAMED `FnSig`s; an indirect call's
///   target is only known via `CgType::Closure`'s call SHAPE, a
///   different and weaker check, and indirect `musttail` has its own
///   LLVM legality constraints unexercised anywhere in this codebase.
///   This means a self-referential closure's own recursive tail call
///   is NOT guaranteed-tail-call-eliminated — a real, documented
///   limitation (direct calls to plain top-level functions are
///   completely unaffected).
///
/// Returns `(result_reg, result_ty, used_musttail)` — neither caller's
/// own `ret`/tail-terminator responsibility is handled here (the
/// non-tail caller never needs one; the tail caller always appends its
/// own `ret` regardless of `used_musttail`, matching this codegen's
/// pre-existing musttail-call-then-ret shape).
fn codegen_call(callee: &Expr, args: &[Expr], env: &Env, em: &mut Emitter, ctx: &Ctx, allow_musttail: bool) -> Result<(String, CgType, bool), String> {
    if let Expr::Var(name) = callee {
        if !env.contains_key(name) {
            if let Some(sig) = ctx.sigs.get(name).cloned() {
                // A bare identifier, not shadowed by a local, naming a
                // known top-level FUNCTION — the DIRECT path.
                let args_ir = codegen_call_args(args, env, em, ctx, &sig, name)?;
                let reg = em.fresh_reg();
                // `!ctx.is_closure_body` is a REQUIRED third condition,
                // not just `allow_musttail && caller_sig == sig` — see
                // `Ctx::is_closure_body`'s own doc comment. A closure
                // body's `caller_sig` only ever records its OWN
                // declared params, never the implicit leading `ptr
                // %env` its real LLVM prototype always has, so a
                // closure whose declared shape happens to match a
                // top-level function's `FnSig` exactly (a realistic,
                // not contrived, case — e.g. a `.fold()` callback
                // deliberately shaped to match the function it wraps)
                // would otherwise pass this check while actually
                // having one MORE real parameter than the callee,
                // producing a `musttail call` `clang`/LLVM correctly
                // rejects as malformed IR.
                if allow_musttail && !ctx.is_closure_body && *ctx.caller_sig == sig {
                    em.push(format!("  {reg} = musttail call {} @{name}({args_ir})", sig.ret.llvm_type()));
                    return Ok((reg, sig.ret, true));
                }
                em.push(format!("  {reg} = call {} @{name}({args_ir})", sig.ret.llvm_type()));
                return Ok((reg, sig.ret, false));
            }
            // Not a known top-level FUNCTION — but it might still be a
            // closure-typed GLOBAL (e.g. a self-referential global
            // closure calling itself by bare name, `fib(n-1)`), which
            // must fall through to the INDIRECT path below rather than
            // erroring out here: `ctx.globals` is `codegen_value`'s own
            // `Var` arm's third resolution tier, reached only via that
            // INDIRECT path's `codegen_value(callee, ..)` call just
            // past this `if`. Only report the clearer, more specific
            // "unknown function" error when the name is unknown
            // EVERYWHERE (env/sigs/globals) — otherwise `codegen_value`
            // would instead report its own generic "unbound variable"
            // message for a name that's actually a perfectly valid
            // (non-function) global.
            if !ctx.globals.contains_key(name) {
                return Err(format!("codegen: unknown function {name:?}"));
            }
        }
    }

    let (closure_reg, closure_ty) = codegen_value(callee, env, em, ctx)?;
    let CgType::Closure(param_tys, ret_ty) = closure_ty else {
        return Err(format!(
            "codegen requires a call to a directly-named function or a closure-typed value (found {closure_ty:?}) \
             — calling a non-closure computed value isn't supported"
        ));
    };
    let sig = FnSig { params: param_tys, ret: *ret_ty };
    let code_addr = em.fresh_reg();
    em.push(format!("  {code_addr} = getelementptr i8, ptr {closure_reg}, i64 8"));
    let code_word = em.fresh_reg();
    em.push(format!("  {code_word} = load i64, ptr {code_addr}"));
    let code_ptr = em.fresh_reg();
    em.push(format!("  {code_ptr} = inttoptr i64 {code_word} to ptr"));
    let args_ir = codegen_call_args(args, env, em, ctx, &sig, "<computed closure value>")?;
    let full_args = if args_ir.is_empty() { format!("ptr {closure_reg}") } else { format!("ptr {closure_reg}, {args_ir}") };
    let reg = em.fresh_reg();
    em.push(format!("  {reg} = call {} {code_ptr}({full_args})", sig.ret.llvm_type()));
    Ok((reg, sig.ret, false))
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
        Expr::Var(name) => {
            if let Some(v) = env.get(name) {
                return Ok(v.clone());
            }
            // Not a local — a bare top-level FUNCTION name used as a
            // VALUE (not called), e.g. `let f = someFn; f(1)`: see
            // `codegen_bare_fn_value`'s doc comment for the trampoline
            // this synthesizes.
            if let Some(sig) = ctx.sigs.get(name).cloned() {
                return codegen_bare_fn_value(name, &sig, em, ctx);
            }
            // Third, and last, resolution tier: a top-level `Global` —
            // see `Ctx::globals`'s own doc comment. Always a `load` of
            // the already-materialized slot, never a re-evaluation of
            // the initializer — `@plum_init_globals` (lib.rs) itself
            // reaches this exact same arm for a later global's
            // reference to an earlier one, which is what makes that
            // property correct BY CONSTRUCTION rather than by
            // convention.
            if let Some(ty) = ctx.globals.get(name).cloned() {
                let r = em.fresh_reg();
                em.push(format!("  {r} = load {}, ptr @global.{name}", ty.llvm_type()));
                return Ok((r, ty));
            }
            Err(format!("codegen: unbound variable {name:?}"))
        }
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
            let (reg, ty, _) = codegen_call(callee, args, env, em, ctx, false)?;
            Ok((reg, ty))
        }
        // `tag == ARRAY_TAG` is a non-empty array literal — see this
        // module's array-literal section above — special-cased here
        // (duplicated-as-local-const, same as `DEFAULT_ARM_TAG` already
        // is) to route to the array-alloc path instead of the ordinary
        // tag-lookup `Ctor` path; every OTHER tag is unaffected.
        Expr::Channel => codegen_channel_literal(em, ctx),
        Expr::ChannelSend { sender, value } => codegen_channel_send(sender, value, env, em, ctx),
        Expr::ChannelRecv { receiver } => codegen_channel_recv(receiver, env, em, ctx),
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
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = codegen_ctor_alloc(tag, &field_vals, em, ctx)?;
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {old_ptr}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
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
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_push_fresh(&old_ptr, (*elem_ty).clone(), &val_reg, em, ctx);
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
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
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_pop_fresh(&old_ptr, (*elem_ty).clone(), em, ctx);
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
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
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_set_fresh(&old_ptr, (*elem_ty).clone(), &idx_reg, &val_reg, em, ctx);
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {old_ptr}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
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
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let (fresh, _) = codegen_array_remove_fresh(&old_ptr, (*elem_ty).clone(), &idx_reg, em, ctx);
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
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
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = em.fresh_reg();
            em.push(format!("  {fresh} = call ptr @plum_str_concat(ptr {old_ptr}, ptr {o_reg})"));
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {grown}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
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
        // `s.runes()` — a thin call into `@plum_str_runes` (`lib.rs`'s
        // `emit_runtime`), which does the actual two-pass UTF-8 decode.
        // No `*Reuse` variant exists in the IR (see `ir::Expr::StrRunes`'s
        // own doc comment: this always builds a brand new `Array[Int]`,
        // a differently-shaped heap value than the `Str` it reads from).
        Expr::StrRunes { base } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.runes()` requires a Str value, found {bty:?}"));
            }
            register_array_elem(ctx, &CgType::Int);
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_str_runes(ptr {b})"));
            Ok((r, CgType::Array(Box::new(CgType::Int))))
        }
        // `.trim()` — fresh path: a plain call into `@plum_str_trim`. See
        // `StrTrimReuse` for the reuse-in-place half.
        Expr::StrTrim { base } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.trim()` requires a Str value, found {bty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_str_trim(ptr {b})"));
            Ok((r, CgType::Str))
        }
        // The reuse-in-place half of `.trim()` — same refcount-check-
        // then-branch shape every other `*Reuse` string op uses. Unlike
        // `.concat()`'s reuse path, trimming only ever SHRINKS, so the
        // reuse branch needs no `@realloc` at all — `@plum_str_trim_
        // inplace` just `@memmove`s the trimmed range down to offset 16
        // and updates `len` in place.
        Expr::StrTrimReuse { reuse_of } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            if old_ty != CgType::Str {
                return Err(format!("codegen: internal error — StrTrimReuse target {reuse_of:?} is not a Str"));
            }

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("str_trim_reuse");
            let alloc_label = em.fresh_label("str_trim_fresh");
            let merge_label = em.fresh_label("str_trim_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            em.push(format!("  call void @plum_str_trim_inplace(ptr {old_ptr})"));
            em.push(format!("  store i64 1, ptr {old_ptr}"));
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = em.fresh_reg();
            em.push(format!("  {fresh} = call ptr @plum_str_trim(ptr {old_ptr})"));
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {old_ptr}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
            Ok((result, CgType::Str))
        }
        // `.to_upper()` — fresh path: a plain call into `@plum_str_to_
        // upper`, which does real Unicode SIMPLE case mapping via libc's
        // `towupper` (locale-aware, one-codepoint-in-one-codepoint-out —
        // see `lib.rs`'s `emit_runtime` for the full mechanism and
        // `@plum_locale_init`). Remaining, precisely-scoped divergence
        // from the interpreter's full Unicode `str::to_uppercase()`:
        // multi-codepoint expansions (e.g. German `ß` -> `"SS"`)
        // structurally can't happen through `towupper`'s 1-in-1-out C
        // signature, so `ß` stays `ß`. See DESIGN.md's "Strings" section
        // for the language-level caveat.
        Expr::StrToUpper { base } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.to_upper()` requires a Str value, found {bty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_str_to_upper(ptr {b})"));
            Ok((r, CgType::Str))
        }
        // The reuse-in-place half of `.to_upper()` — same refcount-
        // check-then-branch shape as every other `*Reuse` string op,
        // but with the SAME documented deviation `StrReplaceReuse`
        // already established (see that arm's own doc comment below):
        // now that case mapping goes through full Unicode `towupper`,
        // it can change a string's total BYTE length (a mapped
        // codepoint's UTF-8 length can differ from its source's), the
        // exact soundness hazard that made true in-place mutation
        // unsound for `StrReplaceReuse`'s growing case. So once
        // uniquely owned, this calls the SAME fresh-allocating
        // `@plum_str_to_upper` (always memory-safe) and then frees the
        // OLD cell directly, rather than reusing its buffer in place.
        Expr::StrToUpperReuse { reuse_of } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            if old_ty != CgType::Str {
                return Err(format!("codegen: internal error — StrToUpperReuse target {reuse_of:?} is not a Str"));
            }

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("str_to_upper_reuse");
            let alloc_label = em.fresh_label("str_to_upper_fresh");
            let merge_label = em.fresh_label("str_to_upper_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            let reused = em.fresh_reg();
            em.push(format!("  {reused} = call ptr @plum_str_to_upper(ptr {old_ptr})"));
            em.push(format!("  call void @free(ptr {old_ptr})"));
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = em.fresh_reg();
            em.push(format!("  {fresh} = call ptr @plum_str_to_upper(ptr {old_ptr})"));
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {reused}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
            Ok((result, CgType::Str))
        }
        // `.to_lower()` — same mechanism and shape as `.to_upper()`,
        // delegating to `@plum_str_to_lower`/libc's `towlower`.
        Expr::StrToLower { base } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.to_lower()` requires a Str value, found {bty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_str_to_lower(ptr {b})"));
            Ok((r, CgType::Str))
        }
        // The reuse-in-place half of `.to_lower()` — same documented
        // deviation as `StrToUpperReuse` above (and `StrReplaceReuse`
        // before it): full Unicode `towlower` case mapping can change a
        // string's total byte length, so once uniquely owned this calls
        // the fresh-allocating `@plum_str_to_lower` and frees the OLD
        // cell directly rather than mutating in place.
        Expr::StrToLowerReuse { reuse_of } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            if old_ty != CgType::Str {
                return Err(format!("codegen: internal error — StrToLowerReuse target {reuse_of:?} is not a Str"));
            }

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("str_to_lower_reuse");
            let alloc_label = em.fresh_label("str_to_lower_fresh");
            let merge_label = em.fresh_label("str_to_lower_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            let reused = em.fresh_reg();
            em.push(format!("  {reused} = call ptr @plum_str_to_lower(ptr {old_ptr})"));
            em.push(format!("  call void @free(ptr {old_ptr})"));
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = em.fresh_reg();
            em.push(format!("  {fresh} = call ptr @plum_str_to_lower(ptr {old_ptr})"));
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {reused}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
            Ok((result, CgType::Str))
        }
        // `s.split(sep)` — a thin call into `@plum_str_split`, which does
        // the actual (runtime-branching on empty-vs-non-empty `sep`)
        // two-pass piece-cutting. No `*Reuse` variant, same reasoning as
        // `.runes()`.
        Expr::StrSplit { base, sep } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.split()` requires a Str value, found {bty:?}"));
            }
            let (s, sty) = codegen_value(sep, env, em, ctx)?;
            if sty != CgType::Str {
                return Err(format!("codegen: `.split()` argument must be Str, found {sty:?}"));
            }
            register_array_elem(ctx, &CgType::Str);
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_str_split(ptr {b}, ptr {s})"));
            Ok((r, CgType::Array(Box::new(CgType::Str))))
        }
        // `.replace(from, to)` — fresh path: a plain call into
        // `@plum_str_replace`. See `StrReplaceReuse` for the reuse
        // half.
        Expr::StrReplace { base, from, to } => {
            let (b, bty) = codegen_value(base, env, em, ctx)?;
            if bty != CgType::Str {
                return Err(format!("codegen: `.replace()` requires a Str value, found {bty:?}"));
            }
            let (f, fty) = codegen_value(from, env, em, ctx)?;
            if fty != CgType::Str {
                return Err(format!("codegen: `.replace()` first argument must be Str, found {fty:?}"));
            }
            let (t, tty) = codegen_value(to, env, em, ctx)?;
            if tty != CgType::Str {
                return Err(format!("codegen: `.replace()` second argument must be Str, found {tty:?}"));
            }
            let r = em.fresh_reg();
            em.push(format!("  {r} = call ptr @plum_str_replace(ptr {b}, ptr {f}, ptr {t})"));
            Ok((r, CgType::Str))
        }
        // The reuse-in-place half of `.replace()` — same refcount-check-
        // then-branch shape as every other `*Reuse` string op, but with
        // a DELIBERATE, DOCUMENTED deviation from this chunk's own
        // design notes: those notes called for the reuse branch to
        // `@realloc` the OLD cell to the newly-computed final size and
        // fill it in place. That's unsound for the GROWING case
        // (`to`'s bytes longer than `from`'s): a naive forward copy
        // reading `%s` while simultaneously writing an EXPANDED result
        // into the SAME buffer can overwrite source bytes the read
        // cursor hasn't reached yet the moment the write cursor drifts
        // ahead of it (verified by hand-tracing `"aa".replace("a",
        // "bbb")` byte-by-byte — the second `'a'` gets clobbered before
        // it's ever read). A fully correct in-place version exists (a
        // right-to-left, `@memmove`-per-gap walk, since `@memmove`
        // itself already handles arbitrary single-range overlap
        // correctly) but is real, non-trivial new algorithm surface
        // this chunk defers rather than risk shipping unverified. So:
        // once uniquely owned, this still calls the SAME fresh-
        // allocating `@plum_str_replace` (always memory-safe — it never
        // aliases source and destination), then frees the OLD cell
        // directly (skipping `@plum_rc_dec_str`'s own redundant second
        // refcount round-trip, since this arm already established the
        // count reached zero) rather than leaving it to a caller-side
        // `Dec`. This still exercises the refcount-gated reuse-vs-fresh
        // distinction meaningfully (freeing without a second decrement),
        // just without a genuine buffer-reuse performance win.
        Expr::StrReplaceReuse { reuse_of, from, to } => {
            let (old_ptr, old_ty) = env
                .get(reuse_of)
                .cloned()
                .ok_or_else(|| format!("codegen: unbound variable {reuse_of:?}"))?;
            if old_ty != CgType::Str {
                return Err(format!("codegen: internal error — StrReplaceReuse target {reuse_of:?} is not a Str"));
            }
            let (f, fty) = codegen_value(from, env, em, ctx)?;
            if fty != CgType::Str {
                return Err(format!("codegen: `.replace()` first argument must be Str, found {fty:?}"));
            }
            let (t, tty) = codegen_value(to, env, em, ctx)?;
            if tty != CgType::Str {
                return Err(format!("codegen: `.replace()` second argument must be Str, found {tty:?}"));
            }

            let rc = em.fresh_reg();
            em.push(format!("  {rc} = load i64, ptr {old_ptr}"));
            let rc2 = em.fresh_reg();
            em.push(format!("  {rc2} = sub i64 {rc}, 1"));
            em.push(format!("  store i64 {rc2}, ptr {old_ptr}"));
            let is_zero = em.fresh_reg();
            em.push(format!("  {is_zero} = icmp eq i64 {rc2}, 0"));

            let reuse_label = em.fresh_label("str_replace_reuse");
            let alloc_label = em.fresh_label("str_replace_fresh");
            let merge_label = em.fresh_label("str_replace_merge");
            em.push(format!("  br i1 {is_zero}, label %{reuse_label}, label %{alloc_label}"));

            em.start_block(&reuse_label);
            let reused = em.fresh_reg();
            em.push(format!("  {reused} = call ptr @plum_str_replace(ptr {old_ptr}, ptr {f}, ptr {t})"));
            em.push(format!("  call void @free(ptr {old_ptr})"));
            let reuse_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&alloc_label);
            let fresh = em.fresh_reg();
            em.push(format!("  {fresh} = call ptr @plum_str_replace(ptr {old_ptr}, ptr {f}, ptr {t})"));
            let alloc_end = em.current_block().to_string();
            em.push(format!("  br label %{merge_label}"));

            em.start_block(&merge_label);
            let result = em.fresh_reg();
            em.push(format!("  {result} = phi ptr [ {reused}, %{reuse_end} ], [ {fresh}, %{alloc_end} ]"));
            Ok((result, CgType::Str))
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
                    // `%.15g`, not `%f`: `%f` always prints exactly 6
                    // decimal places (`3.0` -> `"3.000000"`), which
                    // diverges badly from the interpreter's Rust-
                    // `Display`-based rendering (`3.0` -> `"3"`). `%g`
                    // already omits trailing zero decimals for whole
                    // numbers, and 15 significant digits matches `f64`'s
                    // own precision — closely matches the interpreter
                    // for ordinary program values, though not
                    // byte-perfect at the extremes (e.g. `%g`'s `1e+20`
                    // vs Rust's `1e20`), a documented, honest caveat,
                    // not a silent gap.
                    let fmt = em.fresh_string_global(ctx.fn_name, b"%.15g\0");
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
                // Struct/enum (`Heap`) and array `.to_string()` —
                // dispatches to the generic `@plum_struct_to_string`
                // (one function for every struct/enum shape) or the
                // per-element-type `@plum_array_to_string_<mangled>`,
                // mirroring the equality chunk's `Heap`/`Array(_)`
                // `codegen_binop` wiring exactly (a call-shaped
                // instruction, not something that fits an inline
                // pattern). `Tuple` is unreachable here — rejected
                // earlier, at type-checking time (`plum-types::infer`'s
                // `.to_string()` gate excludes it, since `CgType` has
                // no `Tuple` variant at all).
                CgType::Heap => {
                    let result = em.fresh_reg();
                    em.push(format!("  {result} = call ptr @plum_struct_to_string(ptr {reg})"));
                    Ok((result, CgType::Str))
                }
                CgType::Array(_) => {
                    let Some(to_string_fn) = crate::to_string_fn_for(&ty) else {
                        return Err(format!("codegen: `.to_string()` is not supported for {ty:?}"));
                    };
                    let result = em.fresh_reg();
                    em.push(format!("  {result} = call ptr {to_string_fn}(ptr {reg})"));
                    Ok((result, CgType::Str))
                }
                other => Err(format!("codegen: `.to_string()` is not supported for {other:?}")),
            }
        }
        // The resulting `Env` `codegen_expr` also returns is deliberately
        // DISCARDED here (`.0` only) — this arm is only ever reached
        // from an ordinary VALUE context (a `Binary`/`Unary` operand, a
        // `Call` argument, a `Ctor` field, ...), where nothing downstream
        // has any way to observe an env change even if one occurred (an
        // `Assign` nested inside one of these four constructs, itself
        // used as a plain value rather than in `codegen_expr`'s own
        // statement-sequencing position). Every shape this chunk's own
        // tests exercise (`for`/`.map()`/`.filter()`/`.fold()`) routes
        // `Assign`/`For` through `codegen_expr`'s dedicated `Let`-value
        // special case instead (see that arm's doc comment), which DOES
        // thread the env through correctly — this discard only affects
        // the narrower, not-yet-exercised case of an `Assign` nested
        // inside a value-position `Let`/`If`/`Match`/`RcAnnotated`.
        Expr::Let { .. } | Expr::If { .. } | Expr::Match { .. } | Expr::RcAnnotated { .. } | Expr::Select { .. } => {
            match codegen_expr(expr, env, em, ctx, false)? {
                (Some(v), _) => Ok(v),
                (None, _) => unreachable!("codegen_expr with tail=false always returns Some"),
            }
        }
        // An ordinary (non-self-referential) closure literal — the
        // self-referential LOCAL case is special-cased in `codegen_
        // expr`'s own `Let` arm instead, since it needs to bind the
        // closure's OWN name into scope before this function even runs
        // — see `codegen_closure_literal`'s doc comment.
        Expr::Closure { params, param_types, ret_type, body } => {
            codegen_closure_literal(params, param_types, ret_type, body, env, em, ctx, None)
        }
        Expr::Spawn { block } => codegen_spawn_literal(block, env, em, ctx),
        Expr::TaskJoin { task } => codegen_task_join(task, env, em, ctx),
        Expr::ExternCall { name, args } => codegen_extern_call(name, args, env, em, ctx),
        Expr::AsCStr(inner) => codegen_as_cstr(inner, env, em, ctx),
        Expr::ReadFileRaw { path } => codegen_read_file_raw(path, env, em, ctx),
        Expr::WriteFileRaw { path, contents } => codegen_write_file_raw(path, contents, env, em, ctx),
        Expr::PanicRaw { message } => codegen_panic_raw(message, env, em, ctx),
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

/// Restores `env`'s entry for `name` to what it was BEFORE some inner
/// scope (a `Let` binding, a `Match` arm's field bindings) introduced or
/// shadowed it — `prior` is whatever `name` mapped to immediately before
/// that inner scope started (`None` if `name` wasn't bound at all yet).
/// Needed so a scope's OWN local binding never leaks into the `Env` it
/// hands back to its caller: `merge_envs` (below) relies on every
/// branch-produced `Env` sharing the exact same key set as the `Env` it
/// was originally given, and this is what keeps that invariant true
/// after a `Let`/`Match` arm's own local names have gone out of scope.
fn restore_shadowed(mut env: Env, name: &str, prior: Option<(String, CgType)>) -> Env {
    match prior {
        Some(v) => {
            env.insert(name.to_string(), v);
        }
        None => {
            env.remove(name);
        }
    }
    env
}

/// Generalizes `If`/`Match`'s existing branch-VALUE phi-merge to also
/// phi-merge any `Env` entry whose register diverged between branches —
/// this alone is what makes `.filter()`'s desugaring (an `Assign` nested
/// inside one arm of an `If`) codegen correctly, with no separate
/// detection walk needed: divergence is found by diffing the branches'
/// own resulting envs directly, not by re-analyzing the source `Expr`.
///
/// Every `Env` in `branches` is assumed to share the exact same key set
/// (see `restore_shadowed`'s doc comment for why that invariant holds:
/// `Assign` only ever overrides an EXISTING key, never introduces one,
/// and a branch's own `Let`/`Match`-local names are always stripped
/// back out before reaching here) — panics via an out-of-bounds map
/// index otherwise, which would itself indicate a real bug in one of
/// `codegen_expr`'s OTHER arms, not a case callers need to guard against
/// here. Must be called with `em`'s current block already being the
/// actual merge point (`If`'s `merge_label` / `Match`'s `done_label`) —
/// every phi this emits is pushed into whatever block is current when
/// called, same requirement the pre-existing value-phi logic already
/// has. A key whose register is IDENTICAL across every branch gets no
/// phi at all (just reuses that one shared register) — avoids emitting
/// a redundant trivial phi for the overwhelming majority of `Env`
/// entries, which no branch ever touches.
fn merge_envs(branches: &[(Env, String)], em: &mut Emitter) -> Env {
    let mut merged = Env::new();
    let Some((first_env, _)) = branches.first() else {
        return merged;
    };
    for key in first_env.keys() {
        let (first_reg, ty) = first_env[key].clone();
        let all_same = branches.iter().all(|(e, _)| e[key].0 == first_reg);
        if all_same {
            merged.insert(key.clone(), (first_reg, ty));
        } else {
            let phi_reg = em.fresh_reg();
            let parts: Vec<String> =
                branches.iter().map(|(e, block)| format!("[ {}, %{block} ]", e[key].0)).collect();
            em.push(format!("  {phi_reg} = phi {} {}", ty.llvm_type(), parts.join(", ")));
            merged.insert(key.clone(), (phi_reg, ty));
        }
    }
    merged
}

/// See this module's doc comment for the full tail-position story. As
/// of this chunk, also returns the resulting `Env` — an ordinary,
/// behavior-preserving addition for every arm except `Assign` (one
/// entry overridden) and `For` (loop-carried names' post-loop
/// registers): every OTHER arm just returns back the SAME env it was
/// given (or, for `Let`/`Match`, that env with its own local binding(s)
/// stripped back out via `restore_shadowed` — see that function's doc
/// comment for why). Needed so a `for` loop body's `Assign`s (and an
/// `Assign` anywhere else) can make their reassignment visible to
/// whatever code follows, since `Env` itself stays a plain immutable
/// map, threaded purely through return values, exactly as before.
pub(crate) fn codegen_expr(expr: &Expr, env: &Env, em: &mut Emitter, ctx: &Ctx, tail: bool) -> Result<(Option<(String, CgType)>, Env), String> {
    match expr {
        // A closure-literal `value` is special-cased to support
        // self-referential local closures (`let fib = |n| ... fib(n-1)
        // ...`) — see `codegen_closure_literal`'s `self_bind` parameter
        // doc comment for the full cell-before-self-store ordering this
        // requires. Every OTHER `Let` (including one whose `value`
        // happens to just be a bare reference to an EXISTING closure,
        // `let g = f`) is unaffected — `Expr::Var`'s own codegen
        // already handles that ordinary case.
        Expr::Let { name, value, body } if matches!(value.as_ref(), Expr::Closure { .. }) => {
            let Expr::Closure { params, param_types, ret_type, body: cbody } = value.as_ref() else {
                unreachable!("guarded by the match arm's own pattern");
            };
            let bound = codegen_closure_literal(params, param_types, ret_type, cbody, env, em, ctx, Some(name))?;
            let prior = env.get(name).cloned();
            let mut inner_env = env.clone();
            inner_env.insert(name.clone(), bound);
            let (result, result_env) = codegen_expr(body, &inner_env, em, ctx, tail)?;
            Ok((result, restore_shadowed(result_env, name, prior)))
        }
        // Every OTHER `Let` value is routed through `codegen_expr`
        // itself (tail=false), NOT `codegen_value` directly — this is
        // what makes a `for`/`Assign` reachable from `value` (directly,
        // OR nested inside a `Let`/`If`/`Match`/`RcAnnotated` wrapper —
        // exactly the shape `lower_for`'s `for x in arr` desugaring
        // produces: `Let { arr_name, .., body: For { .. } }` as a
        // WHOLE is itself the outer statement-sequencing `Let`'s
        // `value`) correctly propagate its env changes into `body`,
        // with zero special-casing needed for any particular VALUE
        // shape: whichever arm `value` itself matches (`For`, `Assign`,
        // `Let`, `If`, `Match`, `RcAnnotated`, or the ordinary catch-all
        // that wraps a plain `codegen_value` call in `(Some(v),
        // env.clone())`) already threads its own env correctly, so this
        // one path handles all of them uniformly. `codegen_value`'s OWN
        // signature stays unchanged regardless (see this module's doc
        // comment) — this is a change to what `Let` calls, not to
        // `codegen_value` itself; every OTHER value-context caller (a
        // `Binary`/`Unary` operand, a `Call` argument, a `Ctor` field,
        // ...) still calls `codegen_value` directly and still can't
        // observe an env change from a `Let`/`If`/`Match`/`RcAnnotated`
        // value it contains — an `Assign` reachable ONLY through one of
        // THOSE positions (never through an enclosing `Let`'s own
        // value) remains a known, deliberately out-of-scope gap (no
        // construct this chunk's own tests exercise reaches it).
        Expr::Let { name, value, body } => {
            let (bound_opt, value_env) = codegen_expr(value, env, em, ctx, false)?;
            let bound = bound_opt.ok_or_else(|| {
                "codegen: internal error — a `let`'s value produced no result (codegen_expr with tail=false \
                 should always return Some)"
                    .to_string()
            })?;
            let prior = value_env.get(name).cloned();
            let mut inner_env = value_env;
            inner_env.insert(name.clone(), bound);
            let (result, result_env) = codegen_expr(body, &inner_env, em, ctx, tail)?;
            Ok((result, restore_shadowed(result_env, name, prior)))
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
                    if !matches!(ty, CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..)) {
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
            let (then_result, then_env) = codegen_expr(then_branch, env, em, ctx, tail)?;
            if then_result.is_some() {
                em.push(format!("  br label %{merge_label}"));
            }
            let then_end_block = em.current_block.clone();

            em.start_block(&else_label);
            let (else_result, else_env) = codegen_expr(else_branch, env, em, ctx, tail)?;
            if else_result.is_some() {
                em.push(format!("  br label %{merge_label}"));
            }
            let else_end_block = em.current_block.clone();

            match (then_result, else_result) {
                // Both arms share the same `tail` flag (inherited from
                // this `If` itself), so both always return `None`
                // (already emitted their own terminator) or both
                // always return `Some` (still need this `If` to merge
                // them) — never a mix. Both branches already terminated
                // in a `ret`, so there's no "after" for an env to matter
                // to — `env.clone()` is returned purely to satisfy the
                // signature, never actually consumed by a caller.
                (None, None) => Ok((None, env.clone())),
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
                    let merged_env =
                        merge_envs(&[(then_env, then_end_block), (else_env, else_end_block)], em);
                    Ok((Some((phi_reg, then_ty)), merged_env))
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
            let mut non_tail_results: Vec<(String, CgType, Env, String)> = Vec::new();

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
                // Captured BEFORE this arm's own field bindings are
                // added below — restored via `restore_shadowed` once
                // the arm's body has been codegen'd, so this arm's own
                // local names never leak into the `Env` handed back to
                // `merge_envs` (see that function's key-set invariant).
                let priors: Vec<(String, Option<(String, CgType)>)> =
                    arm.bindings.iter().map(|b| (b.clone(), env.get(b).cloned())).collect();
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

                let (body_result, body_env) = codegen_expr(&arm.body, &arm_env, em, ctx, tail)?;
                let mut restored_env = body_env;
                for (name, prior) in priors {
                    restored_env = restore_shadowed(restored_env, &name, prior);
                }
                if let Some((reg, ty)) = body_result {
                    em.push(format!("  br label %{done_label}"));
                    non_tail_results.push((reg, ty, restored_env, em.current_block.clone()));
                }
                em.start_block(&next_label);
            }
            // Every arm's tag/guard check failed — `plum-types` already
            // proved match exhaustiveness before codegen ever runs, so
            // this is genuinely unreachable for a well-typed program,
            // not just "shouldn't happen."
            em.push("  unreachable");

            if tail {
                Ok((None, env.clone()))
            } else {
                em.start_block(&done_label);
                let ty = non_tail_results
                    .first()
                    .map(|(_, ty, _, _)| ty.clone())
                    .ok_or_else(|| "codegen: internal error — match produced no reachable result".to_string())?;
                let phi_reg = em.fresh_reg();
                let parts: Vec<String> =
                    non_tail_results.iter().map(|(reg, _, _, block)| format!("[ {reg}, %{block} ]")).collect();
                em.push(format!("  {phi_reg} = phi {} {}", ty.llvm_type(), parts.join(", ")));
                let branches: Vec<(Env, String)> =
                    non_tail_results.into_iter().map(|(_, _, e, block)| (e, block)).collect();
                let merged_env = merge_envs(&branches, em);
                Ok((Some((phi_reg, ty)), merged_env))
            }
        }
        // `select { ... }` — see `codegen_select`'s own doc comment for
        // the full busy-poll algorithm and its Match-mirroring result-
        // merging scheme.
        Expr::Select { arms } => codegen_select(arms, env, em, ctx, tail),
        // `name = value; rest` — structurally almost identical to
        // `Let`'s own arm (compute the new value, override ONE existing
        // `Env` entry, recurse into `rest` with `tail` unchanged): see
        // ir.rs's `Assign` doc comment and `fbip.rs`'s own `Assign`
        // handling for why this deliberately emits NO `Dec` for the
        // value being overwritten — an accepted leak, matching the
        // existing `reassigning_a_heap_tracked_variable_leaks_the_old_
        // value_by_design` precedent, not a soundness gap codegen
        // should try to independently "fix." Unlike `Let`, `Assign`
        // never introduces a new key (only overrides an EXISTING one —
        // an assignment to a name `env` doesn't already know about is a
        // clear error, never a silent insert), so `rest`'s resulting env
        // is forwarded straight through with no `restore_shadowed` step
        // needed.
        Expr::Assign { name, value, rest } => {
            let (val_reg, val_ty) = codegen_value(value, env, em, ctx)?;
            let (_, old_ty) = env
                .get(name)
                .cloned()
                .ok_or_else(|| format!("codegen: assignment to undeclared variable {name:?}"))?;
            if val_ty != old_ty {
                return Err(format!(
                    "codegen: assignment to {name:?}: expected {old_ty:?}, found {val_ty:?}"
                ));
            }
            let mut new_env = env.clone();
            new_env.insert(name.clone(), (val_reg, val_ty));
            codegen_expr(rest, &new_env, em, ctx, tail)
        }
        // See this function's own doc comment for the concrete block
        // structure (`preheader` -> `header` -> `body` -> back to
        // `header`, or out to `after`) and the dominance argument for
        // why every phi register this produces is safely usable past
        // the loop.
        Expr::For { var, start, end, body } => codegen_for(var, start, end, body, env, em, ctx, tail),
        // `musttail` is only VALID when the caller's own prototype
        // matches the callee's (a real LLVM constraint — "cannot
        // guarantee tail call due to mismatched parameter counts" —
        // found via an actual `clang` compile failure, not documented
        // up front; see `Ctx::caller_sig`'s doc comment). Self-
        // recursion always trivially qualifies; mutual recursion only
        // does when both functions happen to share a signature. A tail
        // call to a DIFFERENT-shaped function, OR through an INDIRECT
        // closure-typed callee (see `codegen_call`'s own doc comment
        // for why `musttail` is never attempted there this chunk),
        // falls back to an ordinary `call` + `ret` — still correct,
        // just not `musttail`-GUARANTEED to reuse the stack frame.
        Expr::Call { callee, args } if tail => {
            let (reg, ty, _used_musttail) = codegen_call(callee, args, env, em, ctx, true)?;
            em.push(format!("  ret {} {reg}", ty.llvm_type()));
            Ok((None, env.clone()))
        }
        _ => {
            let (reg, ty) = codegen_value(expr, env, em, ctx)?;
            if tail {
                em.push(format!("  ret {} {reg}", ty.llvm_type()));
                Ok((None, env.clone()))
            } else {
                Ok((Some((reg, ty)), env.clone()))
            }
        }
    }
}

/// `for var in start..end { body }` — SSA phi-threading, not stack
/// allocation (keeping this backend's "everything is SSA registers +
/// heap cells, never mutable stack memory" character intact): the
/// induction variable AND every name `body` reassigns (`assigned_vars`)
/// each get their own header-block phi.
///
/// Concrete block structure:
/// ```text
/// preheader: compute start/end once, br to header
/// header:    %i = phi i64 [start, preheader], [i_next, body_end]     ; RESERVED, patched after body
///            %carriedN = phi T [preN, preheader], [finalN, body_end] ; one per assigned_vars() name, RESERVED
///            icmp slt i64 %i, %end; br body/after
/// body:      seed body_env with i + carried phis; codegen_expr(body, ..., tail=false) ALWAYS
///            (body's value is always discarded each iteration, independent
///            of this `For` node's own tail position)
/// body_end:  i_next = add i64 %i, 1; br header
///            (`body_end` isn't a separate block — it's simply whatever
///            block `body`'s own codegen left `em` in, exactly like
///            `If`/`Match`'s existing `then_end_block`/`else_end_block`
///            capture already works)
///            patch header's phi lines using body_end's final env
/// after:     For evaluates to Unit; post_env = env with each carried name's
///            register updated to its header phi
/// ```
///
/// Dominance: `after` is reached ONLY via `header`'s own conditional
/// branch (`icmp` + `br i1 .., body, after`) — no other edge into it
/// exists anywhere in this structure — so `header` dominates `after`,
/// which is exactly what makes every phi register defined at `header`
/// (the induction variable AND every carried name) a valid, usable SSA
/// value for whatever code runs after the loop (`post_env`'s whole
/// point). The header's own phis are similarly valid at `body`'s entry
/// for the same reason (`header` dominates `body` — its `br i1` is
/// `body`'s only predecessor edge from outside the loop, and the
/// `body_end -> header` back-edge is exactly what an ordinary loop-carry
/// phi is built to accept as its second incoming value).
#[allow(clippy::too_many_arguments)]
fn codegen_for(
    var: &str,
    start: &Expr,
    end: &Expr,
    body: &Expr,
    env: &Env,
    em: &mut Emitter,
    ctx: &Ctx,
    tail: bool,
) -> Result<(Option<(String, CgType)>, Env), String> {
    let (start_reg, start_ty) = codegen_value(start, env, em, ctx)?;
    if start_ty != CgType::Int {
        return Err(format!("codegen: `for` loop start must be Int, found {start_ty:?}"));
    }
    let (end_reg, end_ty) = codegen_value(end, env, em, ctx)?;
    if end_ty != CgType::Int {
        return Err(format!("codegen: `for` loop end must be Int, found {end_ty:?}"));
    }

    // Every outer-scope name `body` reassigns — each needs its own
    // loop-header phi so the reassignment is visible both to LATER
    // iterations of `body` itself (read back via the phi at `body`'s
    // entry) and to whatever code runs after the loop (`post_env`,
    // below). See `assigned_vars`'s own doc comment for the full
    // Closure-hard-stop story.
    let carried_names = assigned_vars(body);
    let mut carried: Vec<(String, CgType, String)> = Vec::with_capacity(carried_names.len());
    for name in &carried_names {
        let (pre_reg, ty) = env.get(name).cloned().ok_or_else(|| {
            format!("codegen: `for` loop body reassigns undeclared variable {name:?}")
        })?;
        carried.push((name.clone(), ty, pre_reg));
    }

    let preheader_block = em.current_block().to_string();
    let header_label = em.fresh_label("for_header");
    let body_label = em.fresh_label("for_body");
    let after_label = em.fresh_label("for_after");
    em.push(format!("  br label %{header_label}"));

    em.start_block(&header_label);
    // Reserved now, patched once `body`'s final register (`i_next`) and
    // each carried name's final register are known — see `Emitter::
    // reserve_line`'s own doc comment for why this is needed here but
    // never was for `If`/`Match`'s own phis.
    let i_reg = em.fresh_reg();
    let i_phi_idx = em.reserve_line();
    let mut carried_phis: Vec<(String, CgType, usize, String)> = Vec::with_capacity(carried.len());
    for (name, ty, _) in &carried {
        let phi_reg = em.fresh_reg();
        let idx = em.reserve_line();
        carried_phis.push((name.clone(), ty.clone(), idx, phi_reg));
    }
    let cmp_reg = em.fresh_reg();
    em.push(format!("  {cmp_reg} = icmp slt i64 {i_reg}, {end_reg}"));
    em.push(format!("  br i1 {cmp_reg}, label %{body_label}, label %{after_label}"));

    em.start_block(&body_label);
    let mut body_env = env.clone();
    body_env.insert(var.to_string(), (i_reg.clone(), CgType::Int));
    for (name, ty, _, phi_reg) in &carried_phis {
        body_env.insert(name.clone(), (phi_reg.clone(), ty.clone()));
    }
    // ALWAYS `tail=false`, independent of this `For` node's OWN `tail`
    // position — see this function's doc comment: `body`'s value is
    // discarded every iteration regardless.
    let (_, body_result_env) = codegen_expr(body, &body_env, em, ctx, false)?;
    let body_end_block = em.current_block().to_string();
    let i_next = em.fresh_reg();
    em.push(format!("  {i_next} = add i64 {i_reg}, 1"));
    em.push(format!("  br label %{header_label}"));

    em.patch_line(
        i_phi_idx,
        format!("  {i_reg} = phi i64 [ {start_reg}, %{preheader_block} ], [ {i_next}, %{body_end_block} ]"),
    );
    let mut post_env = env.clone();
    for (name, ty, idx, phi_reg) in &carried_phis {
        let pre_reg = carried
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, r)| r.clone())
            .expect("carried_phis was built from carried, same names");
        let (final_reg, _) = body_result_env.get(name).cloned().ok_or_else(|| {
            format!(
                "codegen: internal error — loop-carried variable {name:?} missing from `for` body's resulting env"
            )
        })?;
        em.patch_line(
            *idx,
            format!("  {phi_reg} = phi {} [ {pre_reg}, %{preheader_block} ], [ {final_reg}, %{body_end_block} ]", ty.llvm_type()),
        );
        post_env.insert(name.clone(), (phi_reg.clone(), ty.clone()));
    }

    em.start_block(&after_label);
    if tail {
        em.push("  ret i1 0");
        Ok((None, post_env))
    } else {
        Ok((Some(("0".to_string(), CgType::Unit)), post_env))
    }
}
