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
}

impl CgType {
    fn llvm_type(&self) -> &'static str {
        match self {
            CgType::Int => "i64",
            CgType::Float => "double",
            CgType::Bool | CgType::Unit => "i1",
            CgType::Heap | CgType::Str | CgType::Array(_) | CgType::Closure(..) => "ptr",
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
        )?);
        bodies.push('\n');
    }

    let mut out = emit_runtime(tag_fields, &tag_ids);
    out.push_str(&emit_array_release_fns(&needed_arrays.into_inner()));
    // Every closure-literal-site-generated function/release function/
    // trampoline, discovered while walking `program.functions` above —
    // spliced in here, same "collect everything while emitting bodies,
    // then place it before the bodies in the final text" convention
    // `emit_array_release_fns` itself already established.
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
    };

    let mut env = HashMap::new();
    let mut param_decls = Vec::with_capacity(f.params.len());
    for (name, ty) in f.params.iter().zip(&sig.params) {
        env.insert(name.clone(), (format!("%{name}"), ty.clone()));
        param_decls.push(format!("{} %{name}", ty.llvm_type()));
    }

    let mut em = codegen::Emitter::new();
    let result = codegen::codegen_expr(&f.body, &env, &mut em, &ctx, true)?;
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
    use plum_ir::ir::{BinOp, Expr, Function, MatchArm, PrimTy, Program, RcOp, UnOp};

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
    fn a_still_unsupported_array_for_loop_is_a_clear_error() {
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
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
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
}
