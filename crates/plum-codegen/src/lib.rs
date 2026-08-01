mod codegen;

use plum_ir::ir;
use std::collections::HashMap;

/// LLVM IR type a Plum value maps to. Deliberately NOT `plum_types::
/// Type` — that would pull a `plum-types` dependency in for a handful
/// of primitive cases; `plum-codegen` stays self-contained and
/// testable in isolation with hand-built `ir::Program` values,
/// matching every other crate in this workspace. `Unit` maps to `i1`
/// (an unused placeholder bit) purely so a `()`-pattern function
/// parameter (see `lower.rs`'s `__unit_paramN` synthetic name) still
/// has SOME LLVM type to declare — no Plum expression produces a
/// meaningful `Unit` VALUE. `Heap` is an opaque `ptr` at the LLVM
/// level — codegen never needs to know WHICH specific struct/enum a
/// given pointer is at compile time beyond "it's heap-shaped," since
/// both `Match` dispatch and the runtime's own recursive-release logic
/// read the cell's TAG at runtime rather than tracking it statically
/// per value — see `codegen.rs`'s module doc comment for the full heap
/// design.
/// `Str`/`Array` are top-level, PARALLEL variants — not `Heap`
/// sub-cases — because unlike a struct/enum (one static `Heap` pointer
/// type covering many different RUNTIME tags, needing the side tables
/// `tag_ids`/`tag_fields` to disambiguate), a string or array's
/// `CgType` is precise and complete on its own: zero runtime tag
/// dispatch is ever needed to know how to store/load/refcount one. See
/// `codegen.rs`'s module doc comment for the string/array heap-cell
/// layouts this distinction drives. `Array` is recursive
/// (`Array[Array[Int]]` etc.) — `Box` only exists to give the variant a
/// finite size, same reason any other recursive Rust enum needs one.
/// `Closure` is ALSO `ptr` at the LLVM level (see `llvm_type` below),
/// but — unlike `Heap`/`Str`/`Array` — deliberately carries its OWN
/// param/return `CgType`s rather than being opaque: a closure is
/// CALLED through (an indirect `call` needs to know the exact argument/
/// return LLVM types to annotate the call with), where `Heap` never
/// needs to be. It deliberately does NOT carry its capture layout
/// (which fields it closed over, and their types) — that's per-
/// LITERAL-SITE information, not part of the closure's static TYPE (two
/// different `if` branches can produce two closure literals with
/// completely different captures that still both flow into the same
/// `CgType::Closure(params, ret)`-typed value at a control-flow join) —
/// see `codegen.rs`'s module doc comment for how release is instead
/// resolved via a function pointer stored IN the heap cell itself.
/// `Task` (added for `spawn`/`.join()`) is ALSO `ptr` at the LLVM
/// level, but — unlike every other variant here — is DELIBERATELY
/// NEVER refcounted: `plum_ir::fbip`'s `is_syntactically_heap` never
/// treats a `spawn`-bound name as heap-tracked (a `spawn` block's
/// result crosses via a fresh deep copy, never a shared pointer two
/// threads could race an `Inc`/`Dec` on — see DESIGN.md's Concurrency
/// section), so `dec_fn_for` below returns `None` for it, matching
/// FBIP's own assumption exactly. Carries the block's own result
/// `CgType` (like `Closure` carries its param/return types) purely so
/// `.join()`'s codegen knows how to unbox the single-word result it
/// gets back from the joined thread — the task CELL itself stays
/// completely opaque about it at the LLVM level (a plain 16-byte
/// `{ i64 joined, i64 pthread_id }` block, see `codegen.rs`'s
/// `codegen_spawn_literal`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgType {
    Int,
    Float,
    Bool,
    Unit,
    Heap,
    Str,
    Array(Box<CgType>),
    Closure(Vec<CgType>, Box<CgType>),
    Task(Box<CgType>),
    /// The sending end of a `channel[T]()` — `ptr` at the LLVM level,
    /// like `Task`, but for a THIRD, distinct reason (see this module's
    /// own doc comment intro): a `Sender`/`Receiver` legitimately
    /// crosses a thread boundary (unlike `Closure`/`Task`), but ONLY as
    /// a verbatim pointer copy to the ONE shared, permanently-leaked
    /// mutex-guarded queue struct `channel[T]()` allocates — never
    /// refcounted (there's no refcount word anywhere in that struct's
    /// layout; see `dec_fn_for`'s own doc comment for why treating it
    /// as one would corrupt the mutex sitting at offset 0) and never
    /// deep-copied (`deepcopy_fn_for` — a deep copy would silently
    /// split one channel into two mutually-invisible halves). Carries
    /// its own element `CgType` purely so `.recv()`/`select` know how
    /// to convert the raw word popped off the queue back into a real
    /// value (mirroring why `Task` carries its own result `CgType`).
    Sender(Box<CgType>),
    /// The receiving end of a `channel[T]()` — see `Sender`'s own doc
    /// comment; kept as a DISTINCT variant (not one `Channel` type with
    /// a "which end" flag) so `.send()`/`.recv()` on the wrong end is
    /// an ordinary compile-time type mismatch, mirroring why `Sender`
    /// and `Receiver` are separate variants rather than one shared one.
    Receiver(Box<CgType>),
}

impl CgType {
    fn llvm_type(&self) -> &'static str {
        match self {
            CgType::Int => "i64",
            CgType::Float => "double",
            CgType::Bool | CgType::Unit => "i1",
            CgType::Heap
            | CgType::Str
            | CgType::Array(_)
            | CgType::Closure(..)
            | CgType::Task(_)
            | CgType::Sender(_)
            | CgType::Receiver(_) => "ptr",
        }
    }

    /// A filesystem/LLVM-identifier-safe name for this type, used to
    /// mangle the per-distinct-element-type array release function name
    /// (`@plum_rc_dec_array_<mangled>`) — see `emit_array_release_fns`'s
    /// doc comment for why one such function exists per element `CgType`
    /// that actually appears in the compiled program, rather than one
    /// generic dispatcher. Recurses for a nested `Array(Array(Int))` ->
    /// `Array_Array_Int`.
    fn mangled(&self) -> String {
        match self {
            CgType::Int => "Int".to_string(),
            CgType::Float => "Float".to_string(),
            CgType::Bool => "Bool".to_string(),
            CgType::Unit => "Unit".to_string(),
            CgType::Heap => "Heap".to_string(),
            CgType::Str => "Str".to_string(),
            CgType::Array(elem) => format!("Array_{}", elem.mangled()),
            // Every closure shape shares ONE mangled name regardless of
            // its actual param/return types — the only consumer of
            // `mangled()` is array-element-release-function naming
            // (`emit_array_release_fns`), and `@plum_rc_dec_closure`
            // (the dec function EVERY closure shares — see `dec_fn_for`)
            // doesn't care about param/return types either, so
            // collapsing every `Closure(..)` to one array-release
            // function is correct, not just convenient: decrementing an
            // `Array[Closure]` element never needs to know what that
            // closure's own call signature was.
            CgType::Closure(..) => "Closure".to_string(),
            // Never actually reached by any LIVE deep-copy/release call
            // (a `Task` can never cross a `spawn` boundary at all — see
            // `CgType::Task`'s own doc comment — so an
            // `Array[Task[_]]` element's mangled name is only ever
            // needed to keep `emit_array_release_fns`/`emit_deepcopy_
            // array_fns` TOTAL over whatever `needed_arrays` happens to
            // contain program-wide, never because a real call site
            // targets it), but still needs a well-formed, distinct name
            // per inner type so two different `Array[Task[T]]` shapes
            // don't collide.
            CgType::Task(inner) => format!("Task_{}", inner.mangled()),
            // Never actually reached by any LIVE deep-copy/release call
            // either — a `Sender`/`Receiver` is never deep-copied (see
            // `deepcopy_fn_for`) and never refcounted (see `dec_fn_for`)
            // — but still needs a well-formed, distinct name per inner
            // type for the same `emit_array_release_fns`/`emit_deepcopy_
            // array_fns` totality reason `Task`'s own arm documents.
            CgType::Sender(inner) => format!("Sender_{}", inner.mangled()),
            CgType::Receiver(inner) => format!("Receiver_{}", inner.mangled()),
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
#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    pub params: Vec<CgType>,
    pub ret: CgType,
}

/// Every distinct tag (a struct name, or an enum variant name) known
/// in the program, mapped to its fields' `CgType`s in DECLARED order —
/// the caller (`plumc`) derives this from `plum_types::TypeContext::
/// struct_fields`/`variant`, restricted to non-generic types only (see
/// DESIGN.md: monomorphization is separate, later work — a generic
/// type's field types can't be resolved to one concrete LLVM
/// representation from the erased IR alone). `emit_program` uses this
/// both to size/lay out `Ctor` allocations and to intern each tag to a
/// small integer for runtime dispatch (`Match`, `@plum_release_fields`
/// — see `codegen.rs`'s module doc comment).
pub type TagFields = HashMap<String, Vec<CgType>>;

/// The runtime decrement function to call for a value of `ty`, or
/// `None` for a scalar `ty` that carries no refcount at all — shared by
/// `plum_release_fields` (struct/enum field release), `codegen.rs`'s
/// `RcAnnotated` dispatch, and the array element-release functions
/// (`emit_array_release_fns`) themselves, so all three ALWAYS agree on
/// which function decrements which shape.
fn dec_fn_for(ty: &CgType) -> Option<String> {
    match ty {
        CgType::Int | CgType::Float | CgType::Bool | CgType::Unit => None,
        CgType::Heap => Some("@plum_rc_dec".to_string()),
        CgType::Str => Some("@plum_rc_dec_str".to_string()),
        CgType::Array(elem) => Some(format!("@plum_rc_dec_array_{}", elem.mangled())),
        // ONE shared function for every closure shape — see `@plum_rc_
        // dec_closure`'s own doc comment in `emit_runtime` for why a
        // single, runtime-dispatched-via-stored-function-pointer
        // function works here where every OTHER heap kind needs a
        // shape-specific (or at least element-type-specific) one.
        CgType::Closure(..) => Some("@plum_rc_dec_closure".to_string()),
        // Deliberately `None`, matching `is_syntactically_heap` in
        // `plum_ir::fbip` — see `CgType::Task`'s own doc comment. If
        // this ever returned `Some(..)`, `RcAnnotated`'s `Inc`/`Dec`
        // codegen (`codegen.rs`) would corrupt a task cell's `joined`
        // word (byte offset 0) by treating it as a refcount — this
        // `None` is exactly what keeps that word forever untouched by
        // anything except `.join()`'s own explicit check.
        CgType::Task(_) => None,
        // Deliberately `None`, for a THIRD reason distinct from both
        // `Task`'s and every scalar's: there's no refcount word ANYWHERE
        // in the shared channel-queue struct's layout at all (`{ [40 x
        // i8] mutex, [48 x i8] cond, ptr head, ptr tail }`, see
        // `codegen.rs`'s channel-runtime doc comment) — treating a
        // `Sender`/`Receiver` as refcounted would corrupt the mutex
        // sitting at byte offset 0, not just silently no-op the way
        // `Task`'s `None` does.
        CgType::Sender(_) | CgType::Receiver(_) => None,
    }
}

fn intern_tags(tag_fields: &TagFields) -> HashMap<String, i64> {
    // Order doesn't matter for correctness (any bijection to distinct
    // integers works) — sorted purely so the same program always gets
    // the same IDs across runs, which makes generated `.ll` output
    // (and any test asserting on it) reproducible.
    let mut names: Vec<&String> = tag_fields.keys().collect();
    names.sort();
    names.into_iter().enumerate().map(|(i, name)| (name.clone(), i as i64)).collect()
}

/// Recursively registers `ty` (and, if `ty` is itself `Array(Array(_))`,
/// every NESTED array element type too) into the set of array element
/// `CgType`s that need their own `@plum_rc_dec_array_<mangled>` release
/// function emitted — see `emit_array_release_fns`'s doc comment for
/// why one such function exists per distinct element type rather than
/// one generic runtime dispatcher. A no-op for any non-`Array` `ty`
/// (there's nothing to register). Keyed by `CgType::mangled()` so the
/// same element type discovered from two different sources (a struct
/// field AND a function signature, say) only ever gets ONE definition.
fn register_array_elem_type(needed: &mut HashMap<String, CgType>, ty: &CgType) {
    if let CgType::Array(elem) = ty {
        needed.entry(elem.mangled()).or_insert_with(|| (**elem).clone());
        register_array_elem_type(needed, elem);
    }
}

/// The small, fixed set of runtime functions every compiled program
/// needs for HEAP-shaped values (structs/enums, refcounted via
/// `@plum_alloc`/`@plum_rc_inc`/`@plum_rc_dec`/`@plum_release_fields`),
/// PLUS the string runtime (`@plum_alloc_str`/`@plum_rc_dec_str`/
/// `@plum_str_concat`/`@plum_str_contains`/`starts_with`/`ends_with`),
/// array allocation (`@plum_alloc_array`), and the shared runtime-
/// checked-failure helper (`@plum_abort`) — see `codegen.rs`'s module
/// doc comment for the heap-cell layouts these operate on. Per-distinct-
/// element-type array RELEASE functions are a separate table
/// (`emit_array_release_fns`), since — unlike everything else here —
/// there isn't a FIXED number of them: one gets emitted per element
/// `CgType` that actually appears in the compiled program, discovered
/// during codegen itself (see `Ctx::needed_arrays`). Emitted as TEXT
/// directly into the program's own `.ll` output (no separate hand-
/// written runtime file at all, matching this whole backend's "no LLVM
/// binding, emit text" style and the project's self-hosting-viability
/// policy).
fn emit_runtime(tag_fields: &TagFields, tag_ids: &HashMap<String, i64>) -> String {
    let mut out = String::new();
    out.push_str("declare ptr @malloc(i64)\n");
    out.push_str("declare void @free(ptr)\n");
    out.push_str("declare ptr @memcpy(ptr, ptr, i64)\n");
    // Only `ArrayRemoveReuse`'s in-place element shift needs `memmove`
    // (overlap-safe) rather than `memcpy` — see `codegen.rs`'s own doc
    // comment on that call site for why.
    out.push_str("declare ptr @memmove(ptr, ptr, i64)\n");
    out.push_str("declare ptr @realloc(ptr, i64)\n");
    out.push_str("declare void @exit(i32)\n");
    out.push_str("declare i32 @printf(ptr, ...)\n");
    out.push_str("declare i32 @snprintf(ptr, i64, ptr, ...)\n\n");

    // Shared by every runtime-checked failure (bounds/emptiness checks
    // — see `codegen.rs`'s `emit_bounds_check` — there's no compile-
    // time-provable-exhaustive case like `Match`'s `unreachable` for
    // these, since the actual index/emptiness is only known at
    // runtime): prints `%msg` (expected to be a NUL-terminated, `i8*`-
    // compatible C string constant — see `codegen.rs`'s call sites for
    // how those are built) then exits non-zero, matching this whole
    // project's "clear, described failure, never a silent wrong
    // answer" spirit at the Rust level (`Result<_, String>`) carried
    // through to compiled code.
    out.push_str(
        "define void @plum_abort(ptr %msg) {\n\
         entry:\n\
         \x20 call i32 (ptr, ...) @printf(ptr %msg)\n\
         \x20 call void @exit(i32 1)\n\
         \x20 unreachable\n\
         }\n\n",
    );

    out.push_str(
        "define ptr @plum_alloc(i64 %tag, i64 %num_fields) {\n\
         entry:\n\
         \x20 %fields_bytes = mul i64 %num_fields, 8\n\
         \x20 %size = add i64 %fields_bytes, 16\n\
         \x20 %p = call ptr @malloc(i64 %size)\n\
         \x20 store i64 1, ptr %p\n\
         \x20 %tag_addr = getelementptr i8, ptr %p, i64 8\n\
         \x20 store i64 %tag, ptr %tag_addr\n\
         \x20 ret ptr %p\n\
         }\n\n",
    );

    out.push_str(
        "define void @plum_rc_inc(ptr %p) {\n\
         entry:\n\
         \x20 %rc = load i64, ptr %p\n\
         \x20 %rc2 = add i64 %rc, 1\n\
         \x20 store i64 %rc2, ptr %p\n\
         \x20 ret void\n\
         }\n\n",
    );

    out.push_str(
        "define void @plum_rc_dec(ptr %p) {\n\
         entry:\n\
         \x20 %rc = load i64, ptr %p\n\
         \x20 %rc2 = sub i64 %rc, 1\n\
         \x20 store i64 %rc2, ptr %p\n\
         \x20 %is_zero = icmp eq i64 %rc2, 0\n\
         \x20 br i1 %is_zero, label %free_block, label %done\n\
         free_block:\n\
         \x20 call void @plum_release_fields(ptr %p)\n\
         \x20 call void @free(ptr %p)\n\
         \x20 br label %done\n\
         done:\n\
         \x20 ret void\n\
         }\n\n",
    );

    // Recursively decs every HEAP-shaped field of `%p`, dispatching on
    // its RUNTIME tag — a plain sequential icmp+br chain (matching
    // `Match`'s own dispatch style, see codegen.rs's module doc
    // comment), not an LLVM `switch`. A tag with no heap-shaped fields
    // gets an empty (immediately-`br`-to-done) block.
    out.push_str("define void @plum_release_fields(ptr %p) {\nentry:\n  %tag_addr = getelementptr i8, ptr %p, i64 8\n  %tag = load i64, ptr %tag_addr\n  br label %check0\n");
    let mut names: Vec<&String> = tag_fields.keys().collect();
    names.sort();
    for (i, name) in names.iter().enumerate() {
        let id = tag_ids[*name];
        let field_types = &tag_fields[*name];
        let check_label = format!("check{i}");
        let body_label = format!("release{i}");
        let next_label = if i + 1 < names.len() { format!("check{}", i + 1) } else { "done".to_string() };
        out.push_str(&format!(
            "{check_label}:\n  %m{i} = icmp eq i64 %tag, {id}\n  br i1 %m{i}, label %{body_label}, label %{next_label}\n"
        ));
        out.push_str(&format!("{body_label}:\n"));
        for (field_idx, field_ty) in field_types.iter().enumerate() {
            // Scalar fields (`Int`/`Float`/`Bool`/`Unit`) carry no
            // refcount at all — `dec_fn_for` returns `None` for exactly
            // those, same "nothing to release" skip as before this
            // chunk (which only ever had `Heap` to consider). `Str`/
            // `Array` fields are NEWLY reachable here: `plum_type_to_
            // cg_type` (plumc) only started resolving `Str`/`Array[T]`
            // field types to something other than an error as of this
            // chunk — see that function's own doc comment.
            let Some(dec_fn) = dec_fn_for(field_ty) else {
                continue;
            };
            let offset = 16 + field_idx as i64 * 8;
            out.push_str(&format!(
                "  %f{i}_{field_idx}_addr = getelementptr i8, ptr %p, i64 {offset}\n  \
                 %f{i}_{field_idx}_word = load i64, ptr %f{i}_{field_idx}_addr\n  \
                 %f{i}_{field_idx}_ptr = inttoptr i64 %f{i}_{field_idx}_word to ptr\n  \
                 call void {dec_fn}(ptr %f{i}_{field_idx}_ptr)\n"
            ));
        }
        out.push_str(&format!("  br label %done\n"));
    }
    if names.is_empty() {
        out.push_str("check0:\n  br label %done\n");
    }
    out.push_str("done:\n  ret void\n}\n\n");

    // --- string runtime ---
    //
    // Cell layout: `{ i64 refcount, i64 len, i8 bytes[len], i8 '\0' }`
    // — ONE extra trailing NUL byte beyond `len`, kept in sync by every
    // function below that produces a string cell's bytes (`@plum_alloc_
    // str` itself, `@plum_str_concat`, and `codegen.rs`'s
    // `StrConcatReuse` reuse branch). This is an IMPLEMENTATION detail
    // only — `.len()` always reports the true byte length, never
    // `len+1` — added purely so `plumc::emit_main`'s own test-
    // observability `printf("%s", ...)` path is safe (Plum strings
    // aren't NUL-terminated on their own, since byte `\0` is a
    // perfectly ordinary Plum string byte). Strings have no nested
    // heap-shaped fields, so — unlike `plum_rc_dec` — `plum_rc_dec_str`
    // needs no `plum_release_fields`-style recursive step at all: at
    // refcount zero it just frees directly.
    out.push_str(
        "define ptr @plum_alloc_str(i64 %len) {\n\
         entry:\n\
         \x20 %bytes_and_nul = add i64 %len, 1\n\
         \x20 %size = add i64 %bytes_and_nul, 16\n\
         \x20 %p = call ptr @malloc(i64 %size)\n\
         \x20 store i64 1, ptr %p\n\
         \x20 %len_addr = getelementptr i8, ptr %p, i64 8\n\
         \x20 store i64 %len, ptr %len_addr\n\
         \x20 %nul_addr = getelementptr i8, ptr %p, i64 16\n\
         \x20 %nul_addr2 = getelementptr i8, ptr %nul_addr, i64 %len\n\
         \x20 store i8 0, ptr %nul_addr2\n\
         \x20 ret ptr %p\n\
         }\n\n",
    );

    out.push_str(
        "define void @plum_rc_dec_str(ptr %p) {\n\
         entry:\n\
         \x20 %rc = load i64, ptr %p\n\
         \x20 %rc2 = sub i64 %rc, 1\n\
         \x20 store i64 %rc2, ptr %p\n\
         \x20 %is_zero = icmp eq i64 %rc2, 0\n\
         \x20 br i1 %is_zero, label %free_block, label %done\n\
         free_block:\n\
         \x20 call void @free(ptr %p)\n\
         \x20 br label %done\n\
         done:\n\
         \x20 ret void\n\
         }\n\n",
    );

    // The FRESH-allocation half of `.concat()` — see `codegen.rs`'s
    // `StrConcatReuse` case for the reuse-in-place half (inlined there,
    // not a runtime function, same "CtorReuse does the reuse-vs-fresh
    // branching inline" precedent).
    out.push_str(
        "define ptr @plum_str_concat(ptr %a, ptr %b) {\n\
         entry:\n\
         \x20 %alen_addr = getelementptr i8, ptr %a, i64 8\n\
         \x20 %alen = load i64, ptr %alen_addr\n\
         \x20 %blen_addr = getelementptr i8, ptr %b, i64 8\n\
         \x20 %blen = load i64, ptr %blen_addr\n\
         \x20 %newlen = add i64 %alen, %blen\n\
         \x20 %cell = call ptr @plum_alloc_str(i64 %newlen)\n\
         \x20 %dst0 = getelementptr i8, ptr %cell, i64 16\n\
         \x20 %asrc = getelementptr i8, ptr %a, i64 16\n\
         \x20 call ptr @memcpy(ptr %dst0, ptr %asrc, i64 %alen)\n\
         \x20 %dst1 = getelementptr i8, ptr %dst0, i64 %alen\n\
         \x20 %bsrc = getelementptr i8, ptr %b, i64 16\n\
         \x20 call ptr @memcpy(ptr %dst1, ptr %bsrc, i64 %blen)\n\
         \x20 ret ptr %cell\n\
         }\n\n",
    );

    // `@plum_str_starts_with(s, prefix)` — a plain byte-compare loop,
    // no `strstr` dependency (Plum strings aren't NUL-terminated at
    // their FRONT — see the cell-layout comment above; the trailing NUL
    // is a `printf`-safety add-on, not something these comparisons rely
    // on).
    out.push_str(
        "define i1 @plum_str_starts_with(ptr %s, ptr %prefix) {\n\
         entry:\n\
         \x20 %slen_addr = getelementptr i8, ptr %s, i64 8\n\
         \x20 %slen = load i64, ptr %slen_addr\n\
         \x20 %plen_addr = getelementptr i8, ptr %prefix, i64 8\n\
         \x20 %plen = load i64, ptr %plen_addr\n\
         \x20 %long_enough = icmp sge i64 %slen, %plen\n\
         \x20 br i1 %long_enough, label %loop, label %too_short\n\
         too_short:\n\
         \x20 ret i1 0\n\
         loop:\n\
         \x20 %i = phi i64 [ 0, %entry ], [ %i_next, %loop_cont ]\n\
         \x20 %done = icmp eq i64 %i, %plen\n\
         \x20 br i1 %done, label %matched, label %check\n\
         check:\n\
         \x20 %s_base = getelementptr i8, ptr %s, i64 16\n\
         \x20 %s_addr = getelementptr i8, ptr %s_base, i64 %i\n\
         \x20 %s_byte = load i8, ptr %s_addr\n\
         \x20 %p_base = getelementptr i8, ptr %prefix, i64 16\n\
         \x20 %p_addr = getelementptr i8, ptr %p_base, i64 %i\n\
         \x20 %p_byte = load i8, ptr %p_addr\n\
         \x20 %eq = icmp eq i8 %s_byte, %p_byte\n\
         \x20 br i1 %eq, label %loop_cont, label %not_matched\n\
         loop_cont:\n\
         \x20 %i_next = add i64 %i, 1\n\
         \x20 br label %loop\n\
         matched:\n\
         \x20 ret i1 1\n\
         not_matched:\n\
         \x20 ret i1 0\n\
         }\n\n",
    );

    out.push_str(
        "define i1 @plum_str_ends_with(ptr %s, ptr %suffix) {\n\
         entry:\n\
         \x20 %slen_addr = getelementptr i8, ptr %s, i64 8\n\
         \x20 %slen = load i64, ptr %slen_addr\n\
         \x20 %suflen_addr = getelementptr i8, ptr %suffix, i64 8\n\
         \x20 %suflen = load i64, ptr %suflen_addr\n\
         \x20 %long_enough = icmp sge i64 %slen, %suflen\n\
         \x20 br i1 %long_enough, label %init, label %too_short\n\
         too_short:\n\
         \x20 ret i1 0\n\
         init:\n\
         \x20 %start = sub i64 %slen, %suflen\n\
         \x20 br label %loop\n\
         loop:\n\
         \x20 %i = phi i64 [ 0, %init ], [ %i_next, %loop_cont ]\n\
         \x20 %done = icmp eq i64 %i, %suflen\n\
         \x20 br i1 %done, label %matched, label %check\n\
         check:\n\
         \x20 %s_idx = add i64 %start, %i\n\
         \x20 %s_base = getelementptr i8, ptr %s, i64 16\n\
         \x20 %s_addr = getelementptr i8, ptr %s_base, i64 %s_idx\n\
         \x20 %s_byte = load i8, ptr %s_addr\n\
         \x20 %suf_base = getelementptr i8, ptr %suffix, i64 16\n\
         \x20 %suf_addr = getelementptr i8, ptr %suf_base, i64 %i\n\
         \x20 %suf_byte = load i8, ptr %suf_addr\n\
         \x20 %eq = icmp eq i8 %s_byte, %suf_byte\n\
         \x20 br i1 %eq, label %loop_cont, label %not_matched\n\
         loop_cont:\n\
         \x20 %i_next = add i64 %i, 1\n\
         \x20 br label %loop\n\
         matched:\n\
         \x20 ret i1 1\n\
         not_matched:\n\
         \x20 ret i1 0\n\
         }\n\n",
    );

    // `@plum_str_contains` — a naive O(n*m) double loop (no need for
    // anything smarter at this scope: strings aren't expected to be
    // huge, and there's no existing precedent anywhere else in this
    // backend for a non-naive algorithm either).
    out.push_str(
        "define i1 @plum_str_contains(ptr %s, ptr %needle) {\n\
         entry:\n\
         \x20 %slen_addr = getelementptr i8, ptr %s, i64 8\n\
         \x20 %slen = load i64, ptr %slen_addr\n\
         \x20 %nlen_addr = getelementptr i8, ptr %needle, i64 8\n\
         \x20 %nlen = load i64, ptr %nlen_addr\n\
         \x20 %fits = icmp sge i64 %slen, %nlen\n\
         \x20 br i1 %fits, label %outer_init, label %not_found\n\
         outer_init:\n\
         \x20 %max_start = sub i64 %slen, %nlen\n\
         \x20 br label %outer\n\
         outer:\n\
         \x20 %start = phi i64 [ 0, %outer_init ], [ %start_next, %outer_cont ]\n\
         \x20 %outer_done = icmp sgt i64 %start, %max_start\n\
         \x20 br i1 %outer_done, label %not_found, label %inner_init\n\
         inner_init:\n\
         \x20 br label %inner\n\
         inner:\n\
         \x20 %j = phi i64 [ 0, %inner_init ], [ %j_next, %inner_cont ]\n\
         \x20 %inner_done = icmp eq i64 %j, %nlen\n\
         \x20 br i1 %inner_done, label %found, label %inner_check\n\
         inner_check:\n\
         \x20 %s_idx = add i64 %start, %j\n\
         \x20 %s_base = getelementptr i8, ptr %s, i64 16\n\
         \x20 %s_addr = getelementptr i8, ptr %s_base, i64 %s_idx\n\
         \x20 %s_byte = load i8, ptr %s_addr\n\
         \x20 %n_base = getelementptr i8, ptr %needle, i64 16\n\
         \x20 %n_addr = getelementptr i8, ptr %n_base, i64 %j\n\
         \x20 %n_byte = load i8, ptr %n_addr\n\
         \x20 %eq = icmp eq i8 %s_byte, %n_byte\n\
         \x20 br i1 %eq, label %inner_cont, label %outer_cont\n\
         inner_cont:\n\
         \x20 %j_next = add i64 %j, 1\n\
         \x20 br label %inner\n\
         outer_cont:\n\
         \x20 %start_next = add i64 %start, 1\n\
         \x20 br label %outer\n\
         found:\n\
         \x20 ret i1 1\n\
         not_found:\n\
         \x20 ret i1 0\n\
         }\n\n",
    );

    // --- array runtime ---
    //
    // Cell layout: `{ i64 refcount, i64 len, <elemTy word> elements[len] }`
    // — structurally identical to a `Ctor` cell (refcount + one more
    // `i64` + N words), so `field_byte_offset`/`store_field_word`/
    // `load_field_word` (codegen.rs) are reused UNCHANGED for arrays,
    // just called with a runtime-variable index instead of a
    // statically-bounded one. Kept as its own dedicated allocator
    // (rather than reusing `@plum_alloc`, which takes a TAG) purely for
    // readability of the generated IR — it avoids "tag" terminology
    // leaking into array-context error messages/variable names, even
    // though the two functions' bodies are otherwise identical in
    // shape.
    out.push_str(
        "define ptr @plum_alloc_array(i64 %len) {\n\
         entry:\n\
         \x20 %elems_bytes = mul i64 %len, 8\n\
         \x20 %size = add i64 %elems_bytes, 16\n\
         \x20 %p = call ptr @malloc(i64 %size)\n\
         \x20 store i64 1, ptr %p\n\
         \x20 %len_addr = getelementptr i8, ptr %p, i64 8\n\
         \x20 store i64 %len, ptr %len_addr\n\
         \x20 ret ptr %p\n\
         }\n\n",
    );

    // --- closure runtime ---
    //
    // Cell layout: `{ i64 refcount, i64 code_ptr, i64 release_fn_ptr,
    // i64 captured[N] }` — a 3-WORD header, one word wider than every
    // other heap cell in this backend (`Ctor`/array cells are 2 words:
    // refcount + one more). This extra width is deliberate, not
    // incidental: a closure's release logic can't be resolved purely
    // from its static `CgType` the way an array's element-type-keyed
    // release can, because two DIFFERENT closure literals (different
    // capture layouts, e.g. from two branches of an `if`) can both flow
    // into the same `CgType::Closure(params, ret)`-typed value at a
    // control-flow join — so which fields to release has to be resolved
    // via a function pointer stored IN the cell itself, at a FIXED
    // offset every closure cell shares, rather than a name derivable
    // from the type alone. `codegen.rs`'s `closure_field_byte_offset`
    // (24 + index*8) is the captured-field counterpart to `field_byte_
    // offset` (16 + index*8) for exactly this reason. Kept as its own
    // dedicated allocator (rather than reusing `@plum_alloc`, which
    // takes a tag, or `@plum_alloc_array`, whose 2-word header is too
    // narrow) purely for readability, same precedent as `@plum_alloc_
    // array` itself.
    out.push_str(
        "define ptr @plum_alloc_closure(i64 %num_captured) {\n\
         entry:\n\
         \x20 %captured_bytes = mul i64 %num_captured, 8\n\
         \x20 %size = add i64 %captured_bytes, 24\n\
         \x20 %p = call ptr @malloc(i64 %size)\n\
         \x20 store i64 1, ptr %p\n\
         \x20 ret ptr %p\n\
         }\n\n",
    );

    // ONE shared dec function for EVERY closure shape — mirroring
    // `Heap`'s own `@plum_rc_dec`, which dispatches via a runtime TAG
    // rather than a compile-time-distinct function per struct, this
    // dispatches via a function POINTER stored in the cell (word 2,
    // byte offset 16) instead: at refcount zero, call it indirectly
    // (its job is ONLY to dec whatever captured fields are heap-shaped,
    // matching `@plum_release_fields`'s own "release fields, don't free
    // the cell itself" contract), then free the cell. `plum-codegen`
    // generates one such release function per closure LITERAL SITE
    // (`@closure_release$<fn>$<K>`, see `codegen.rs`), each with a
    // signature of `void(ptr)` so this indirect call is always legal
    // regardless of which literal site's cell actually flows through
    // here at runtime.
    out.push_str(
        "define void @plum_rc_dec_closure(ptr %p) {\n\
         entry:\n\
         \x20 %rc = load i64, ptr %p\n\
         \x20 %rc2 = sub i64 %rc, 1\n\
         \x20 store i64 %rc2, ptr %p\n\
         \x20 %is_zero = icmp eq i64 %rc2, 0\n\
         \x20 br i1 %is_zero, label %free_block, label %done\n\
         free_block:\n\
         \x20 %release_addr = getelementptr i8, ptr %p, i64 16\n\
         \x20 %release_word = load i64, ptr %release_addr\n\
         \x20 %release_fn = inttoptr i64 %release_word to ptr\n\
         \x20 call void %release_fn(ptr %p)\n\
         \x20 call void @free(ptr %p)\n\
         \x20 br label %done\n\
         done:\n\
         \x20 ret void\n\
         }\n\n",
    );

    // A shared, trivial "nothing captured, nothing to release" release
    // function — used for every ZERO-capture closure cell (a bare
    // top-level function reference wrapped as a value via a trampoline
    // — see `codegen.rs`'s `codegen_bare_fn_value` — always has zero
    // captures by construction), so codegen doesn't need to generate a
    // separate, identically-empty release function per trampoline.
    out.push_str(
        "define void @plum_closure_release_noop(ptr %cell) {\n\
         entry:\n\
         \x20 ret void\n\
         }\n\n",
    );

    out
}

/// One `@plum_rc_dec_array_<mangled>` release function per DISTINCT
/// array element `CgType` that actually appears anywhere in the
/// compiled program (`Ctx::needed_arrays`, seeded up front from every
/// struct/enum field and function signature — see `emit_program` — and
/// grown further as `codegen.rs` discovers more array literals/ops
/// during each function body's own codegen). Since an array's element
/// type is always statically known at every codegen site that touches
/// that array (see `plum-codegen`'s module doc comment's "how array
/// element CgType is determined" story), this needs NO runtime tag
/// dispatch at all — a genuine simplification over `plum_release_
/// fields`'s per-struct-tag `icmp` chain: each element type gets one
/// small, direct function, decrementing (and, at zero, freeing) an
/// array cell of exactly that shape.
fn emit_array_release_fns(needed: &HashMap<String, CgType>) -> String {
    let mut out = String::new();
    // Sorted purely for reproducible `.ll` output across runs, same
    // reasoning as `intern_tags`.
    let mut names: Vec<&String> = needed.keys().collect();
    names.sort();
    for mangled in names {
        let elem = &needed[mangled];
        let name = format!("plum_rc_dec_array_{mangled}");
        out.push_str(&format!("define void @{name}(ptr %p) {{\n"));
        out.push_str("entry:\n");
        out.push_str("  %rc = load i64, ptr %p\n");
        out.push_str("  %rc2 = sub i64 %rc, 1\n");
        out.push_str("  store i64 %rc2, ptr %p\n");
        out.push_str("  %is_zero = icmp eq i64 %rc2, 0\n");
        out.push_str("  br i1 %is_zero, label %free_block, label %done\n");
        out.push_str("free_block:\n");
        match dec_fn_for(elem) {
            // A heap-shaped element (`Heap`/`Str`/`Array(_)`) needs
            // every element decremented FIRST, before the array cell
            // itself is freed — an ordinary counted loop, same
            // "sequential icmp+br chain, not a fancy construct"
            // convention as everything else in this backend.
            Some(elem_dec_fn) => {
                out.push_str("  %len_addr = getelementptr i8, ptr %p, i64 8\n");
                out.push_str("  %len = load i64, ptr %len_addr\n");
                out.push_str("  br label %loop_check\n");
                out.push_str("loop_check:\n");
                out.push_str("  %i = phi i64 [ 0, %free_block ], [ %i_next, %loop_body ]\n");
                out.push_str("  %continue = icmp slt i64 %i, %len\n");
                out.push_str("  br i1 %continue, label %loop_body, label %after_loop\n");
                out.push_str("loop_body:\n");
                out.push_str("  %word_off = mul i64 %i, 8\n");
                out.push_str("  %byte_off = add i64 %word_off, 16\n");
                out.push_str("  %elem_addr = getelementptr i8, ptr %p, i64 %byte_off\n");
                out.push_str("  %elem_word = load i64, ptr %elem_addr\n");
                out.push_str("  %elem_ptr = inttoptr i64 %elem_word to ptr\n");
                out.push_str(&format!("  call void {elem_dec_fn}(ptr %elem_ptr)\n"));
                out.push_str("  %i_next = add i64 %i, 1\n");
                out.push_str("  br label %loop_check\n");
                out.push_str("after_loop:\n");
                out.push_str("  call void @free(ptr %p)\n");
                out.push_str("  br label %done\n");
            }
            // A scalar element (`Int`/`Float`/`Bool`/`Unit`) carries no
            // refcount at all — just free the cell directly.
            None => {
                out.push_str("  call void @free(ptr %p)\n");
                out.push_str("  br label %done\n");
            }
        }
        out.push_str("done:\n  ret void\n}\n\n");
    }
    out
}

/// The deep-copy counterpart to `dec_fn_for` — which runtime function
/// (if any) recursively snapshots a value of `ty` into a FRESH cell,
/// rather than decrementing an existing one. `None` means "just copy
/// the raw word as-is" (correct for a scalar, where the word already
/// IS the whole value — no separate cell to copy at all). `Closure`/
/// `Task` map to `None` too, but for a completely different reason:
/// they can NEVER actually be captured across a `spawn` boundary in a
/// well-typed program (rejected at the capture site by `codegen.rs`'s
/// `crosses_spawn_boundary`, and — for a nested struct/enum field —
/// by `check_no_closure_or_task_fields` below, run before this
/// function's callers ever emit a single deep-copy call). This arm
/// only exists so `deepcopy_fn_for` stays TOTAL over `CgType` at all
/// (`needed_arrays`, which `emit_deepcopy_array_fns` iterates, is a
/// whole-PROGRAM set — it can perfectly well contain an `Array
/// [Closure]` entry for a reason that has nothing to do with `spawn`
/// at all, e.g. an ordinary higher-order-function signature elsewhere
/// in the same program) — the resulting "deep-copy" function for such
/// an element type is never actually CALLED by any real capture site,
/// so its (incorrect, shallow) behavior is permanently dead code, not
/// a live correctness gap.
fn deepcopy_fn_for(ty: &CgType) -> Option<String> {
    match ty {
        CgType::Int | CgType::Float | CgType::Bool | CgType::Unit => None,
        CgType::Heap => Some("@plum_deepcopy_heap".to_string()),
        CgType::Str => Some("@plum_deepcopy_str".to_string()),
        CgType::Array(elem) => Some(format!("@plum_deepcopy_array_{}", elem.mangled())),
        CgType::Closure(..) | CgType::Task(_) => None,
        // A `Sender`/`Receiver` crosses a thread boundary as a VERBATIM
        // pointer copy, never a deep copy — see `codegen.rs`'s
        // `deep_copy_capture` `Sender`/`Receiver` arm for the exact
        // mechanism. `None` here is exactly what makes `@plum_deepcopy_
        // heap`'s generic per-field fallback ("no copy function for
        // this field type — just copy the raw word") correct for a
        // NESTED `Sender`/`Receiver` struct field too: copying the
        // shared queue pointer verbatim is exactly right, unlike
        // `Closure`/`Task` (where the same fallback would be silently
        // WRONG — see `check_no_closure_or_task_fields`'s own doc
        // comment for why only those two need a separate whole-program
        // check).
        CgType::Sender(_) | CgType::Receiver(_) => None,
    }
}

/// `pthread_create`/`pthread_join`'s C declarations — emitted ONLY when
/// `Ctx::needs_spawn_runtime` (`codegen.rs`) observed at least one real
/// `Spawn`/`TaskJoin` node during body codegen (see `emit_program`'s own
/// call site). `pthread_t` is confirmed a plain 8-byte scalar
/// (`unsigned long`) on this platform (verified directly via `sizeof
/// (pthread_t)`, not assumed) — this backend already ties itself to the
/// local platform/ABI by shelling out to `clang` (see DESIGN.md's
/// "Implementation plan"), same precedent as every other libc-behavior
/// dependency accepted elsewhere, so declaring it as a plain `i64`
/// rather than an opaque struct is safe here.
fn emit_spawn_pthread_decls() -> String {
    "declare i32 @pthread_create(ptr, ptr, ptr, ptr)\ndeclare i32 @pthread_join(i64, ptr)\n\n".to_string()
}

/// The deep-copy runtime BOTH `spawn` (a captured free variable) and
/// channels (`.send()`'s value — see `codegen.rs`'s `codegen_channel_
/// send`) need to snapshot a value into a form the OTHER thread can
/// safely own outright — emitted whenever EITHER `Ctx::needs_spawn_
/// runtime` OR `Ctx::needs_channel_runtime` observed a real crossing
/// node during body codegen (see `emit_program`'s own call site): a
/// channel used with no `spawn` anywhere still needs this (e.g. a
/// heap-shaped value sent on a channel and received back on the SAME
/// thread still deep-copies on the way in, matching `.send()`'s own
/// doc comment), and a `spawn` capture needs it independent of whether
/// the program ever touches a channel. Split out from the combined
/// `pthread_create`/`pthread_join` declarations above specifically so
/// a channel-only program (no `spawn` at all) still gets this without
/// also pulling in `pthread_create`/`pthread_join`, which it never
/// calls. `@plum_deepcopy_heap` mirrors `@plum_release_fields`'s exact
/// runtime-tag-dispatch shape — same `icmp`-chain over the SAME
/// `tag_ids`, just allocating a fresh cell and recursively deep-copying
/// each heap-shaped field (via `deepcopy_fn_for`) instead of
/// decrementing it. `@plum_deepcopy_str` allocates fresh + `memcpy`s
/// bytes — strings have no nested heap fields, so (unlike `@plum_
/// deepcopy_heap`) this needs no recursion at all, mirroring `@plum_rc_
/// dec_str`'s own equally-simple shape.
fn emit_deepcopy_runtime(tag_fields: &TagFields, tag_ids: &HashMap<String, i64>) -> String {
    let mut out = String::new();

    out.push_str(
        "define ptr @plum_deepcopy_str(ptr %p) {\n\
         entry:\n\
         \x20 %len_addr = getelementptr i8, ptr %p, i64 8\n\
         \x20 %len = load i64, ptr %len_addr\n\
         \x20 %new = call ptr @plum_alloc_str(i64 %len)\n\
         \x20 %dst = getelementptr i8, ptr %new, i64 16\n\
         \x20 %src = getelementptr i8, ptr %p, i64 16\n\
         \x20 call ptr @memcpy(ptr %dst, ptr %src, i64 %len)\n\
         \x20 ret ptr %new\n\
         }\n\n",
    );

    // Same sequential `icmp`+`br` tag-dispatch chain as `plum_release_
    // fields` (see that function's own doc comment for why: `Match`-
    // style, not an LLVM `switch`) — but each matched block ALLOCATES a
    // fresh cell (`@plum_alloc`, same allocator every OTHER `Ctor`
    // construction in this backend uses) and copies/recursively-deep-
    // copies each field into it, rather than decrementing. The `done`
    // block `phi`-merges whichever tag's block actually ran; if
    // `tag_fields` is empty (no struct/enum declared at all — this
    // function would then never actually be CALLED, since there'd be
    // no `Heap`-typed value in the whole program to capture), `done` is
    // genuinely unreachable, so it's `unreachable` rather than a bogus
    // `ret` with nothing to return.
    out.push_str("define ptr @plum_deepcopy_heap(ptr %p) {\nentry:\n  %tag_addr = getelementptr i8, ptr %p, i64 8\n  %tag = load i64, ptr %tag_addr\n  br label %check0\n");
    let mut names: Vec<&String> = tag_fields.keys().collect();
    names.sort();
    let mut phi_parts = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let id = tag_ids[*name];
        let field_types = &tag_fields[*name];
        let check_label = format!("check{i}");
        let body_label = format!("copy{i}");
        // Unlike `plum_release_fields` (a `void` function, where the
        // last check's "no match" edge can harmlessly fall straight
        // into `done`), `done` HERE needs an actual `ptr` value on
        // EVERY incoming edge for its `phi` to be well-formed — so the
        // last check's failure edge goes to a dedicated `no_match`
        // block instead (`unreachable`, since a well-typed program's
        // runtime tag always matches ONE of these known tags), not
        // `done` directly.
        let next_label = if i + 1 < names.len() { format!("check{}", i + 1) } else { "no_match".to_string() };
        out.push_str(&format!(
            "{check_label}:\n  %m{i} = icmp eq i64 %tag, {id}\n  br i1 %m{i}, label %{body_label}, label %{next_label}\n"
        ));
        out.push_str(&format!("{body_label}:\n  %new{i} = call ptr @plum_alloc(i64 {id}, i64 {})\n", field_types.len()));
        for (field_idx, field_ty) in field_types.iter().enumerate() {
            let offset = 16 + field_idx as i64 * 8;
            out.push_str(&format!(
                "  %f{i}_{field_idx}_addr = getelementptr i8, ptr %p, i64 {offset}\n  \
                 %f{i}_{field_idx}_word = load i64, ptr %f{i}_{field_idx}_addr\n"
            ));
            let new_word = match deepcopy_fn_for(field_ty) {
                None => format!("%f{i}_{field_idx}_word"),
                Some(copy_fn) => {
                    out.push_str(&format!(
                        "  %f{i}_{field_idx}_ptr = inttoptr i64 %f{i}_{field_idx}_word to ptr\n  \
                         %f{i}_{field_idx}_copy = call ptr {copy_fn}(ptr %f{i}_{field_idx}_ptr)\n  \
                         %f{i}_{field_idx}_copyword = ptrtoint ptr %f{i}_{field_idx}_copy to i64\n"
                    ));
                    format!("%f{i}_{field_idx}_copyword")
                }
            };
            let new_addr = format!("%new{i}_{field_idx}_addr");
            out.push_str(&format!(
                "  {new_addr} = getelementptr i8, ptr %new{i}, i64 {offset}\n  \
                 store i64 {new_word}, ptr {new_addr}\n"
            ));
        }
        out.push_str(&format!("  br label %done\n"));
        phi_parts.push(format!("[ %new{i}, %{body_label} ]"));
    }
    if names.is_empty() {
        out.push_str("check0:\n  unreachable\ndone:\n  unreachable\n}\n\n");
    } else {
        out.push_str("no_match:\n  unreachable\n");
        out.push_str(&format!("done:\n  %result = phi ptr {}\n  ret ptr %result\n}}\n\n", phi_parts.join(", ")));
    }

    out
}

/// `pthread_mutex_*`/`pthread_cond_*`/`usleep`'s C declarations, plus
/// the small, fixed channel-queue runtime every `channel[T]()` needs —
/// emitted ONLY when `Ctx::needs_channel_runtime` (`codegen.rs`)
/// observed a real `Channel`/`ChannelSend`/`ChannelRecv`/`Select` node
/// during body codegen. Deliberately does NOT declare `pthread_mutex_
/// destroy`/`pthread_cond_destroy`/`pthread_cond_broadcast` — dead code
/// given this backend's permanent-leak decision (a channel's queue
/// struct is never destroyed, and nothing ever needs to wake more than
/// one waiter at a time — `send` always enqueues exactly one node, so
/// `pthread_cond_signal` alone is always correct).
///
/// Queue struct layout (one per `channel[T]()`, `malloc`'d and
/// permanently leaked — matching this backend's own established
/// "leak over unsoundness" precedent, e.g. `Task`'s own captures):
/// `{ [40 x i8] mutex, [48 x i8] cond, ptr head, ptr tail }` — 104
/// bytes total (mutex at offset 0, cond at offset 40, `head` at offset
/// 88, `tail` at offset 96); `pthread_mutex_t`/`pthread_cond_t`
/// confirmed fixed-size opaque buffers on this platform (same
/// "verified directly, not assumed" precedent as `pthread_t`'s own
/// plain-`i64` treatment — see `emit_spawn_pthread_decls`'s doc
/// comment). Queue NODE layout (one per enqueued value, `malloc`'d by
/// `send`, `free`'d by whichever `recv`/`try_recv` call pops it):
/// `{ i64 value_word, ptr next }` — 16 bytes, using the SAME uniform
/// word representation every other single-word "box" in this backend
/// already uses (`codegen.rs`'s `value_to_word`/`word_to_value`).
///
/// `@plum_channel_send`/`@plum_channel_recv`/`@plum_channel_try_recv`
/// are small, hand-rolled runtime FUNCTIONS (not inlined at every call
/// site) purely to keep the mutex/cond dance — and its correctness —
/// written exactly ONCE, matching this whole file's "small, fixed
/// runtime, called from codegen.rs" convention every other heap
/// operation already follows (`@plum_alloc`/`@plum_rc_inc`/...).
///
/// # The central correctness property: no lost update under concurrent senders
///
/// EVERY read or write of `head`/`tail`/a node's `next` pointer happens
/// strictly between this same struct's own `pthread_mutex_lock`/
/// `pthread_mutex_unlock` pair, in EVERY one of `@plum_channel_send`/
/// `@plum_channel_recv`/`@plum_channel_try_recv` — the mutex is the
/// SAME one embedded in the queue struct itself (`%q`, byte offset 0),
/// so any two concurrent callers (regardless of which of these three
/// functions, or how many distinct OS threads) strictly serialize
/// their access to those three pointers: whichever thread's
/// `pthread_mutex_lock` returns first performs its ENTIRE append/pop
/// (including the read-modify-write of `tail`) before any other
/// caller's lock call can return. `@plum_channel_send`'s own node
/// `malloc`/`store i64 %word, ptr %node` happen BEFORE the lock, safe
/// because each caller mallocs its OWN independent node — nothing
/// shared is touched until the lock is held. A lost update to `tail`
/// (the classic multi-producer race this queue shape is vulnerable to
/// if unsynchronized) is therefore structurally impossible: there is
/// no window where two threads can both read `tail`'s old value and
/// both write a new one without the mutex serializing between them.
/// `pthread_cond_wait` is called ONLY while the mutex is already held
/// (`@plum_channel_recv`'s `wait` block) — POSIX guarantees it
/// atomically unlocks-and-waits, then re-locks before returning, so
/// this never races against `send`'s own critical section either.
fn emit_channel_runtime() -> String {
    let mut out = String::new();
    out.push_str("declare i32 @pthread_mutex_init(ptr, ptr)\n");
    out.push_str("declare i32 @pthread_mutex_lock(ptr)\n");
    out.push_str("declare i32 @pthread_mutex_unlock(ptr)\n");
    out.push_str("declare i32 @pthread_cond_init(ptr, ptr)\n");
    out.push_str("declare i32 @pthread_cond_wait(ptr, ptr)\n");
    out.push_str("declare i32 @pthread_cond_signal(ptr)\n");
    out.push_str("declare i32 @usleep(i32)\n\n");

    out.push_str(
        "define ptr @plum_channel_new() {\n\
         entry:\n\
         \x20 %q = call ptr @malloc(i64 104)\n\
         \x20 %mutex_init_r = call i32 @pthread_mutex_init(ptr %q, ptr null)\n\
         \x20 %cond = getelementptr i8, ptr %q, i64 40\n\
         \x20 %cond_init_r = call i32 @pthread_cond_init(ptr %cond, ptr null)\n\
         \x20 %head_addr = getelementptr i8, ptr %q, i64 88\n\
         \x20 store ptr null, ptr %head_addr\n\
         \x20 %tail_addr = getelementptr i8, ptr %q, i64 96\n\
         \x20 store ptr null, ptr %tail_addr\n\
         \x20 ret ptr %q\n\
         }\n\n",
    );

    out.push_str(
        "define void @plum_channel_send(ptr %q, i64 %word) {\n\
         entry:\n\
         \x20 %node = call ptr @malloc(i64 16)\n\
         \x20 store i64 %word, ptr %node\n\
         \x20 %node_next_addr = getelementptr i8, ptr %node, i64 8\n\
         \x20 store ptr null, ptr %node_next_addr\n\
         \x20 %lock_r = call i32 @pthread_mutex_lock(ptr %q)\n\
         \x20 %tail_addr = getelementptr i8, ptr %q, i64 96\n\
         \x20 %tail = load ptr, ptr %tail_addr\n\
         \x20 %tail_is_null = icmp eq ptr %tail, null\n\
         \x20 br i1 %tail_is_null, label %empty, label %nonempty\n\
         empty:\n\
         \x20 %head_addr = getelementptr i8, ptr %q, i64 88\n\
         \x20 store ptr %node, ptr %head_addr\n\
         \x20 store ptr %node, ptr %tail_addr\n\
         \x20 br label %signal\n\
         nonempty:\n\
         \x20 %tail_next_addr = getelementptr i8, ptr %tail, i64 8\n\
         \x20 store ptr %node, ptr %tail_next_addr\n\
         \x20 store ptr %node, ptr %tail_addr\n\
         \x20 br label %signal\n\
         signal:\n\
         \x20 %cond = getelementptr i8, ptr %q, i64 40\n\
         \x20 %signal_r = call i32 @pthread_cond_signal(ptr %cond)\n\
         \x20 %unlock_r = call i32 @pthread_mutex_unlock(ptr %q)\n\
         \x20 ret void\n\
         }\n\n",
    );

    // A REAL blocking wait (`pthread_cond_wait`), not a busy-poll —
    // unlike `select` (which has no single-primitive way to block on
    // MULTIPLE channels at once with plain pthread primitives), a
    // single channel's `recv` has a real condvar to wait on, so it
    // uses it. No second deep-copy on the way out: once a node is off
    // the queue (popped under the mutex, in `pop` below), only THIS
    // call ever touches its payload word again — see this function's
    // own doc comment for the full happens-before argument.
    out.push_str(
        "define i64 @plum_channel_recv(ptr %q) {\n\
         entry:\n\
         \x20 %lock_r = call i32 @pthread_mutex_lock(ptr %q)\n\
         \x20 br label %wait_check\n\
         wait_check:\n\
         \x20 %head_addr = getelementptr i8, ptr %q, i64 88\n\
         \x20 %head = load ptr, ptr %head_addr\n\
         \x20 %is_null = icmp eq ptr %head, null\n\
         \x20 br i1 %is_null, label %wait, label %pop\n\
         wait:\n\
         \x20 %cond = getelementptr i8, ptr %q, i64 40\n\
         \x20 %wait_r = call i32 @pthread_cond_wait(ptr %cond, ptr %q)\n\
         \x20 br label %wait_check\n\
         pop:\n\
         \x20 %word = load i64, ptr %head\n\
         \x20 %next_addr = getelementptr i8, ptr %head, i64 8\n\
         \x20 %next = load ptr, ptr %next_addr\n\
         \x20 store ptr %next, ptr %head_addr\n\
         \x20 %next_is_null = icmp eq ptr %next, null\n\
         \x20 br i1 %next_is_null, label %clear_tail, label %skip_clear\n\
         clear_tail:\n\
         \x20 %tail_addr = getelementptr i8, ptr %q, i64 96\n\
         \x20 store ptr null, ptr %tail_addr\n\
         \x20 br label %skip_clear\n\
         skip_clear:\n\
         \x20 %unlock_r = call i32 @pthread_mutex_unlock(ptr %q)\n\
         \x20 call void @free(ptr %head)\n\
         \x20 ret i64 %word\n\
         }\n\n",
    );

    // The non-blocking counterpart `select` polls with — a plain `try_
    // lock`-free lock/check/unlock (this backend has no `pthread_mutex_
    // trylock` precedent to reach for, and an ordinary blocking lock
    // held only briefly is simplest and still correct: `select`'s OWN
    // blocking behavior comes from its outer `usleep`-and-retry loop in
    // codegen.rs, not from this function ever blocking). Returns `i1`
    // (`1` = a value was popped into `%out`, `0` = the queue was empty
    // at the moment of the check) rather than any richer result —
    // there's no `Disconnected` case (see this module's own "documented
    // behavioral gap" note in codegen.rs's `codegen_select`).
    out.push_str(
        "define i1 @plum_channel_try_recv(ptr %q, ptr %out) {\n\
         entry:\n\
         \x20 %lock_r = call i32 @pthread_mutex_lock(ptr %q)\n\
         \x20 %head_addr = getelementptr i8, ptr %q, i64 88\n\
         \x20 %head = load ptr, ptr %head_addr\n\
         \x20 %is_null = icmp eq ptr %head, null\n\
         \x20 br i1 %is_null, label %empty, label %pop\n\
         empty:\n\
         \x20 %unlock_empty_r = call i32 @pthread_mutex_unlock(ptr %q)\n\
         \x20 ret i1 0\n\
         pop:\n\
         \x20 %word = load i64, ptr %head\n\
         \x20 %next_addr = getelementptr i8, ptr %head, i64 8\n\
         \x20 %next = load ptr, ptr %next_addr\n\
         \x20 store ptr %next, ptr %head_addr\n\
         \x20 %next_is_null = icmp eq ptr %next, null\n\
         \x20 br i1 %next_is_null, label %clear_tail, label %skip_clear\n\
         clear_tail:\n\
         \x20 %tail_addr = getelementptr i8, ptr %q, i64 96\n\
         \x20 store ptr null, ptr %tail_addr\n\
         \x20 br label %skip_clear\n\
         skip_clear:\n\
         \x20 %unlock_r = call i32 @pthread_mutex_unlock(ptr %q)\n\
         \x20 store i64 %word, ptr %out\n\
         \x20 call void @free(ptr %head)\n\
         \x20 ret i1 1\n\
         }\n\n",
    );

    out
}

/// One `@plum_deepcopy_array_<mangled>` deep-copy function per DISTINCT
/// array element `CgType` in `needed` — the deep-copy counterpart to
/// `emit_array_release_fns`, reusing the SAME `needed_arrays` discovery
/// set (see that function's own doc comment for why one function per
/// element type, not a single runtime-dispatched one, is both possible
/// and simplest here). A scalar element type needs only a flat
/// `memcpy` of the whole element region (correct: a scalar's "deep
/// copy" IS just copying its bytes, no separate cell to recurse into —
/// same reasoning `deepcopy_fn_for`'s `None` case documents). A heap-
/// shaped element type needs a counted loop deep-copying each element
/// individually via the appropriate `@plum_deepcopy_*` function, mirror-
/// ing `emit_array_release_fns`'s own release loop exactly, just
/// allocating+copying instead of decrementing.
fn emit_deepcopy_array_fns(needed: &HashMap<String, CgType>) -> String {
    let mut out = String::new();
    let mut names: Vec<&String> = needed.keys().collect();
    names.sort();
    for mangled in names {
        let elem = &needed[mangled];
        let name = format!("plum_deepcopy_array_{mangled}");
        out.push_str(&format!("define ptr @{name}(ptr %p) {{\n"));
        out.push_str("entry:\n");
        out.push_str("  %len_addr = getelementptr i8, ptr %p, i64 8\n");
        out.push_str("  %len = load i64, ptr %len_addr\n");
        out.push_str("  %new = call ptr @plum_alloc_array(i64 %len)\n");
        match deepcopy_fn_for(elem) {
            Some(elem_copy_fn) => {
                out.push_str("  br label %loop_check\n");
                out.push_str("loop_check:\n");
                out.push_str("  %i = phi i64 [ 0, %entry ], [ %i_next, %loop_body ]\n");
                out.push_str("  %continue = icmp slt i64 %i, %len\n");
                out.push_str("  br i1 %continue, label %loop_body, label %after_loop\n");
                out.push_str("loop_body:\n");
                out.push_str("  %word_off = mul i64 %i, 8\n");
                out.push_str("  %byte_off = add i64 %word_off, 16\n");
                out.push_str("  %elem_addr = getelementptr i8, ptr %p, i64 %byte_off\n");
                out.push_str("  %elem_word = load i64, ptr %elem_addr\n");
                out.push_str("  %elem_ptr = inttoptr i64 %elem_word to ptr\n");
                out.push_str(&format!("  %elem_copy = call ptr {elem_copy_fn}(ptr %elem_ptr)\n"));
                out.push_str("  %elem_copyword = ptrtoint ptr %elem_copy to i64\n");
                out.push_str("  %new_elem_addr = getelementptr i8, ptr %new, i64 %byte_off\n");
                out.push_str("  store i64 %elem_copyword, ptr %new_elem_addr\n");
                out.push_str("  %i_next = add i64 %i, 1\n");
                out.push_str("  br label %loop_check\n");
                out.push_str("after_loop:\n");
                out.push_str("  ret ptr %new\n");
            }
            None => {
                out.push_str("  %bytes = mul i64 %len, 8\n");
                out.push_str("  %src = getelementptr i8, ptr %p, i64 16\n");
                out.push_str("  %dst = getelementptr i8, ptr %new, i64 16\n");
                out.push_str("  call ptr @memcpy(ptr %dst, ptr %src, i64 %bytes)\n");
                out.push_str("  ret ptr %new\n");
            }
        }
        out.push_str("}\n\n");
    }
    out
}

/// The conservative, whole-PROGRAM structural check backing `spawn`'s
/// deep-copy correctness: if the program uses `spawn` ANYWHERE, no
/// declared struct/enum may have a closure- or task-typed field
/// ANYWHERE, even in one never actually captured by any `spawn` site.
/// Needed because `Heap` is opaque at a `spawn` capture site (`codegen.
/// rs`'s `crosses_spawn_boundary` can reject a DIRECTLY closure/task-
/// typed capture, but has no way to see three levels into an opaque
/// `Heap` pointer to notice a closure hiding in one of its fields) —
/// this is deliberately simple and structural over exactly precise
/// (matching this backend's established precedent, e.g. `needed_arrays`
/// registering more than strictly necessary), cheap (one linear pass
/// over `tag_fields`), and correct in the safe direction: a program that
/// never actually spawns anything is completely unaffected, no matter
/// what its struct/enum fields look like.
fn check_no_closure_or_task_fields(tag_fields: &TagFields) -> Result<(), String> {
    let mut names: Vec<&String> = tag_fields.keys().collect();
    names.sort();
    for tag in names {
        for (i, field_ty) in tag_fields[tag].iter().enumerate() {
            if matches!(field_ty, CgType::Closure(..) | CgType::Task(_)) {
                return Err(format!(
                    "codegen: struct/enum {tag:?}'s field {i} is closure/task-shaped ({field_ty:?}) — a \
                     program that uses `spawn` anywhere cannot declare a struct/enum with a closure- or \
                     task-typed field anywhere else either, since such a value could reach a `spawn` \
                     capture through an opaque heap pointer and neither a closure's captured environment \
                     nor a task handle can cross a thread boundary (matching the interpreter's own \
                     restriction — see `plum_interp::Interpreter::to_portable`)"
                ));
            }
        }
    }
    Ok(())
}

/// Emits an entire program as LLVM IR TEXT (the `.ll` format) — no
/// LLVM Rust binding involved at all (see DESIGN.md's "Implementation
/// plan" section for why: this machine has no `llvm-config`/dev
/// headers installed, and text + shelling to `clang` is also more
/// self-hosting-friendly than binding to a version-specific LLVM C
/// API). `signatures` must contain an entry for every function
/// `program.functions` calls (including itself, for recursion) — see
/// `FnSig`'s doc comment. `tag_fields` must contain an entry for every
/// tag any `Ctor`/`CtorReuse`/`Match` in the program constructs or
/// deconstructs — see `TagFields`'s doc comment.
///
/// Supported scope (see DESIGN.md): scalars (`Int`/`Float`/`Bool`/
/// `Unit`), `Var`, `Unary`, `Binary` (including short-circuit `&&`/
/// `||`), `Let`, `If`, plain named `Call` (with any tail call — self-
/// or mutual-recursive — compiled to `musttail call` + `ret`, LLVM's
/// portable guaranteed-tail-call-elimination mechanism), and
/// non-generic-struct/enum heap values (`Ctor`/`CtorReuse`/
/// `RcAnnotated`/`Match`, refcounted via four small runtime functions
/// emitted alongside the program itself — see `emit_runtime`).
/// `program.globals`/`program.externs` and every other `ir::Expr`
/// variant (strings, arrays, closures, concurrency, FFI, generics,
/// ...) are out of scope for now and produce a clear error naming
/// what's missing, never a panic — see this crate's tests for the
/// exact error shapes.
pub fn emit_program(program: &ir::Program, signatures: &HashMap<String, FnSig>, tag_fields: &TagFields) -> Result<String, String> {
    if !program.globals.is_empty() {
        return Err("codegen does not yet support top-level globals (v1 scope is functions only)".to_string());
    }
    if !program.externs.is_empty() {
        return Err("codegen does not yet support extern \"C\" functions (v1 scope has no FFI)".to_string());
    }
    let tag_ids = intern_tags(tag_fields);

    // Every array element `CgType` that needs its own `@plum_rc_dec_
    // array_<mangled>` release function — seeded up front from every
    // struct/enum field (a struct COULD hold an `Array[T]` field even
    // if no function body ever directly constructs one of that exact
    // shape) and every function signature (same reasoning: a caller
    // might only ever DECREMENT an array whose element type it never
    // itself constructs), then grown further as each function body's
    // own codegen discovers more array literals/ops — see `Ctx::
    // needed_arrays` and `register_array_elem_type`. A `RefCell`
    // because `Ctx` is threaded through `codegen_expr`/`codegen_value`
    // as a shared `&Ctx` (matching every other field there), but
    // discovery genuinely needs to MUTATE this one set as codegen
    // walks each function body — the same "shared, mutably-discovered
    // side table behind a `&Ctx`" shape as nowhere else in this
    // backend yet, but the least invasive way to thread it without
    // making every codegen function take an extra `&mut` parameter
    // just for this.
    let needed_arrays = std::cell::RefCell::new(HashMap::new());
    for field_types in tag_fields.values() {
        for ty in field_types {
            register_array_elem_type(&mut needed_arrays.borrow_mut(), ty);
        }
    }
    for sig in signatures.values() {
        for p in &sig.params {
            register_array_elem_type(&mut needed_arrays.borrow_mut(), p);
        }
        register_array_elem_type(&mut needed_arrays.borrow_mut(), &sig.ret);
    }

    // Function bodies are emitted BEFORE the runtime preamble text is
    // assembled (even though the preamble appears FIRST in the final
    // output) specifically so `needed_arrays` is fully populated by the
    // time `emit_array_release_fns` runs — LLVM IR doesn't require
    // forward declaration order between top-level `define`s in the same
    // module, so this reordering is purely a Rust-side "collect
    // everything, then emit" convenience, invisible in the output.
    // Shared, program-wide state for closure-literal-site codegen — see
    // `codegen::Ctx::closure_counter`/`closure_defs`/`trampolines`'
    // own doc comments. Same `RefCell`-behind-a-shared-reference shape
    // as `needed_arrays`, for the same reason: genuinely mutated as
    // codegen walks each function body, but threaded through as a
    // plain `&Ctx` everywhere else.
    let closure_counter = std::cell::RefCell::new(0usize);
    let closure_defs = std::cell::RefCell::new(Vec::new());
    let trampolines = std::cell::RefCell::new(HashMap::new());
    // Set to `true` the first time codegen actually walks a `Spawn`/
    // `TaskJoin` node (`codegen.rs`'s `codegen_spawn_literal`/`codegen_
    // task_join`) — read AFTER every function body has been emitted
    // (same "collect everything while emitting bodies, then finalize"
    // convention `needed_arrays` itself already established) to decide
    // whether the spawn runtime (`emit_spawn_runtime`/`emit_deepcopy_
    // array_fns`) and the whole-program `check_no_closure_or_task_
    // fields` check are needed at all — a program that never spawns
    // anything pays zero cost (no extra declarations, no rejected
    // struct/enum shapes it never even touches through `spawn`).
    let needs_spawn_runtime = std::cell::RefCell::new(false);
    // Set to `true` the first time codegen actually walks a `Channel`/
    // `ChannelSend`/`ChannelRecv`/`Select` node (`codegen.rs`'s
    // `codegen_channel_literal`/`codegen_channel_send`/`codegen_channel_
    // recv`/`codegen_select`) — same "collect while emitting bodies,
    // finalize after" shape as `needs_spawn_runtime`, gating the channel
    // runtime (`emit_channel_runtime`) independently: a program that
    // uses channels but never `spawn` still needs the channel-queue
    // runtime (and the shared deep-copy runtime — see `emit_deepcopy_
    // runtime`'s own doc comment) but NOT `pthread_create`/`pthread_
    // join`, and vice versa.
    let needs_channel_runtime = std::cell::RefCell::new(false);

    let mut bodies = String::new();
    for f in &program.functions {
        bodies.push_str(&emit_function(
            f,
            signatures,
            &tag_ids,
            tag_fields,
            &needed_arrays,
            &closure_counter,
            &closure_defs,
            &trampolines,
            &needs_spawn_runtime,
            &needs_channel_runtime,
        )?);
        bodies.push('\n');
    }

    // The whole-program closure/task-field rejection fires whenever
    // EITHER spawn OR channels are used — a channel send can smuggle a
    // closure/task three levels deep into an opaque `Heap` pointer
    // exactly as easily as a spawn capture can (see `check_no_closure_
    // or_task_fields`'s own doc comment).
    if *needs_spawn_runtime.borrow() || *needs_channel_runtime.borrow() {
        check_no_closure_or_task_fields(tag_fields)?;
    }

    let needed_arrays = needed_arrays.into_inner();
    let mut out = emit_runtime(tag_fields, &tag_ids);
    out.push_str(&emit_array_release_fns(&needed_arrays));
    if *needs_spawn_runtime.borrow() || *needs_channel_runtime.borrow() {
        out.push_str(&emit_deepcopy_runtime(tag_fields, &tag_ids));
        out.push_str(&emit_deepcopy_array_fns(&needed_arrays));
    }
    if *needs_spawn_runtime.borrow() {
        out.push_str(&emit_spawn_pthread_decls());
    }
    if *needs_channel_runtime.borrow() {
        out.push_str(&emit_channel_runtime());
    }
    // Every closure-literal-site-generated function/release function/
    // trampoline/spawn-entry-function, discovered while walking
    // `program.functions` above — spliced in here, same "collect
    // everything while emitting bodies, then place it before the
    // bodies in the final text" convention `emit_array_release_fns`
    // itself already established.
    for def in closure_defs.into_inner() {
        out.push_str(&def);
        out.push('\n');
    }
    out.push_str(&bodies);
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn emit_function(
    f: &ir::Function,
    signatures: &HashMap<String, FnSig>,
    tag_ids: &HashMap<String, i64>,
    tag_fields: &TagFields,
    needed_arrays: &std::cell::RefCell<HashMap<String, CgType>>,
    closure_counter: &std::cell::RefCell<usize>,
    closure_defs: &std::cell::RefCell<Vec<String>>,
    trampolines: &std::cell::RefCell<HashMap<String, String>>,
    needs_spawn_runtime: &std::cell::RefCell<bool>,
    needs_channel_runtime: &std::cell::RefCell<bool>,
) -> Result<String, String> {
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
    let ctx = codegen::Ctx {
        sigs: signatures,
        caller_sig: &sig,
        tag_ids,
        tag_fields,
        fn_name: &f.name,
        needed_arrays,
        closure_counter,
        closure_defs,
        trampolines,
        needs_spawn_runtime,
        needs_channel_runtime,
    };

    let mut env = HashMap::new();
    let mut param_decls = Vec::with_capacity(f.params.len());
    for (name, ty) in f.params.iter().zip(&sig.params) {
        env.insert(name.clone(), (format!("%{name}"), ty.clone()));
        param_decls.push(format!("{} %{name}", ty.llvm_type()));
    }

    let mut em = codegen::Emitter::new();
    let (result, _) = codegen::codegen_expr(&f.body, &env, &mut em, &ctx, true)?;
    if result.is_some() {
        return Err(format!(
            "internal codegen error: function {:?}'s body did not terminate with a `ret` in tail position",
            f.name
        ));
    }

    // Module-level string-literal globals this function's body needed
    // (see `codegen::Emitter::fresh_string_global`) must appear OUTSIDE
    // the `define { ... }` block — emitted just before it here, rather
    // than interleaved into `em.lines` (which only ever holds
    // FUNCTION-BODY instructions).
    let mut out = String::new();
    for g in &em.string_globals {
        out.push_str(g);
        out.push('\n');
    }
    out.push_str(&format!("define {} @{}({}) {{\n", sig.ret.llvm_type(), f.name, param_decls.join(", ")));
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
    use plum_ir::ir::{BinOp, Expr, Function, MatchArm, PrimTy, Program, RcOp, SelectArm, UnOp};

    fn sigs(entries: &[(&str, Vec<CgType>, CgType)]) -> HashMap<String, FnSig> {
        entries
            .iter()
            .map(|(name, params, ret)| (name.to_string(), FnSig { params: params.clone(), ret: ret.clone() }))
            .collect()
    }

    fn tags(entries: &[(&str, Vec<CgType>)]) -> TagFields {
        entries.iter().map(|(name, fields)| (name.to_string(), fields.clone())).collect()
    }

    fn program(functions: Vec<Function>) -> Program {
        Program { functions, globals: vec![], externs: vec![] }
    }

    fn emit(prog: &Program, s: &HashMap<String, FnSig>, t: &TagFields) -> Result<String, String> {
        emit_program(prog, s, t)
    }

    #[test]
    fn emits_a_define_with_correct_signature() {
        let prog = program(vec![Function {
            name: "double".to_string(),
            params: vec!["n".to_string()],
            body: Expr::Binary(BinOp::Mul, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(2))),
        }]);
        let ir = emit(&prog, &sigs(&[("double", vec![CgType::Int], CgType::Int)]), &TagFields::new()).unwrap();
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
        let ir = emit(&prog, &sigs(&[("sum", vec![CgType::Int, CgType::Int], CgType::Int)]), &TagFields::new()).unwrap();
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
        let ir = emit(
            &prog,
            &sigs(&[
                ("is_even", vec![CgType::Int], CgType::Bool),
                ("is_odd", vec![CgType::Int], CgType::Bool),
            ]),
            &TagFields::new(),
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
        let ir = emit(
            &prog,
            &sigs(&[("double", vec![CgType::Int], CgType::Int), ("go", vec![CgType::Int], CgType::Int)]),
            &TagFields::new(),
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
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Int], CgType::Int)]), &TagFields::new()).unwrap();
        assert!(ir.contains(" = phi i64 "), "{ir}");
    }

    #[test]
    fn short_circuit_and_uses_branching_not_a_plain_and_instruction() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: Expr::Binary(BinOp::And, Box::new(Expr::Var("a".to_string())), Box::new(Expr::Var("b".to_string()))),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Bool, CgType::Bool], CgType::Bool)]), &TagFields::new()).unwrap();
        assert!(ir.contains("br i1 %a"), "{ir}");
        assert!(ir.contains(" = phi i1 "), "{ir}");
        assert!(!ir.contains(" and i1 "), "{ir}");
    }

    #[test]
    fn unsupported_construct_is_a_clear_error_not_a_panic() {
        // `Str` literals are supported as of this chunk (see the
        // string-literal tests below) — `StrRunes` (Unicode-aware,
        // explicitly deferred per this chunk's scope) is still a clear,
        // unsupported-construct error instead.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::StrRunes { base: Box::new(Expr::Str("x".to_string())) },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Unit)]), &TagFields::new()).expect_err("expected a clear error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_global_is_rejected_with_a_clear_error() {
        let prog = Program {
            functions: vec![],
            globals: vec![plum_ir::ir::Global { name: "x".to_string(), value: Expr::Int(1) }],
            externs: vec![],
        };
        let err = emit(&prog, &HashMap::new(), &TagFields::new()).expect_err("expected globals to be rejected");
        assert!(err.contains("globals"), "unexpected error: {err}");
    }

    #[test]
    fn a_call_through_a_computed_callee_is_rejected() {
        // A computed callee IS now supported when it's `CgType::
        // Closure`-typed (see this file's closure tests) — but a
        // NON-closure-typed computed callee (an `Int`, here) is still
        // correctly rejected: there's genuinely nothing to call through.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Call { callee: Box::new(Expr::Int(0)), args: vec![] },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Unit)]), &TagFields::new())
            .expect_err("expected a computed non-closure callee to be rejected");
        assert!(err.contains("directly-named function") && err.contains("closure"), "unexpected error: {err}");
    }

    #[test]
    fn a_call_to_an_unknown_function_is_rejected() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Call { callee: Box::new(Expr::Var("nope".to_string())), args: vec![] },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Unit)]), &TagFields::new())
            .expect_err("expected an unknown callee to be rejected");
        assert!(err.contains("unknown function"), "unexpected error: {err}");
    }

    #[test]
    fn ctor_construction_calls_plum_alloc() {
        // let go () = Point { x: 1, y: 2 } -- represented directly as
        // Ctor since lowering has already turned struct literals into
        // this shape by the time codegen sees it.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Ctor { tag: "Point".to_string(), fields: vec![Expr::Int(1), Expr::Int(2)] },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("call ptr @plum_alloc(i64 0, i64 2)"), "{ir}");
        assert!(ir.contains("define ptr @plum_alloc"), "{ir}");
    }

    #[test]
    fn rc_annotated_inc_and_dec_call_the_runtime() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["p".to_string()],
            body: Expr::RcAnnotated {
                op: RcOp::Inc,
                target: "p".to_string(),
                rest: Box::new(Expr::RcAnnotated {
                    op: RcOp::Dec,
                    target: "p".to_string(),
                    rest: Box::new(Expr::Var("p".to_string())),
                }),
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("call void @plum_rc_inc(ptr %p)"), "{ir}");
        assert!(ir.contains("call void @plum_rc_dec(ptr %p)"), "{ir}");
    }

    #[test]
    fn ctor_reuse_never_calls_plum_alloc_on_the_reuse_path() {
        // The REUSE branch overwrites in place — `@plum_alloc` should
        // only be called from the FRESH-allocation fallback branch,
        // never unconditionally. We can't observe which branch actually
        // RUNS from a text-only test, but we can confirm the reuse
        // branch's own block contains no alloc call while the fresh
        // branch's does — a structural proxy for "codegen emitted the
        // right shape."
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["old".to_string()],
            body: Expr::CtorReuse {
                reuse_of: "old".to_string(),
                tag: "Cons".to_string(),
                fields: vec![Expr::Int(1)],
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Heap)]),
            &tags(&[("Cons", vec![CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("reuse0:") || ir.contains("reuse"), "{ir}");
        assert!(ir.contains("call ptr @plum_alloc(i64"), "{ir}");
        assert!(ir.contains("call void @plum_release_fields(ptr %old)"), "{ir}");
        // The reuse block itself must not contain an alloc call — check
        // the text BETWEEN the reuse label and the next label, scoped
        // to `@go`'s OWN body text (the always-emitted runtime preamble
        // now ALSO contains text matching the plain substring
        // `"call ptr @plum_alloc"` — e.g. `@plum_alloc_str`/
        // `@plum_alloc_array`'s own bodies — so searching the whole
        // `.ll` text unscoped would find one of THOSE instead).
        let go_start = ir.find("define ptr @go(").expect("expected @go's own definition");
        let go_body = &ir[go_start..];
        // Simplification: just confirm the STRUCT alloc call (`@plum_
        // alloc(i64 <tag>, ...)`, distinct from `@plum_alloc_str`/
        // `@plum_alloc_array` by the literal `(i64` immediately after
        // the name) appears strictly AFTER the reuse label's own
        // release-fields call — i.e. in a later (fresh-alloc) block,
        // not folded into the reuse path.
        let store_tag_idx = go_body.find("call void @plum_release_fields(ptr %old)").unwrap();
        let alloc_idx = go_body.find("call ptr @plum_alloc(i64").unwrap();
        assert!(alloc_idx > store_tag_idx, "{go_body}");
    }

    #[test]
    fn match_dispatches_by_tag_and_binds_fields() {
        // match p { Point(x, y) => x }
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["p".to_string()],
            body: Expr::Match {
                scrutinee: Box::new(Expr::Var("p".to_string())),
                arms: vec![MatchArm {
                    tag: "Point".to_string(),
                    bindings: vec!["x".to_string(), "y".to_string()],
                    guard: None,
                    body: Expr::Var("x".to_string()),
                }],
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Int)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("icmp eq i64"), "{ir}");
        assert!(ir.contains("unreachable"), "{ir}");
    }

    #[test]
    fn match_guard_falls_through_to_the_next_arm() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["p".to_string()],
            body: Expr::Match {
                scrutinee: Box::new(Expr::Var("p".to_string())),
                arms: vec![
                    MatchArm {
                        tag: "Point".to_string(),
                        bindings: vec!["x".to_string(), "y".to_string()],
                        guard: Some(Box::new(Expr::Binary(BinOp::Gt, Box::new(Expr::Var("x".to_string())), Box::new(Expr::Int(0))))),
                        body: Expr::Int(1),
                    },
                    MatchArm {
                        tag: "Point".to_string(),
                        bindings: vec!["x".to_string(), "y".to_string()],
                        guard: None,
                        body: Expr::Int(0),
                    },
                ],
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Int)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("arm_guard_pass"), "{ir}");
    }

    #[test]
    fn a_generic_type_is_not_representable_via_tag_fields_and_ctor_construction_fails_cleanly() {
        // The generics-exclusion boundary is enforced by `plumc`
        // (which never populates `tag_fields` for a generic type in
        // the first place) — from `plum-codegen`'s own perspective,
        // that just looks like "unknown tag," exercised here directly.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Ctor { tag: "Some".to_string(), fields: vec![Expr::Int(1)] },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Heap)]), &TagFields::new())
            .expect_err("expected an unknown tag to be rejected");
        assert!(err.contains("unknown tag"), "unexpected error: {err}");
    }

    // --- strings and arrays ---

    #[test]
    fn string_literal_allocates_via_plum_alloc_str_and_copies_a_global_constant() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Str("hi".to_string()),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(ir.contains("call ptr @plum_alloc_str(i64 2)"), "{ir}");
        assert!(ir.contains("private constant [2 x i8]"), "{ir}");
        assert!(ir.contains("call ptr @memcpy("), "{ir}");
        assert!(ir.contains("define ptr @plum_alloc_str(i64 %len)"), "{ir}");
    }

    #[test]
    fn str_concat_reuse_never_calls_plum_str_concat_on_its_reuse_branch() {
        // Same structural proxy `ctor_reuse_never_calls_plum_alloc_on_
        // the_reuse_path` uses: confirm the FRESH-alloc call
        // (`@plum_str_concat`) only appears strictly AFTER the reuse
        // branch's own realloc, i.e. in the separate fresh-alloc block,
        // never folded into the reuse path itself.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: Expr::StrConcatReuse {
                reuse_of: "a".to_string(),
                other: Box::new(Expr::Var("b".to_string())),
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Str, CgType::Str], CgType::Str)]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("call ptr @realloc("), "{ir}");
        let go_start = ir.find("define ptr @go(").unwrap();
        let go_body = &ir[go_start..];
        let realloc_idx = go_body.find("call ptr @realloc(").unwrap();
        let concat_idx = go_body.find("call ptr @plum_str_concat(").expect("expected a fresh-alloc fallback call");
        assert!(concat_idx > realloc_idx, "{go_body}");
    }

    #[test]
    fn array_literal_allocates_via_plum_alloc_array() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Ctor {
                tag: "0Array".to_string(),
                fields: vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)],
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Array(Box::new(CgType::Int)))]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("call ptr @plum_alloc_array(i64 3)"), "{ir}");
        assert!(ir.contains("define ptr @plum_alloc_array(i64 %len)"), "{ir}");
    }

    #[test]
    fn array_push_reuse_emits_a_realloc_never_a_fresh_array_alloc_on_its_reuse_branch() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "v".to_string()],
            body: Expr::ArrayPushReuse {
                reuse_of: "a".to_string(),
                value: Box::new(Expr::Var("v".to_string())),
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Array(Box::new(CgType::Int)), CgType::Int], CgType::Array(Box::new(CgType::Int)))]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("call ptr @realloc("), "{ir}");
        let go_start = ir.find("define ptr @go(").unwrap();
        let go_body = &ir[go_start..];
        let realloc_idx = go_body.find("call ptr @realloc(").unwrap();
        let fresh_alloc_idx = go_body
            .find("call ptr @plum_alloc_array(")
            .expect("expected a fresh-alloc fallback call");
        assert!(fresh_alloc_idx > realloc_idx, "{go_body}");
    }

    #[test]
    fn index_dispatches_differently_for_array_versus_str() {
        let array_prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "i".to_string()],
            body: Expr::Index {
                base: Box::new(Expr::Var("a".to_string())),
                index: Box::new(Expr::Var("i".to_string())),
            },
        }]);
        let array_ir = emit(
            &array_prog,
            &sigs(&[("go", vec![CgType::Array(Box::new(CgType::Int)), CgType::Int], CgType::Int)]),
            &TagFields::new(),
        )
        .unwrap();
        // Array indexing loads a full 8-byte WORD (`load i64`), never a
        // single byte — scoped to `@go`'s OWN body, since the always-
        // emitted string runtime (`@plum_str_starts_with`/etc.)
        // legitimately contains `load i8, ptr` elsewhere in the file.
        let array_go_start = array_ir.find("define i64 @go(").unwrap();
        let array_go_body = &array_ir[array_go_start..];
        assert!(array_go_body.contains("load i64, ptr"), "{array_go_body}");
        assert!(!array_go_body.contains("load i8, ptr"), "{array_go_body}");

        let str_prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string(), "i".to_string()],
            body: Expr::Index {
                base: Box::new(Expr::Var("s".to_string())),
                index: Box::new(Expr::Var("i".to_string())),
            },
        }]);
        let str_ir = emit(
            &str_prog,
            &sigs(&[("go", vec![CgType::Str, CgType::Int], CgType::Int)]),
            &TagFields::new(),
        )
        .unwrap();
        // String indexing loads a single BYTE, then zero-extends it.
        assert!(str_ir.contains("load i8, ptr"), "{str_ir}");
        assert!(str_ir.contains("zext i8"), "{str_ir}");
    }

    #[test]
    fn a_still_unsupported_string_op_is_a_clear_error() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrToUpper { base: Box::new(Expr::Var("s".to_string())) },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new())
            .expect_err("expected a clear error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_for_loop_with_a_non_int_bound_is_a_clear_error() {
        // `Expr::For` only ever carries Int `start`/`end` at the IR
        // level — real `for x in arr` surface syntax is desugared into
        // an index-based Int range loop by `lower.rs` well before
        // codegen ever sees it (see `codegen_for`'s own doc comment), so
        // an Array-typed `end` here is malformed IR, not a supported-vs-
        // unsupported surface construct — this asserts codegen still
        // reports it as a clear, specific type error rather than a
        // panic, now that `For` has real codegen support (as of this
        // chunk) instead of hitting the old catch-all "does not yet
        // support this construct" error.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string()],
            body: Expr::For {
                var: "x".to_string(),
                start: Box::new(Expr::Int(0)),
                end: Box::new(Expr::Var("a".to_string())),
                body: Box::new(Expr::Unit),
            },
        }]);
        let err = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Array(Box::new(CgType::Int))], CgType::Unit)]),
            &TagFields::new(),
        )
        .expect_err("expected a clear error");
        assert!(err.contains("must be Int"), "unexpected error: {err}");
    }

    #[test]
    fn empty_array_literal_allocates_a_zero_length_array_of_the_carried_elem_type() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::EmptyArray(PrimTy::Int),
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Array(Box::new(CgType::Int)))]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("call ptr @plum_alloc_array(i64 0)"), "{ir}");
    }

    // --- closures ---

    #[test]
    fn closure_literal_capturing_a_scalar_allocates_a_cell_and_a_separate_function() {
        // let go(n: Int): Int = { let f = |x| x + n; f(5) }
        let closure_body =
            Expr::Binary(BinOp::Add, Box::new(Expr::Var("x".to_string())), Box::new(Expr::Var("n".to_string())));
        let body = Expr::Let {
            name: "f".to_string(),
            value: Box::new(Expr::Closure {
                params: vec!["x".to_string()],
                param_types: Some(vec![PrimTy::Int]),
                ret_type: Some(PrimTy::Int),
                body: Box::new(closure_body),
            }),
            body: Box::new(Expr::Call {
                callee: Box::new(Expr::Var("f".to_string())),
                args: vec![Expr::Int(5)],
            }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["n".to_string()], body }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Int], CgType::Int)]), &TagFields::new()).unwrap();
        assert!(ir.contains("call ptr @plum_alloc_closure(i64 1)"), "{ir}");
        assert!(ir.contains("define i64 @closure$go$0("), "{ir}");
    }

    #[test]
    fn closure_capturing_a_heap_shaped_value_increments_its_refcount_on_capture() {
        // let go(p: Heap): Heap = { let f = |x| p; f(0) }
        let body = Expr::Let {
            name: "f".to_string(),
            value: Box::new(Expr::Closure {
                params: vec!["x".to_string()],
                param_types: Some(vec![PrimTy::Int]),
                ret_type: Some(PrimTy::Heap),
                body: Box::new(Expr::Var("p".to_string())),
            }),
            body: Box::new(Expr::Call {
                callee: Box::new(Expr::Var("f".to_string())),
                args: vec![Expr::Int(0)],
            }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["p".to_string()], body }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("call void @plum_rc_inc(ptr"), "{ir}");
    }

    #[test]
    fn release_function_for_a_heap_capturing_closure_decs_the_capture_and_never_frees_itself() {
        // Same program as the capture-inc test above — this asserts
        // the GENERATED release function's own shape: it decs the
        // captured field, but never `free`s anything itself (that's
        // `@plum_rc_dec_closure`'s job, once the release function it
        // calls returns — see `emit_runtime`'s own doc comment).
        let body = Expr::Let {
            name: "f".to_string(),
            value: Box::new(Expr::Closure {
                params: vec!["x".to_string()],
                param_types: Some(vec![PrimTy::Int]),
                ret_type: Some(PrimTy::Heap),
                body: Box::new(Expr::Var("p".to_string())),
            }),
            body: Box::new(Expr::Call {
                callee: Box::new(Expr::Var("f".to_string())),
                args: vec![Expr::Int(0)],
            }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["p".to_string()], body }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        let release_start = ir.find("define void @closure_release$go$0(ptr %cell) {").expect(&format!("expected a generated release fn in:\n{ir}"));
        let release_end = ir[release_start..].find("\n}\n").expect("expected the release fn to terminate") + release_start;
        let release_text = &ir[release_start..release_end];
        assert!(release_text.contains("call void @plum_rc_dec(ptr"), "{release_text}");
        assert!(!release_text.contains("@free"), "{release_text}");
        // The SHARED `@plum_rc_dec_closure` runtime function is what
        // actually frees the cell, at refcount zero, AFTER calling the
        // release function above.
        assert!(ir.contains("define void @plum_rc_dec_closure(ptr %p)"), "{ir}");
        assert!(ir.contains("call void @free(ptr %p)"), "{ir}");
    }

    #[test]
    fn calling_through_a_closure_typed_value_is_an_indirect_call_never_musttail() {
        // let apply(f: Closure([Int], Int), x: Int): Int = f(x)  -- a
        // bare-Call-in-tail-position body, so this exercises the
        // TAIL-position half of the indirect-call path specifically
        // (the one place `musttail` might otherwise have been
        // (wrongly) attempted).
        let prog = program(vec![Function {
            name: "apply".to_string(),
            params: vec!["f".to_string(), "x".to_string()],
            body: Expr::Call {
                callee: Box::new(Expr::Var("f".to_string())),
                args: vec![Expr::Var("x".to_string())],
            },
        }]);
        let closure_sig = CgType::Closure(vec![CgType::Int], Box::new(CgType::Int));
        let ir = emit(
            &prog,
            &sigs(&[("apply", vec![closure_sig, CgType::Int], CgType::Int)]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("inttoptr i64"), "{ir}");
        assert!(!ir.contains("musttail"), "{ir}");
        // A genuine `call` (through the loaded function pointer) must
        // still appear — the indirect path is a `call`, not a dropped
        // instruction.
        let apply_start = ir.find("define i64 @apply(").unwrap();
        assert!(ir[apply_start..].contains(" = call i64 %"), "{ir}");
    }

    #[test]
    fn self_referential_closure_allocates_the_cell_before_the_self_store_and_skips_its_own_inc() {
        // let go(): Int = { let fib = |n| if n < 2 { n } else { fib(n-1) + fib(n-2) }; fib(5) }
        let fib_body = Expr::If {
            cond: Box::new(Expr::Binary(BinOp::Lt, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(2)))),
            then_branch: Box::new(Expr::Var("n".to_string())),
            else_branch: Box::new(Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Call {
                    callee: Box::new(Expr::Var("fib".to_string())),
                    args: vec![Expr::Binary(BinOp::Sub, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(1)))],
                }),
                Box::new(Expr::Call {
                    callee: Box::new(Expr::Var("fib".to_string())),
                    args: vec![Expr::Binary(BinOp::Sub, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(2)))],
                }),
            )),
        };
        let body = Expr::Let {
            name: "fib".to_string(),
            value: Box::new(Expr::Closure {
                params: vec!["n".to_string()],
                param_types: Some(vec![PrimTy::Int]),
                ret_type: Some(PrimTy::Int),
                body: Box::new(fib_body),
            }),
            body: Box::new(Expr::Call {
                callee: Box::new(Expr::Var("fib".to_string())),
                args: vec![Expr::Int(5)],
            }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec![], body }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        // Exactly ONE capture: `fib` itself (self-reference) — never
        // incremented (see this function's own doc comment on the
        // deliberate, accepted-leak-avoiding skip).
        assert!(ir.contains("call ptr @plum_alloc_closure(i64 1)"), "{ir}");
        // `@plum_rc_inc` is always DEFINED in the runtime preamble
        // regardless of whether anything calls it — the real assertion
        // is that nothing ever CALLS it here.
        assert!(!ir.contains("call void @plum_rc_inc("), "{ir}");
        let alloc_idx = ir.find("call ptr @plum_alloc_closure(i64 1)").unwrap();
        // The self-capture slot is stored at `closure_field_byte_
        // offset(0)` = 24 — its `getelementptr` must appear AFTER the
        // alloc (the cell's address has to exist before it can be
        // stored into its own slot).
        let self_store_idx = ir[alloc_idx..].find("i64 24").expect("expected the self-capture slot's own offset");
        assert!(self_store_idx > 0, "{ir}");
    }

    #[test]
    fn bare_top_level_function_reference_used_as_a_value_generates_a_trampoline() {
        // let double(n: Int): Int = n * 2
        // let go(): Int = { let f = double; f(21) }
        let double_fn = Function {
            name: "double".to_string(),
            params: vec!["n".to_string()],
            body: Expr::Binary(BinOp::Mul, Box::new(Expr::Var("n".to_string())), Box::new(Expr::Int(2))),
        };
        let go_fn = Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Let {
                name: "f".to_string(),
                value: Box::new(Expr::Var("double".to_string())),
                body: Box::new(Expr::Call {
                    callee: Box::new(Expr::Var("f".to_string())),
                    args: vec![Expr::Int(21)],
                }),
            },
        };
        let prog = program(vec![double_fn, go_fn]);
        let ir = emit(
            &prog,
            &sigs(&[("double", vec![CgType::Int], CgType::Int), ("go", vec![], CgType::Int)]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("define i64 @trampoline$double("), "{ir}");
        assert!(ir.contains("call ptr @plum_alloc_closure(i64 0)"), "{ir}");
    }

    // --- `for`/`Assign` codegen (this chunk) ---

    /// Every one of this section's tests compiles a SINGLE function
    /// named `go` and cares only about phi/instruction counts WITHIN
    /// that function's own body — not the shared runtime preamble
    /// `emit_runtime` unconditionally emits ahead of it (which itself
    /// contains several `phi i64` loops, e.g. `@plum_str_starts_with`'s
    /// own byte-compare loop). Since `go` is the only function in every
    /// one of these programs, its `define` is textually the LAST one in
    /// the whole emitted module (`emit_program` always places runtime +
    /// array-release fns + closure defs BEFORE the program's own
    /// function bodies) — slicing from the last `"\ndefine "` isolates
    /// exactly `go`'s own body for these counts.
    fn go_body(ir: &str) -> &str {
        let idx = ir.rfind("\ndefine ").expect("expected at least one `define` in the emitted IR");
        &ir[idx..]
    }

    #[test]
    fn a_non_mutating_for_loop_only_produces_the_induction_variables_own_phi() {
        // let go () = { for i in 0..10 { i }; 0 }
        // No `Assign` anywhere in the body — `assigned_vars` finds
        // nothing loop-carried, so the loop header should have exactly
        // ONE phi (`%i` itself), not a second one for anything else.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Let {
                name: "_".to_string(),
                value: Box::new(Expr::For {
                    var: "i".to_string(),
                    start: Box::new(Expr::Int(0)),
                    end: Box::new(Expr::Int(10)),
                    body: Box::new(Expr::Var("i".to_string())),
                }),
                body: Box::new(Expr::Int(0)),
            },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        let body = go_body(&ir);
        let phi_count = body.matches(" = phi i64 [").count();
        assert_eq!(phi_count, 1, "expected exactly one phi (the induction variable), got:\n{ir}");
        assert!(ir.contains("icmp slt i64"), "{ir}");
    }

    #[test]
    fn an_accumulator_for_loop_produces_two_phis_and_the_accumulator_phi_traces_to_the_bodys_real_update() {
        // let go () = { let mut sum = 0; for i in 0..10 { sum = sum + i }; sum }
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Let {
                name: "sum".to_string(),
                value: Box::new(Expr::Int(0)),
                body: Box::new(Expr::Let {
                    name: "_".to_string(),
                    value: Box::new(Expr::For {
                        var: "i".to_string(),
                        start: Box::new(Expr::Int(0)),
                        end: Box::new(Expr::Int(10)),
                        body: Box::new(Expr::Assign {
                            name: "sum".to_string(),
                            value: Box::new(Expr::Binary(
                                BinOp::Add,
                                Box::new(Expr::Var("sum".to_string())),
                                Box::new(Expr::Var("i".to_string())),
                            )),
                            rest: Box::new(Expr::Unit),
                        }),
                    }),
                    body: Box::new(Expr::Var("sum".to_string())),
                }),
            },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        // Two header phis: the induction variable `%i` and the carried
        // accumulator `sum`.
        let body = go_body(&ir);
        let phi_count = body.matches(" = phi i64 [").count();
        assert_eq!(phi_count, 2, "expected exactly two phis (induction var + accumulator), got:\n{ir}");

        // The accumulator phi's SECOND operand must be the register the
        // `add` instruction inside the body actually produced — not a
        // stale/placeholder one. Find the `add i64 %sumN, %iN` line
        // (the body's real update) and confirm ITS result register is
        // exactly what the phi's second incoming value names.
        let add_line = ir
            .lines()
            .find(|l| l.trim_start().starts_with('%') && l.contains(" = add i64 ") && l.contains(", %v"))
            .unwrap_or_else(|| panic!("expected to find the accumulator's `add` instruction in:\n{ir}"));
        let add_reg = add_line.trim_start().split(' ').next().unwrap();
        ir.lines()
            .find(|l| l.contains(" = phi i64 ") && l.contains(&format!("[ {add_reg}, %")))
            .unwrap_or_else(|| {
                panic!("expected the accumulator phi's 2nd operand to be {add_reg:?} (the body's real update), got:\n{ir}")
            });
    }

    #[test]
    fn a_conditional_assign_inside_an_if_inside_a_for_loop_merges_via_merge_envs() {
        // let go () = { let mut acc = 0; for i in 0..5 { if i > 2 { acc = acc + i } else { () } }; acc }
        // Mirrors `.filter()`'s exact desugared shape (an `Assign`
        // nested inside one arm of an `If`, itself inside a `for` body)
        // — asserts `merge_envs`'s OWN phi shows up at the `If`'s own
        // merge block, not just the loop header (three total `phi i64`
        // sites: the induction variable, the loop-header accumulator
        // phi, AND the `If`-merge accumulator phi).
        let if_body = Expr::If {
            cond: Box::new(Expr::Binary(BinOp::Gt, Box::new(Expr::Var("i".to_string())), Box::new(Expr::Int(2)))),
            then_branch: Box::new(Expr::Assign {
                name: "acc".to_string(),
                value: Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Var("acc".to_string())),
                    Box::new(Expr::Var("i".to_string())),
                )),
                rest: Box::new(Expr::Unit),
            }),
            else_branch: Box::new(Expr::Unit),
        };
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Let {
                name: "acc".to_string(),
                value: Box::new(Expr::Int(0)),
                body: Box::new(Expr::Let {
                    name: "_".to_string(),
                    value: Box::new(Expr::For {
                        var: "i".to_string(),
                        start: Box::new(Expr::Int(0)),
                        end: Box::new(Expr::Int(5)),
                        body: Box::new(if_body),
                    }),
                    body: Box::new(Expr::Var("acc".to_string())),
                }),
            },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        // induction var phi + loop-header accumulator phi + If-merge
        // accumulator phi = 3.
        let body = go_body(&ir);
        let phi_count = body.matches(" = phi i64 [").count();
        assert_eq!(phi_count, 3, "expected 3 phis (induction var, loop-header acc, If-merge acc), got:\n{ir}");
        // The If-merge phi specifically must live in a `merge`-labeled
        // block (`merge_envs`'s own contribution), not the `for_header`.
        let merge_block_start = body.find("merge").expect("expected an If merge block");
        let after_merge = &body[merge_block_start..];
        assert!(after_merge.contains(" = phi i64 ["), "expected a phi inside the If's own merge block:\n{ir}");
    }

    #[test]
    fn assign_to_a_heap_shaped_variable_emits_no_extra_dec() {
        // let go (p: Point, q: Point): Point = { p = q; p }
        // `fbip.rs` already documents this as an accepted leak (the OLD
        // `p` is simply orphaned) — codegen must not invent a `Dec` here.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["p".to_string(), "q".to_string()],
            body: Expr::Assign {
                name: "p".to_string(),
                value: Box::new(Expr::Var("q".to_string())),
                rest: Box::new(Expr::Var("p".to_string())),
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap, CgType::Heap], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int])]),
        )
        .unwrap();
        // Scoped to `go`'s own body (not the shared runtime preamble,
        // whose `@plum_rc_dec` FUNCTION DEFINITION itself textually
        // contains `@plum_rc_dec(ptr %p)` as its own signature — that's
        // not a call site) and specifically a `call`, not just any
        // substring match.
        assert!(!go_body(&ir).contains("call void @plum_rc_dec(ptr %p)"), "expected no Dec of the overwritten `p`:\n{ir}");
    }

    #[test]
    fn assign_to_an_undeclared_variable_is_a_clear_error() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Assign {
                name: "nope".to_string(),
                value: Box::new(Expr::Int(1)),
                rest: Box::new(Expr::Unit),
            },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Unit)]), &TagFields::new())
            .expect_err("expected assignment to an undeclared variable to be rejected");
        assert!(err.contains("undeclared"), "unexpected error: {err}");
    }

    #[test]
    fn nested_for_loops_each_get_their_own_independent_accumulator_phi() {
        // let go () = {
        //   let mut total = 0;
        //   for i in 0..3 { for j in 0..3 { total = total + i * j } };
        //   total
        // }
        // `assigned_vars` recurses into a NESTED `for` (not a hard stop,
        // unlike `Closure`) — so the OUTER loop's own header ALSO gets a
        // carried phi for `total` (since it's reassigned somewhere
        // reachable from the outer body), independent of the INNER
        // loop's own carried phi for the same name.
        let inner = Expr::For {
            var: "j".to_string(),
            start: Box::new(Expr::Int(0)),
            end: Box::new(Expr::Int(3)),
            body: Box::new(Expr::Assign {
                name: "total".to_string(),
                value: Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Var("total".to_string())),
                    Box::new(Expr::Binary(BinOp::Mul, Box::new(Expr::Var("i".to_string())), Box::new(Expr::Var("j".to_string())))),
                )),
                rest: Box::new(Expr::Unit),
            }),
        };
        let outer = Expr::For {
            var: "i".to_string(),
            start: Box::new(Expr::Int(0)),
            end: Box::new(Expr::Int(3)),
            body: Box::new(Expr::Let {
                name: "_".to_string(),
                value: Box::new(inner),
                body: Box::new(Expr::Unit),
            }),
        };
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Let {
                name: "total".to_string(),
                value: Box::new(Expr::Int(0)),
                body: Box::new(Expr::Let {
                    name: "_".to_string(),
                    value: Box::new(outer),
                    body: Box::new(Expr::Var("total".to_string())),
                }),
            },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        // 2 induction-variable phis (i, j) + 2 independent `total`
        // carried phis (one per loop level) = 4.
        let body = go_body(&ir);
        let phi_count = body.matches(" = phi i64 [").count();
        assert_eq!(phi_count, 4, "expected 4 phis (2 induction vars + 2 independent `total` accumulators), got:\n{ir}");
        let header_count = body.matches("for_header").count();
        assert!(header_count >= 2, "expected at least two distinct `for_header` blocks:\n{ir}");
    }

    // --- spawn / join (this chunk) ---

    #[test]
    fn scalar_only_spawn_and_join_emits_pthread_calls_and_no_deepcopy_calls() {
        // let go(): Int = spawn { 1 + 2 }.join()
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::TaskJoin {
                task: Box::new(Expr::Spawn {
                    block: Box::new(Expr::Binary(BinOp::Add, Box::new(Expr::Int(1)), Box::new(Expr::Int(2)))),
                }),
            },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        assert!(ir.contains("declare i32 @pthread_create("), "{ir}");
        assert!(ir.contains("declare i32 @pthread_join("), "{ir}");
        assert!(ir.contains("call i32 @pthread_create("), "{ir}");
        assert!(ir.contains("call i32 @pthread_join("), "{ir}");
        // A scalar-only capture set (in this case: NO captures at all)
        // never calls a deep-copy function — `@plum_deepcopy_heap`/
        // `@plum_deepcopy_str` are still DEFINED unconditionally
        // whenever spawn is used anywhere (same "small fixed runtime,
        // emitted once" precedent as every other runtime function), but
        // nothing here actually CALLS one.
        assert!(!ir.contains("call ptr @plum_deepcopy_heap("), "{ir}");
        assert!(!ir.contains("call ptr @plum_deepcopy_str("), "{ir}");
        // The spawn-args block is entirely skipped (zero captures) —
        // `null` is passed directly as `pthread_create`'s `arg`.
        assert!(ir.contains("@pthread_create(ptr %v"), "{ir}");
    }

    #[test]
    fn spawn_capturing_a_heap_shaped_value_deep_copies_it_and_never_incs_the_original() {
        // let go(p: Point): Point = { let t = spawn { p }; t.join() }
        let body = Expr::Let {
            name: "t".to_string(),
            value: Box::new(Expr::Spawn { block: Box::new(Expr::Var("p".to_string())) }),
            body: Box::new(Expr::TaskJoin { task: Box::new(Expr::Var("t".to_string())) }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["p".to_string()], body }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        // The captured original (`%p`, the function's own parameter
        // register) is deep-copied via `@plum_deepcopy_heap` — the key
        // structural proxy that this is a real, independent snapshot,
        // not a shared pointer a closure capture would instead
        // `plum_rc_inc`.
        assert!(ir.contains("call ptr @plum_deepcopy_heap(ptr %p)"), "{ir}");
        // And critically: nothing anywhere in the whole program ever
        // `plum_rc_inc`s `%p` — if it did, both this thread and the
        // spawned one could end up racing a non-atomic refcount word on
        // the SAME original cell, exactly the bug deep-copy exists to
        // prevent (see `deep_copy_capture`'s own doc comment in
        // codegen.rs).
        assert!(!ir.contains("call void @plum_rc_inc(ptr %p)"), "{ir}");
    }

    #[test]
    fn spawn_capturing_a_closure_typed_value_is_a_clear_error() {
        // let go(f: Closure([Int], Int)): Int = { spawn { f(1) }; 0 }
        let body = Expr::Let {
            name: "_t".to_string(),
            value: Box::new(Expr::Spawn {
                block: Box::new(Expr::Call { callee: Box::new(Expr::Var("f".to_string())), args: vec![Expr::Int(1)] }),
            }),
            body: Box::new(Expr::Int(0)),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["f".to_string()], body }]);
        let closure_sig = CgType::Closure(vec![CgType::Int], Box::new(CgType::Int));
        let err = emit(&prog, &sigs(&[("go", vec![closure_sig], CgType::Int)]), &TagFields::new())
            .expect_err("expected a closure-typed spawn capture to be rejected");
        assert!(err.contains("spawn") && err.contains("cross a thread boundary"), "unexpected error: {err}");
    }

    #[test]
    fn spawn_using_program_rejects_any_struct_with_a_closure_field_even_if_never_actually_captured() {
        // The program's ONLY spawn never touches `Holder` at all — this
        // proves the whole-program, structurally-conservative rejection
        // (`check_no_closure_or_task_fields`) fires regardless, since
        // `Heap` is opaque at a real capture site and can't otherwise be
        // inspected for a closure hiding three fields deep.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::TaskJoin { task: Box::new(Expr::Spawn { block: Box::new(Expr::Int(1)) }) },
        }]);
        let closure_ty = CgType::Closure(vec![], Box::new(CgType::Int));
        let err = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Int)]),
            &tags(&[("Holder", vec![closure_ty])]),
        )
        .expect_err("expected the whole program to be rejected");
        assert!(err.contains("Holder"), "unexpected error: {err}");
        assert!(err.contains("closure") || err.contains("task"), "unexpected error: {err}");
    }

    #[test]
    fn spawn_using_program_rejects_any_struct_with_a_task_field_even_if_never_actually_captured() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::TaskJoin { task: Box::new(Expr::Spawn { block: Box::new(Expr::Int(1)) }) },
        }]);
        let task_ty = CgType::Task(Box::new(CgType::Int));
        let err = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Int)]),
            &tags(&[("Holder", vec![task_ty])]),
        )
        .expect_err("expected the whole program to be rejected");
        assert!(err.contains("Holder"), "unexpected error: {err}");
    }

    #[test]
    fn a_program_with_a_closure_field_but_no_spawn_anywhere_is_unaffected() {
        // The SAME `Holder` shape as the rejection test above — but
        // with no `spawn` anywhere in the program at all, this must
        // compile cleanly: `check_no_closure_or_task_fields` only ever
        // runs when `needs_spawn_runtime` is true.
        let prog = program(vec![Function { name: "go".to_string(), params: vec![], body: Expr::Int(1) }]);
        let closure_ty = CgType::Closure(vec![], Box::new(CgType::Int));
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Int)]),
            &tags(&[("Holder", vec![closure_ty])]),
        )
        .unwrap();
        assert!(!ir.contains("declare i32 @pthread_create"), "{ir}");
    }

    #[test]
    fn task_join_checks_the_joined_flag_and_aborts_on_a_second_join() {
        // let go(): Int = { let t = spawn { 1 }; t.join() }
        let body = Expr::Let {
            name: "t".to_string(),
            value: Box::new(Expr::Spawn { block: Box::new(Expr::Int(1)) }),
            body: Box::new(Expr::TaskJoin { task: Box::new(Expr::Var("t".to_string())) }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec![], body }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        // The joined-flag load (byte offset 0) + comparison against 0.
        assert!(ir.contains("icmp eq i64"), "{ir}");
        // The runtime-checked-failure mechanism (`emit_runtime_check`,
        // the SAME one array/string bounds checks use) — aborts via
        // `@plum_abort` rather than continuing if the task was already
        // joined.
        assert!(ir.contains("call void @plum_abort("), "{ir}");
        assert!(ir.contains("unreachable"), "{ir}");
        // The flag is then marked joined (`store i64 1, ...`) before
        // the real `pthread_join` proceeds.
        assert!(ir.contains("store i64 1, ptr"), "{ir}");
        assert!(ir.contains("call i32 @pthread_join("), "{ir}");
        // Both the tiny result box AND the task cell itself are `free`d
        // exactly once — the cell is consumed, not reusable.
        let go_start = ir.find("define i64 @go(").unwrap();
        let go_body = &ir[go_start..];
        assert_eq!(go_body.matches("call void @free(").count(), 2, "{go_body}");
    }

    #[test]
    fn join_on_a_non_task_value_is_a_clear_error() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::TaskJoin { task: Box::new(Expr::Int(1)) },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new())
            .expect_err("expected a non-Task `.join()` target to be rejected");
        assert!(err.contains("Task"), "unexpected error: {err}");
    }

    // --- channels / select (this chunk) ---

    /// `let (tx, rx) = channel[Int]()` — the exact shape `wrap_destructure`
    /// lowers a channel destructure into: a `Let` binding a synthetic
    /// name to `Expr::Channel`, whose `body` is a `Match` with a single
    /// `"2Tuple"`-tagged arm binding `tx`/`rx` positionally.
    fn channel_destructure_body(tail: Expr) -> Expr {
        Expr::Let {
            name: "__nested0".to_string(),
            value: Box::new(Expr::Channel),
            body: Box::new(Expr::Match {
                scrutinee: Box::new(Expr::Var("__nested0".to_string())),
                arms: vec![MatchArm {
                    tag: "2Tuple".to_string(),
                    bindings: vec!["tx".to_string(), "rx".to_string()],
                    guard: None,
                    body: tail,
                }],
            }),
        }
    }

    fn int_channel_tags() -> TagFields {
        tags(&[("2Tuple", vec![CgType::Sender(Box::new(CgType::Int)), CgType::Receiver(Box::new(CgType::Int))])])
    }

    #[test]
    fn channel_creation_emits_mutex_and_cond_init_shape() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: channel_destructure_body(Expr::Int(0)),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &int_channel_tags()).unwrap();
        assert!(ir.contains("call ptr @plum_channel_new()"), "{ir}");
        assert!(ir.contains("declare i32 @pthread_mutex_init(ptr, ptr)"), "{ir}");
        assert!(ir.contains("declare i32 @pthread_cond_init(ptr, ptr)"), "{ir}");
        // The queue struct's own init calls: mutex at offset 0 (the
        // struct pointer itself), cond at offset 40.
        assert!(ir.contains("call i32 @pthread_mutex_init(ptr %q, ptr null)"), "{ir}");
        assert!(ir.contains("%cond = getelementptr i8, ptr %q, i64 40"), "{ir}");
        assert!(ir.contains("call i32 @pthread_cond_init(ptr %cond, ptr null)"), "{ir}");
    }

    #[test]
    fn send_of_a_heap_value_deep_copies_and_never_incs() {
        // let go(p: Point): Int = { let (tx, rx) = channel[Point](); tx.send(p); 0 }
        let body = Expr::Let {
            name: "__nested0".to_string(),
            value: Box::new(Expr::Channel),
            body: Box::new(Expr::Match {
                scrutinee: Box::new(Expr::Var("__nested0".to_string())),
                arms: vec![MatchArm {
                    tag: "2Tuple".to_string(),
                    bindings: vec!["tx".to_string(), "rx".to_string()],
                    guard: None,
                    body: Expr::Let {
                        name: "_s".to_string(),
                        value: Box::new(Expr::ChannelSend {
                            sender: Box::new(Expr::Var("tx".to_string())),
                            value: Box::new(Expr::Var("p".to_string())),
                        }),
                        body: Box::new(Expr::Int(0)),
                    },
                }],
            }),
        };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["p".to_string()], body }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Int)]),
            &tags(&[
                ("Point", vec![CgType::Int, CgType::Int]),
                ("2Tuple", vec![CgType::Sender(Box::new(CgType::Heap)), CgType::Receiver(Box::new(CgType::Heap))]),
            ]),
        )
        .unwrap();
        assert!(ir.contains("call ptr @plum_deepcopy_heap(ptr %p)"), "{ir}");
        assert!(!ir.contains("call void @plum_rc_inc(ptr %p)"), "{ir}");
        assert!(ir.contains("call void @plum_channel_send("), "{ir}");
    }

    #[test]
    fn sending_a_sender_over_another_channel_copies_the_pointer_verbatim() {
        // A channel sent over another channel: `.send()`'s value is
        // itself `Sender`-typed — must be the SAME verbatim pointer
        // copy `deep_copy_capture` gives `spawn` captures, never a
        // deep-copy call (there's nothing to `@plum_deepcopy_*` a
        // channel handle THROUGH).
        let body = Expr::Let {
            name: "_s".to_string(),
            value: Box::new(Expr::ChannelSend {
                sender: Box::new(Expr::Var("outer_tx".to_string())),
                value: Box::new(Expr::Var("inner_tx".to_string())),
            }),
            body: Box::new(Expr::Int(0)),
        };
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["outer_tx".to_string(), "inner_tx".to_string()],
            body,
        }]);
        let ir = emit(
            &prog,
            &sigs(&[(
                "go",
                vec![
                    CgType::Sender(Box::new(CgType::Sender(Box::new(CgType::Int)))),
                    CgType::Sender(Box::new(CgType::Int)),
                ],
                CgType::Int,
            )]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("call void @plum_channel_send(ptr %outer_tx, i64"), "{ir}");
        assert!(!ir.contains("call ptr @plum_deepcopy_heap("), "{ir}");
        assert!(!ir.contains("call ptr @plum_deepcopy_str("), "{ir}");
        // The word sent is `%inner_tx` itself (`ptrtoint`), not the
        // result of any copy function.
        assert!(ir.contains("ptrtoint ptr %inner_tx to i64"), "{ir}");
    }

    #[test]
    fn recv_emits_a_real_cond_wait_loop_with_no_second_deep_copy() {
        // let go(rx: Receiver[Point]): Point = rx.recv()
        let body = Expr::ChannelRecv { receiver: Box::new(Expr::Var("rx".to_string())) };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["rx".to_string()], body }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Receiver(Box::new(CgType::Heap))], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("call i64 @plum_channel_recv(ptr %rx)"), "{ir}");
        assert!(ir.contains("call i32 @pthread_cond_wait(ptr %cond, ptr %q)"), "{ir}");
        // No deep-copy CALL anywhere in the whole program — `.recv()`
        // adopts the popped word directly (see `codegen_channel_recv`'s
        // own doc comment for the happens-before argument). The `@plum_
        // deepcopy_heap` FUNCTION DEFINITION is still unconditionally
        // emitted (same "small fixed runtime, always defined once
        // needed" precedent `scalar_only_spawn_and_join_emits_pthread_
        // calls_and_no_deepcopy_calls` already established for spawn) —
        // only its absence as a CALL site is asserted here.
        let go_start = ir.find("define ptr @go(").unwrap();
        let go_body = &ir[go_start..];
        assert!(!go_body.contains("call ptr @plum_deepcopy_heap("), "{go_body}");
    }

    #[test]
    fn select_polls_arms_in_fixed_index_order_and_never_blocks() {
        // select { rx0.recv() => 0, rx1.recv() => 1, rx2.recv() => 2 }
        let arm = |rx: &str, n: i64| SelectArm { receiver: Expr::Var(rx.to_string()), body: Expr::Int(n) };
        let body = Expr::Select { arms: vec![arm("rx0", 0), arm("rx1", 1), arm("rx2", 2)] };
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["rx0".to_string(), "rx1".to_string(), "rx2".to_string()],
            body,
        }]);
        let recv_ty = CgType::Receiver(Box::new(CgType::Int));
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![recv_ty.clone(), recv_ty.clone(), recv_ty], CgType::Int)]),
            &TagFields::new(),
        )
        .unwrap();
        // Fixed index order: %rx0's try_recv call textually precedes
        // %rx1's, which precedes %rx2's.
        let i0 = ir.find("@plum_channel_try_recv(ptr %rx0").unwrap();
        let i1 = ir.find("@plum_channel_try_recv(ptr %rx1").unwrap();
        let i2 = ir.find("@plum_channel_try_recv(ptr %rx2").unwrap();
        assert!(i0 < i1 && i1 < i2, "expected fixed arm-0/1/2 poll order:\n{ir}");
        // A genuine busy-poll (usleep-and-retry) — never a blocking
        // `pthread_cond_wait`, unlike `.recv()`. `@plum_channel_recv`
        // (unused here) still gets DEFINED unconditionally as part of
        // the small, fixed channel runtime — so this checks `go`'s OWN
        // body never CALLS `pthread_cond_wait`, not that the substring
        // never appears anywhere in the whole program's text.
        assert!(ir.contains("call i32 @usleep(i32 1000)"), "{ir}");
        let go_start = ir.find("define i64 @go(").unwrap();
        let go_body = &ir[go_start..];
        assert!(!go_body.contains("pthread_cond_wait"), "{go_body}");
    }

    #[test]
    fn select_arm_1_being_the_ready_one_does_not_require_arm_0_specific_codegen() {
        // Structural proxy for "the loop doesn't just always pick arm
        // 0" (the actual RUNTIME proof is `plumc`'s own compile-and-run
        // test) — asserts each arm's own body is reachable via its OWN
        // matched block, not hardcoded to arm 0's.
        let arm = |rx: &str, n: i64| SelectArm { receiver: Expr::Var(rx.to_string()), body: Expr::Int(n) };
        let body = Expr::Select { arms: vec![arm("rx0", 10), arm("rx1", 20)] };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["rx0".to_string(), "rx1".to_string()], body }]);
        let recv_ty = CgType::Receiver(Box::new(CgType::Int));
        let ir = emit(&prog, &sigs(&[("go", vec![recv_ty.clone(), recv_ty], CgType::Int)]), &TagFields::new()).unwrap();
        assert!(ir.contains("ret i64 10"), "{ir}");
        assert!(ir.contains("ret i64 20"), "{ir}");
    }

    #[test]
    fn sending_a_closure_on_a_channel_is_a_clear_error() {
        // `f` (a Closure-typed param) is sent directly.
        let body = Expr::Let {
            name: "_s".to_string(),
            value: Box::new(Expr::ChannelSend {
                sender: Box::new(Expr::Var("tx".to_string())),
                value: Box::new(Expr::Var("f".to_string())),
            }),
            body: Box::new(Expr::Int(0)),
        };
        let closure_ty = CgType::Closure(vec![], Box::new(CgType::Int));
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["tx".to_string(), "f".to_string()],
            body,
        }]);
        let err = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Sender(Box::new(closure_ty.clone())), closure_ty], CgType::Int)]),
            &TagFields::new(),
        )
        .expect_err("expected a closure-typed channel send to be rejected");
        assert!(err.contains("send") && err.contains("thread boundary"), "unexpected error: {err}");
    }

    #[test]
    fn a_struct_with_a_closure_field_is_rejected_when_only_channel_is_used_no_spawn() {
        // Proves the OR-gating: `check_no_closure_or_task_fields` fires
        // from the CHANNEL side even with no `spawn` anywhere in the
        // program at all.
        let body = Expr::ChannelRecv { receiver: Box::new(Expr::Var("rx".to_string())) };
        let prog = program(vec![Function { name: "go".to_string(), params: vec!["rx".to_string()], body }]);
        let closure_ty = CgType::Closure(vec![], Box::new(CgType::Int));
        let err = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Receiver(Box::new(CgType::Int))], CgType::Int)]),
            &tags(&[("Holder", vec![closure_ty])]),
        )
        .expect_err("expected the whole program to be rejected even though only `channel` (no `spawn`) is used");
        assert!(err.contains("Holder"), "unexpected error: {err}");
        assert!(err.contains("closure") || err.contains("task"), "unexpected error: {err}");
    }

    #[test]
    fn a_hand_built_zero_arm_select_is_a_defensive_error_never_a_panic() {
        let prog = program(vec![Function { name: "go".to_string(), params: vec![], body: Expr::Select { arms: vec![] } }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new())
            .expect_err("expected a zero-arm select to be a clear error, not a panic");
        assert!(err.contains("zero arms"), "unexpected error: {err}");
    }
}
