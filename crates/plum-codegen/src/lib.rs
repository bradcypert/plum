mod codegen;

use plum_ir::ir;
use std::collections::{BTreeMap, HashMap};

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
    /// A bare, NUL-terminated C string pointer (`char*`) produced ONLY
    /// by `Expr::AsCStr` (surface syntax `.as_cstr()`) — `ptr` at the
    /// LLVM level, like `Str`, but a genuinely DIFFERENT representation,
    /// not merely a relabeling: `Str` is Plum's own length-prefixed,
    /// REFCOUNTED heap cell (`{ i64 refcount, i64 len, bytes... }`,
    /// allocated via `@plum_alloc_str`); `CStr` has no header, no
    /// refcount, and no length prefix at all — just a raw pointer to
    /// NUL-terminated bytes, exactly the shape a C API expects. Because
    /// there's no refcount word, `dec_fn_for`/`deepcopy_fn_for` both
    /// return `None` for it (mirroring `Sender`/`Receiver`'s "no
    /// refcount word anywhere in the layout" reasoning, not `Task`'s
    /// "deliberately untracked" one) — but UNLIKE `Sender`/`Receiver`
    /// (which legitimately cross a thread boundary as a verbatim
    /// pointer copy), a `CStr` is REJECTED from crossing a spawn/channel
    /// boundary entirely (see `check_no_closure_or_task_fields`'s own
    /// doc comment): an unowned, unsynchronized raw pointer aliased
    /// across two threads with no shared-ownership protocol at all is
    /// strictly worse than a `Task`/`Closure` capture, which at least
    /// has defined single-owner or Plum-managed semantics. See
    /// `codegen.rs`'s `codegen_as_cstr` for why producing one requires a
    /// FRESH allocation (never a pointer aliased into an existing `Str`
    /// cell) — a genuine soundness requirement, not a missed
    /// optimization.
    CStr,
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
            | CgType::Receiver(_)
            | CgType::CStr => "ptr",
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
            CgType::CStr => "CStr".to_string(),
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
        // No refcount word anywhere — a `CStr` is a bare, header-free
        // `malloc`'d buffer (see `CgType::CStr`'s own doc comment), not
        // a Plum-managed heap cell at all. Unlike `Str`'s `@plum_rc_dec_
        // str`, there is no analogous "dec a CStr" runtime function:
        // nothing in this backend ever owns a `CStr` value long enough
        // to need one — it's produced fresh by `AsCStr` and consumed
        // immediately as an extern-call argument (see `codegen.rs`'s
        // `codegen_as_cstr`), never bound into a heap cell's field or
        // otherwise kept alive past its one use site.
        CgType::CStr => None,
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
    out.push_str("declare i32 @snprintf(ptr, i64, ptr, ...)\n");
    // `@strlen`/`@memchr` — reached for over hand-rolled loops for the
    // SAME reason `@memcpy`/`@memmove` above are: a real libc primitive
    // already exists and is trusted elsewhere in this backend, matching
    // its established "reach for a real libc declare over hand-rolled
    // codegen" precedent. `@strlen` measures a `CStr`-typed extern
    // return's length before copying it into a fresh Plum `Str` cell
    // (`codegen.rs`'s `codegen_extern_call`); `@memchr` validates
    // `.as_cstr()`'s embedded-NUL check (`codegen_as_cstr`). Declared
    // here UNCONDITIONALLY, alongside this function's other always-
    // present libc declares, rather than reactively gated the way the
    // spawn/channel runtime is — both are used only internally by
    // codegen's own FFI machinery, never left undeclared-but-referenced,
    // so the (tiny, fixed) cost of always declaring them is simpler than
    // adding a third reactive-gating flag alongside `needs_spawn_
    // runtime`/`needs_channel_runtime` for what's a rare, narrow-scope
    // feature. Also why both names are reserved against user `extern`
    // declarations (see `is_reserved_extern_name`) — a user declaring
    // their own `strlen`/`memchr` would otherwise collide with these.
    out.push_str("declare i64 @strlen(ptr)\n");
    out.push_str("declare ptr @memchr(ptr, i32, i64)\n");
    // `@memcmp` — backs `@plum_str_eq` below (Str `==`/`!=`), the same
    // "reach for a real libc declare" precedent as `@memcpy`/`@strlen`/
    // `@memchr` above.
    out.push_str("declare i32 @memcmp(ptr, ptr, i64)\n\n");

    // `@towupper`/`@towlower`/`@setlocale` — the real Unicode-aware case
    // mapping primitives `.to_upper()`/`.to_lower()` are built on (see
    // `@plum_str_to_upper`/`@plum_str_to_lower` below), matching this
    // backend's "reach for a real libc declare over hand-rolled codegen"
    // philosophy already established by `@memcpy`/`@strlen`/`@memchr`
    // above. `@plum_locale_init` calls `@setlocale(LC_ALL, "C.utf8")`
    // exactly once — confirmed empirically (real scratch-C-program
    // testing against this platform's glibc) that WITHOUT this call,
    // `towupper`/`towlower` default to the ASCII-only "C" locale, silently
    // reproducing the very ASCII-only behavior this chunk replaces.
    // `C.utf8` (built into glibc since 2.35, confirmed via `locale -a`,
    // no locale generation needed) is used over e.g. `en_US.UTF-8` for
    // portability — it doesn't depend on a specific locale being
    // installed. `LC_ALL = 6` is this platform's real value, confirmed
    // via a `setlocale`/`limits.h` scratch program, not assumed from
    // documentation (glibc's `LC_ALL` numbering isn't standardized by
    // POSIX). Declared/emitted unconditionally, matching every other
    // primitive in this function.
    out.push_str("declare i32 @towupper(i32)\n");
    out.push_str("declare i32 @towlower(i32)\n");
    out.push_str("declare ptr @setlocale(i32, ptr)\n");
    out.push_str("@plum_locale_str = private constant [7 x i8] c\"C.utf8\\00\"\n");
    out.push_str("define void @plum_locale_init() {\n");
    out.push_str("entry:\n");
    out.push_str("  %r = call ptr @setlocale(i32 6, ptr @plum_locale_str)\n");
    out.push_str("  ret void\n");
    out.push_str("}\n\n");

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

    // `@plum_str_eq` — backs Str `==`/`!=` in `codegen_binop` (previously
    // entirely unsupported in this backend: `Str` fell through to a hard
    // `Err` there). Fast-reject on length mismatch (same `len` offset,
    // 8, `@plum_str_contains` above reads), otherwise `@memcmp` the byte
    // ranges (data starts at offset 16, same layout as everywhere else
    // in this runtime) and check for exact equality.
    out.push_str(
        "define i1 @plum_str_eq(ptr %a, ptr %b) {\n\
         entry:\n\
         \x20 %alen_addr = getelementptr i8, ptr %a, i64 8\n\
         \x20 %alen = load i64, ptr %alen_addr\n\
         \x20 %blen_addr = getelementptr i8, ptr %b, i64 8\n\
         \x20 %blen = load i64, ptr %blen_addr\n\
         \x20 %same_len = icmp eq i64 %alen, %blen\n\
         \x20 br i1 %same_len, label %cmp_bytes, label %not_equal\n\
         cmp_bytes:\n\
         \x20 %adata = getelementptr i8, ptr %a, i64 16\n\
         \x20 %bdata = getelementptr i8, ptr %b, i64 16\n\
         \x20 %cmp = call i32 @memcmp(ptr %adata, ptr %bdata, i64 %alen)\n\
         \x20 %eq = icmp eq i32 %cmp, 0\n\
         \x20 ret i1 %eq\n\
         not_equal:\n\
         \x20 ret i1 0\n\
         }\n\n",
    );

    // --- Unicode string ops (`.runes()`/`.trim()`/`.split()`/
    // `.to_upper()`/`.to_lower()`/`.replace()`) ---
    //
    // A shared UTF-8 decoder pair, a small Unicode-whitespace classifier,
    // and one runtime function per op (built ON TOP of those, plus the
    // string primitives above) — see this crate's own design notes (the
    // implementing chunk's plan) for the full reasoning. Built with
    // plain `push_str` calls per instruction line (not one big escaped
    // literal like the functions above) purely for THIS author's own
    // review-ability while hand-assembling the more intricate control
    // flow below — functionally no different.
    //
    // `.to_upper()`/`.to_lower()` use real Unicode SIMPLE case mapping
    // via libc's `towupper`/`towlower` (see `@plum_str_to_upper`/
    // `@plum_str_to_lower` below and `@plum_locale_init` above). The
    // one remaining, precisely-scoped divergence from the interpreter's
    // full Unicode `str::to_uppercase()`/`to_lowercase()`: multi-
    // codepoint expansions (e.g. German `ß` -> `"SS"`) structurally
    // cannot happen through `towupper`/`towlower`'s 1-in-1-out C
    // signature, so `ß` stays `ß`. See DESIGN.md's "Strings" section
    // for the language-level caveat this drives.
    out.push_str(
        "; --- Unicode string runtime ---\n\
         ; `@plum_utf8_len_at`/`@plum_utf8_decode` are the shared UTF-8\n\
         ; decode primitives every op below (`.runes()`, `.trim()`,\n\
         ; `.split(\"\")`, `.replace(\"\", to)`) walks the buffer with.\n\
         ; ASSUME-VALID-BY-CONSTRUCTION: no defensive malformed-UTF-8\n\
         ; handling anywhere here — a Plum `Str` can only ever originate\n\
         ; from valid UTF-8 source text or byte-preserving transforms of\n\
         ; one, matching this backend's existing trust of `@memcpy`/\n\
         ; `@strlen` without extra validation. Revisit this assumption if\n\
         ; a future feature ever lets raw arbitrary bytes become a `Str`.\n",
    );

    // `@plum_utf8_len_at` — classify ONLY (no continuation-byte decode),
    // for callers that only need to advance a byte cursor one character
    // (`.trim()`'s backward scan doesn't use this one — see its own
    // "find character start scanning backwards" comment below — but
    // `.runes()`'s/`.split("")`'s/`.replace("", to)`'s counting passes
    // do). A sequential `icmp`+`br` classification chain, matching
    // `@plum_release_fields`'s established "no `switch`" style.
    out.push_str("define i64 @plum_utf8_len_at(ptr %base, i64 %pos) {\n");
    out.push_str("entry:\n");
    out.push_str("  %addr = getelementptr i8, ptr %base, i64 %pos\n");
    out.push_str("  %b0_i8 = load i8, ptr %addr\n");
    out.push_str("  %b0 = zext i8 %b0_i8 to i64\n");
    out.push_str("  %and80 = and i64 %b0, 128\n");
    out.push_str("  %is1 = icmp eq i64 %and80, 0\n");
    out.push_str("  br i1 %is1, label %len1, label %check2\n");
    out.push_str("len1:\n");
    out.push_str("  ret i64 1\n");
    out.push_str("check2:\n");
    out.push_str("  %andE0 = and i64 %b0, 224\n");
    out.push_str("  %is2 = icmp eq i64 %andE0, 192\n");
    out.push_str("  br i1 %is2, label %len2, label %check3\n");
    out.push_str("len2:\n");
    out.push_str("  ret i64 2\n");
    out.push_str("check3:\n");
    out.push_str("  %andF0 = and i64 %b0, 240\n");
    out.push_str("  %is3 = icmp eq i64 %andF0, 224\n");
    out.push_str("  br i1 %is3, label %len3, label %len4\n");
    out.push_str("len3:\n");
    out.push_str("  ret i64 3\n");
    out.push_str("len4:\n");
    out.push_str("  ret i64 4\n");
    out.push_str("}\n\n");

    // `@plum_utf8_decode` — classify AND decode: extracts/combines the
    // continuation bytes' low 6 bits via `shl`+`or` into the actual
    // Unicode scalar value, writing the character's byte length to
    // `%out_nbytes` as an out-parameter (this backend's established
    // "simple ptr/i64/i1 signatures" style, not an LLVM multi-return
    // struct, which has no precedent here) so callers needing BOTH the
    // codepoint and the advance amount (every fill/decode pass below)
    // don't need a second call.
    out.push_str("define i64 @plum_utf8_decode(ptr %base, i64 %pos, ptr %out_nbytes) {\n");
    out.push_str("entry:\n");
    out.push_str("  %addr = getelementptr i8, ptr %base, i64 %pos\n");
    out.push_str("  %b0_i8 = load i8, ptr %addr\n");
    out.push_str("  %b0 = zext i8 %b0_i8 to i64\n");
    out.push_str("  %and80 = and i64 %b0, 128\n");
    out.push_str("  %is1 = icmp eq i64 %and80, 0\n");
    out.push_str("  br i1 %is1, label %len1, label %check2\n");
    out.push_str("len1:\n");
    out.push_str("  store i64 1, ptr %out_nbytes\n");
    out.push_str("  ret i64 %b0\n");
    out.push_str("check2:\n");
    out.push_str("  %andE0 = and i64 %b0, 224\n");
    out.push_str("  %is2 = icmp eq i64 %andE0, 192\n");
    out.push_str("  br i1 %is2, label %len2, label %check3\n");
    out.push_str("len2:\n");
    out.push_str("  %pos1_2 = add i64 %pos, 1\n");
    out.push_str("  %addr1_2 = getelementptr i8, ptr %base, i64 %pos1_2\n");
    out.push_str("  %b1_2_i8 = load i8, ptr %addr1_2\n");
    out.push_str("  %b1_2 = zext i8 %b1_2_i8 to i64\n");
    out.push_str("  %b1_2low = and i64 %b1_2, 63\n");
    out.push_str("  %lead5 = and i64 %b0, 31\n");
    out.push_str("  %lead5sh = shl i64 %lead5, 6\n");
    out.push_str("  %cp2 = or i64 %lead5sh, %b1_2low\n");
    out.push_str("  store i64 2, ptr %out_nbytes\n");
    out.push_str("  ret i64 %cp2\n");
    out.push_str("check3:\n");
    out.push_str("  %andF0 = and i64 %b0, 240\n");
    out.push_str("  %is3 = icmp eq i64 %andF0, 224\n");
    out.push_str("  br i1 %is3, label %len3, label %len4\n");
    out.push_str("len3:\n");
    out.push_str("  %pos1_3 = add i64 %pos, 1\n");
    out.push_str("  %addr1_3 = getelementptr i8, ptr %base, i64 %pos1_3\n");
    out.push_str("  %b1_3_i8 = load i8, ptr %addr1_3\n");
    out.push_str("  %b1_3 = zext i8 %b1_3_i8 to i64\n");
    out.push_str("  %b1_3low = and i64 %b1_3, 63\n");
    out.push_str("  %pos2_3 = add i64 %pos, 2\n");
    out.push_str("  %addr2_3 = getelementptr i8, ptr %base, i64 %pos2_3\n");
    out.push_str("  %b2_3_i8 = load i8, ptr %addr2_3\n");
    out.push_str("  %b2_3 = zext i8 %b2_3_i8 to i64\n");
    out.push_str("  %b2_3low = and i64 %b2_3, 63\n");
    out.push_str("  %lead4 = and i64 %b0, 15\n");
    out.push_str("  %lead4sh = shl i64 %lead4, 12\n");
    out.push_str("  %b1_3sh = shl i64 %b1_3low, 6\n");
    out.push_str("  %tmp3 = or i64 %lead4sh, %b1_3sh\n");
    out.push_str("  %cp3 = or i64 %tmp3, %b2_3low\n");
    out.push_str("  store i64 3, ptr %out_nbytes\n");
    out.push_str("  ret i64 %cp3\n");
    out.push_str("len4:\n");
    out.push_str("  %pos1_4 = add i64 %pos, 1\n");
    out.push_str("  %addr1_4 = getelementptr i8, ptr %base, i64 %pos1_4\n");
    out.push_str("  %b1_4_i8 = load i8, ptr %addr1_4\n");
    out.push_str("  %b1_4 = zext i8 %b1_4_i8 to i64\n");
    out.push_str("  %b1_4low = and i64 %b1_4, 63\n");
    out.push_str("  %pos2_4 = add i64 %pos, 2\n");
    out.push_str("  %addr2_4 = getelementptr i8, ptr %base, i64 %pos2_4\n");
    out.push_str("  %b2_4_i8 = load i8, ptr %addr2_4\n");
    out.push_str("  %b2_4 = zext i8 %b2_4_i8 to i64\n");
    out.push_str("  %b2_4low = and i64 %b2_4, 63\n");
    out.push_str("  %pos3_4 = add i64 %pos, 3\n");
    out.push_str("  %addr3_4 = getelementptr i8, ptr %base, i64 %pos3_4\n");
    out.push_str("  %b3_4_i8 = load i8, ptr %addr3_4\n");
    out.push_str("  %b3_4 = zext i8 %b3_4_i8 to i64\n");
    out.push_str("  %b3_4low = and i64 %b3_4, 63\n");
    out.push_str("  %lead3 = and i64 %b0, 7\n");
    out.push_str("  %lead3sh = shl i64 %lead3, 18\n");
    out.push_str("  %b1_4sh = shl i64 %b1_4low, 12\n");
    out.push_str("  %b2_4sh = shl i64 %b2_4low, 6\n");
    out.push_str("  %tmp4a = or i64 %lead3sh, %b1_4sh\n");
    out.push_str("  %tmp4b = or i64 %tmp4a, %b2_4sh\n");
    out.push_str("  %cp4 = or i64 %tmp4b, %b3_4low\n");
    out.push_str("  store i64 4, ptr %out_nbytes\n");
    out.push_str("  ret i64 %cp4\n");
    out.push_str("}\n\n");

    // `@plum_utf8_encoded_len` — pure length classification of an
    // ALREADY-KNOWN codepoint VALUE (unlike `@plum_utf8_len_at`, which
    // classifies from a leading BYTE it hasn't decoded yet): `cp < 0x80`
    // -> 1, `cp < 0x800` -> 2, `cp < 0x10000` -> 3, else -> 4. Used by
    // `@plum_str_to_upper`/`@plum_str_to_lower`'s counting pass, where a
    // codepoint has already been mapped via `towupper`/`towlower` and its
    // resulting UTF-8 byte length (which can differ from its SOURCE
    // codepoint's length) needs classifying. Same sequential-`icmp`+`br`
    // style as `@plum_utf8_len_at`, no loop/table.
    out.push_str("define i64 @plum_utf8_encoded_len(i64 %cp) {\n");
    out.push_str("entry:\n");
    out.push_str("  %is1 = icmp ult i64 %cp, 128\n");
    out.push_str("  br i1 %is1, label %len1, label %check2\n");
    out.push_str("len1:\n");
    out.push_str("  ret i64 1\n");
    out.push_str("check2:\n");
    out.push_str("  %is2 = icmp ult i64 %cp, 2048\n");
    out.push_str("  br i1 %is2, label %len2, label %check3\n");
    out.push_str("len2:\n");
    out.push_str("  ret i64 2\n");
    out.push_str("check3:\n");
    out.push_str("  %is3 = icmp ult i64 %cp, 65536\n");
    out.push_str("  br i1 %is3, label %len3, label %len4\n");
    out.push_str("len3:\n");
    out.push_str("  ret i64 3\n");
    out.push_str("len4:\n");
    out.push_str("  ret i64 4\n");
    out.push_str("}\n\n");

    // `@plum_utf8_encode` — the inverse of `@plum_utf8_decode`: encodes
    // codepoint `%cp` as UTF-8 bytes into `%dst`, returning the number of
    // bytes written (== `@plum_utf8_encoded_len(%cp)`, computed here as a
    // side effect of the same branching rather than calling that function
    // again). Mirrors `@plum_utf8_decode`'s four-way length-classification
    // structure but WRITES/shifts-out bytes instead of reading/combining
    // them. Standard UTF-8 encoding: 1 byte plain 7-bit; 2 bytes
    // `110xxxxx 10xxxxxx`; 3 bytes `1110xxxx 10xxxxxx 10xxxxxx`; 4 bytes
    // `11110xxx 10xxxxxx 10xxxxxx 10xxxxxx`.
    out.push_str("define i64 @plum_utf8_encode(ptr %dst, i64 %cp) {\n");
    out.push_str("entry:\n");
    out.push_str("  %is1 = icmp ult i64 %cp, 128\n");
    out.push_str("  br i1 %is1, label %len1, label %check2\n");
    out.push_str("len1:\n");
    out.push_str("  %b0_1 = trunc i64 %cp to i8\n");
    out.push_str("  store i8 %b0_1, ptr %dst\n");
    out.push_str("  ret i64 1\n");
    out.push_str("check2:\n");
    out.push_str("  %is2 = icmp ult i64 %cp, 2048\n");
    out.push_str("  br i1 %is2, label %len2, label %check3\n");
    out.push_str("len2:\n");
    out.push_str("  %hi_2 = lshr i64 %cp, 6\n");
    out.push_str("  %hi_2m = or i64 %hi_2, 192\n");
    out.push_str("  %b0_2 = trunc i64 %hi_2m to i8\n");
    out.push_str("  store i8 %b0_2, ptr %dst\n");
    out.push_str("  %lo_2 = and i64 %cp, 63\n");
    out.push_str("  %lo_2m = or i64 %lo_2, 128\n");
    out.push_str("  %b1_2 = trunc i64 %lo_2m to i8\n");
    out.push_str("  %addr1_2 = getelementptr i8, ptr %dst, i64 1\n");
    out.push_str("  store i8 %b1_2, ptr %addr1_2\n");
    out.push_str("  ret i64 2\n");
    out.push_str("check3:\n");
    out.push_str("  %is3 = icmp ult i64 %cp, 65536\n");
    out.push_str("  br i1 %is3, label %len3, label %len4\n");
    out.push_str("len3:\n");
    out.push_str("  %hi_3 = lshr i64 %cp, 12\n");
    out.push_str("  %hi_3m = or i64 %hi_3, 224\n");
    out.push_str("  %b0_3 = trunc i64 %hi_3m to i8\n");
    out.push_str("  store i8 %b0_3, ptr %dst\n");
    out.push_str("  %mid_3 = lshr i64 %cp, 6\n");
    out.push_str("  %mid_3a = and i64 %mid_3, 63\n");
    out.push_str("  %mid_3m = or i64 %mid_3a, 128\n");
    out.push_str("  %b1_3 = trunc i64 %mid_3m to i8\n");
    out.push_str("  %addr1_3 = getelementptr i8, ptr %dst, i64 1\n");
    out.push_str("  store i8 %b1_3, ptr %addr1_3\n");
    out.push_str("  %lo_3 = and i64 %cp, 63\n");
    out.push_str("  %lo_3m = or i64 %lo_3, 128\n");
    out.push_str("  %b2_3 = trunc i64 %lo_3m to i8\n");
    out.push_str("  %addr2_3 = getelementptr i8, ptr %dst, i64 2\n");
    out.push_str("  store i8 %b2_3, ptr %addr2_3\n");
    out.push_str("  ret i64 3\n");
    out.push_str("len4:\n");
    out.push_str("  %hi_4 = lshr i64 %cp, 18\n");
    out.push_str("  %hi_4m = or i64 %hi_4, 240\n");
    out.push_str("  %b0_4 = trunc i64 %hi_4m to i8\n");
    out.push_str("  store i8 %b0_4, ptr %dst\n");
    out.push_str("  %mid1_4 = lshr i64 %cp, 12\n");
    out.push_str("  %mid1_4a = and i64 %mid1_4, 63\n");
    out.push_str("  %mid1_4m = or i64 %mid1_4a, 128\n");
    out.push_str("  %b1_4 = trunc i64 %mid1_4m to i8\n");
    out.push_str("  %addr1_4 = getelementptr i8, ptr %dst, i64 1\n");
    out.push_str("  store i8 %b1_4, ptr %addr1_4\n");
    out.push_str("  %mid2_4 = lshr i64 %cp, 6\n");
    out.push_str("  %mid2_4a = and i64 %mid2_4, 63\n");
    out.push_str("  %mid2_4m = or i64 %mid2_4a, 128\n");
    out.push_str("  %b2_4 = trunc i64 %mid2_4m to i8\n");
    out.push_str("  %addr2_4 = getelementptr i8, ptr %dst, i64 2\n");
    out.push_str("  store i8 %b2_4, ptr %addr2_4\n");
    out.push_str("  %lo_4 = and i64 %cp, 63\n");
    out.push_str("  %lo_4m = or i64 %lo_4, 128\n");
    out.push_str("  %b3_4 = trunc i64 %lo_4m to i8\n");
    out.push_str("  %addr3_4 = getelementptr i8, ptr %dst, i64 3\n");
    out.push_str("  store i8 %b3_4, ptr %addr3_4\n");
    out.push_str("  ret i64 4\n");
    out.push_str("}\n\n");

    // `@plum_is_unicode_whitespace` — the Unicode `White_Space` property,
    // as a fixed 25-codepoint list (matching Rust's own `char::
    // is_whitespace` exactly, the ground truth `plum-interp`'s `.trim()`
    // already delegates to): U+0009-000D, U+0020, U+0085, U+00A0,
    // U+1680, U+2000-200A, U+2028, U+2029, U+202F, U+205F, U+3000.
    // Implemented as a fixed sequence of `icmp` range/equality checks
    // combined with `or i1` — no loop, no table, cheap and exact.
    out.push_str("define i1 @plum_is_unicode_whitespace(i64 %cp) {\n");
    out.push_str("entry:\n");
    out.push_str("  %r1a = icmp sge i64 %cp, 9\n");
    out.push_str("  %r1b = icmp sle i64 %cp, 13\n");
    out.push_str("  %r1 = and i1 %r1a, %r1b\n");
    out.push_str("  %r2 = icmp eq i64 %cp, 32\n");
    out.push_str("  %r3 = icmp eq i64 %cp, 133\n");
    out.push_str("  %r4 = icmp eq i64 %cp, 160\n");
    out.push_str("  %r5 = icmp eq i64 %cp, 5760\n");
    out.push_str("  %r6a = icmp sge i64 %cp, 8192\n");
    out.push_str("  %r6b = icmp sle i64 %cp, 8202\n");
    out.push_str("  %r6 = and i1 %r6a, %r6b\n");
    out.push_str("  %r7 = icmp eq i64 %cp, 8232\n");
    out.push_str("  %r8 = icmp eq i64 %cp, 8233\n");
    out.push_str("  %r9 = icmp eq i64 %cp, 8239\n");
    out.push_str("  %r10 = icmp eq i64 %cp, 8287\n");
    out.push_str("  %r11 = icmp eq i64 %cp, 12288\n");
    out.push_str("  %o1 = or i1 %r1, %r2\n");
    out.push_str("  %o2 = or i1 %o1, %r3\n");
    out.push_str("  %o3 = or i1 %o2, %r4\n");
    out.push_str("  %o4 = or i1 %o3, %r5\n");
    out.push_str("  %o5 = or i1 %o4, %r6\n");
    out.push_str("  %o6 = or i1 %o5, %r7\n");
    out.push_str("  %o7 = or i1 %o6, %r8\n");
    out.push_str("  %o8 = or i1 %o7, %r9\n");
    out.push_str("  %o9 = or i1 %o8, %r10\n");
    out.push_str("  %o10 = or i1 %o9, %r11\n");
    out.push_str("  ret i1 %o10\n");
    out.push_str("}\n\n");

    // `@plum_str_trim_bounds` — finds the `[start, end)` byte range of
    // `%s` with leading/trailing Unicode whitespace stripped, without
    // allocating anything itself (shared by both `.trim()`'s fresh path,
    // `@plum_str_trim`, and its reuse-in-place path, `@plum_str_trim_
    // inplace`, below). Forward scan: decode from position 0, advancing
    // by each codepoint's own byte length while it's whitespace.
    // Backward scan: the standard UTF-8 "find character start scanning
    // backwards" trick — continuation bytes are all `10xxxxxx`, so scan
    // backwards from `len-1` until a NON-continuation byte (a character
    // START) is found, decode forward once from there to check
    // whitespace, and repeat character-by-character from the end.
    out.push_str("define void @plum_str_trim_bounds(ptr %s, ptr %out_start, ptr %out_end) {\n");
    out.push_str("entry:\n");
    out.push_str("  %len_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %len = load i64, ptr %len_addr\n");
    out.push_str("  %base = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  br label %fwd_check\n");
    out.push_str("fwd_check:\n");
    out.push_str("  %pos = phi i64 [ 0, %entry ], [ %pos_next, %fwd_advance ]\n");
    out.push_str("  %fwd_cont = icmp slt i64 %pos, %len\n");
    out.push_str("  br i1 %fwd_cont, label %fwd_body, label %fwd_done\n");
    out.push_str("fwd_body:\n");
    out.push_str("  %fwd_nbytes_slot = alloca i64\n");
    out.push_str("  %fwd_cp = call i64 @plum_utf8_decode(ptr %base, i64 %pos, ptr %fwd_nbytes_slot)\n");
    out.push_str("  %fwd_is_ws = call i1 @plum_is_unicode_whitespace(i64 %fwd_cp)\n");
    out.push_str("  br i1 %fwd_is_ws, label %fwd_advance, label %fwd_done\n");
    out.push_str("fwd_advance:\n");
    out.push_str("  %fwd_nbytes = load i64, ptr %fwd_nbytes_slot\n");
    out.push_str("  %pos_next = add i64 %pos, %fwd_nbytes\n");
    out.push_str("  br label %fwd_check\n");
    out.push_str("fwd_done:\n");
    out.push_str("  %start = phi i64 [ %pos, %fwd_check ], [ %pos, %fwd_body ]\n");
    out.push_str("  store i64 %start, ptr %out_start\n");
    out.push_str("  br label %bwd_check\n");
    out.push_str("bwd_check:\n");
    out.push_str("  %end = phi i64 [ %len, %fwd_done ], [ %end_next, %bwd_advance ]\n");
    out.push_str("  %bwd_cont = icmp sgt i64 %end, %start\n");
    out.push_str("  br i1 %bwd_cont, label %bwd_scan_start, label %bwd_done\n");
    out.push_str("bwd_scan_start:\n");
    out.push_str("  %last = sub i64 %end, 1\n");
    out.push_str("  br label %bwd_scan\n");
    out.push_str("bwd_scan:\n");
    out.push_str("  %scan_pos = phi i64 [ %last, %bwd_scan_start ], [ %scan_pos_next, %bwd_scan_cont ]\n");
    out.push_str("  %scan_addr = getelementptr i8, ptr %base, i64 %scan_pos\n");
    out.push_str("  %scan_byte_i8 = load i8, ptr %scan_addr\n");
    out.push_str("  %scan_byte = zext i8 %scan_byte_i8 to i64\n");
    out.push_str("  %scan_and = and i64 %scan_byte, 192\n");
    out.push_str("  %is_cont = icmp eq i64 %scan_and, 128\n");
    out.push_str("  br i1 %is_cont, label %bwd_scan_cont, label %bwd_char_start\n");
    out.push_str("bwd_scan_cont:\n");
    out.push_str("  %scan_pos_next = sub i64 %scan_pos, 1\n");
    out.push_str("  br label %bwd_scan\n");
    out.push_str("bwd_char_start:\n");
    out.push_str("  %bwd_nbytes_slot = alloca i64\n");
    out.push_str("  %bwd_cp = call i64 @plum_utf8_decode(ptr %base, i64 %scan_pos, ptr %bwd_nbytes_slot)\n");
    out.push_str("  %bwd_is_ws = call i1 @plum_is_unicode_whitespace(i64 %bwd_cp)\n");
    out.push_str("  br i1 %bwd_is_ws, label %bwd_advance, label %bwd_done\n");
    out.push_str("bwd_advance:\n");
    out.push_str("  %end_next = phi i64 [ %scan_pos, %bwd_char_start ]\n");
    out.push_str("  br label %bwd_check\n");
    out.push_str("bwd_done:\n");
    out.push_str("  %final_end = phi i64 [ %end, %bwd_check ], [ %end, %bwd_char_start ]\n");
    out.push_str("  store i64 %final_end, ptr %out_end\n");
    out.push_str("  ret void\n");
    out.push_str("}\n\n");

    // `@plum_str_trim` — the FRESH-allocation half of `.trim()`: fresh
    // `@plum_alloc_str` + `@memcpy` of just `[start, end)`, matching the
    // FFI chunk's established "fresh allocation, never alias into
    // another cell" pattern (the original cell can still be
    // independently released/mutated afterward). See `@plum_str_trim_
    // inplace` for `.trim()`'s reuse-in-place half.
    out.push_str("define ptr @plum_str_trim(ptr %s) {\n");
    out.push_str("entry:\n");
    out.push_str("  %start_slot = alloca i64\n");
    out.push_str("  %end_slot = alloca i64\n");
    out.push_str("  call void @plum_str_trim_bounds(ptr %s, ptr %start_slot, ptr %end_slot)\n");
    out.push_str("  %start = load i64, ptr %start_slot\n");
    out.push_str("  %end = load i64, ptr %end_slot\n");
    out.push_str("  %newlen = sub i64 %end, %start\n");
    out.push_str("  %cell = call ptr @plum_alloc_str(i64 %newlen)\n");
    out.push_str("  %sbase = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  %src = getelementptr i8, ptr %sbase, i64 %start\n");
    out.push_str("  %dst = getelementptr i8, ptr %cell, i64 16\n");
    out.push_str("  call ptr @memcpy(ptr %dst, ptr %src, i64 %newlen)\n");
    out.push_str("  ret ptr %cell\n");
    out.push_str("}\n\n");

    // `@plum_str_trim_inplace` — the reuse-in-place half of `.trim()`,
    // called ONLY once the caller (`codegen.rs`'s `StrTrimReuse` arm) has
    // already confirmed unique ownership via the standard refcount-
    // check-then-branch shape every other `*Reuse` string op uses.
    // Trimming only ever SHRINKS, so — unlike `.concat()`'s reuse path —
    // this needs no `@realloc` at all: just `@memmove` the trimmed range
    // down to offset 16 (overlap-safe, since `[start, end)` can overlap
    // `[0, newlen)` when `start > 0`) and update `len` + the trailing
    // NUL (see the string-cell-layout comment above `@plum_alloc_str`).
    out.push_str("define void @plum_str_trim_inplace(ptr %s) {\n");
    out.push_str("entry:\n");
    out.push_str("  %start_slot = alloca i64\n");
    out.push_str("  %end_slot = alloca i64\n");
    out.push_str("  call void @plum_str_trim_bounds(ptr %s, ptr %start_slot, ptr %end_slot)\n");
    out.push_str("  %start = load i64, ptr %start_slot\n");
    out.push_str("  %end = load i64, ptr %end_slot\n");
    out.push_str("  %newlen = sub i64 %end, %start\n");
    out.push_str("  %base = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  %src = getelementptr i8, ptr %base, i64 %start\n");
    out.push_str("  call ptr @memmove(ptr %base, ptr %src, i64 %newlen)\n");
    out.push_str("  %len_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  store i64 %newlen, ptr %len_addr\n");
    out.push_str("  %nul_addr = getelementptr i8, ptr %base, i64 %newlen\n");
    out.push_str("  store i8 0, ptr %nul_addr\n");
    out.push_str("  ret void\n");
    out.push_str("}\n\n");

    // `@plum_str_to_upper`/`@plum_str_to_lower` — real Unicode SIMPLE
    // case mapping via libc's `towupper`/`towlower` (locale-aware,
    // one-codepoint-in-one-codepoint-out — see `@plum_locale_init`
    // above for why `setlocale(LC_ALL, "C.utf8")` must run first, or
    // these silently degrade to ASCII-only "C" locale behavior).
    // Two-pass, matching `@plum_str_runes`'s established "count then
    // fill, re-decoding/re-mapping in both passes rather than caching"
    // shape exactly (cheap; avoids a scratch buffer): pass 1 walks `%s`
    // via `@plum_utf8_decode`, maps each codepoint through `towupper`/
    // `towlower`, and accumulates `@plum_utf8_encoded_len` of the
    // MAPPED codepoint (not the source one — case mapping can change a
    // character's UTF-8 byte length, e.g. ASCII `i`(1 byte) has no
    // Turkish-dotted mapping here but plenty of Latin-1 pairs cross the
    // 1-vs-2-byte boundary) into a running total; pass 2 re-walks and
    // re-maps identically, writing each mapped codepoint via
    // `@plum_utf8_encode` into the freshly `@plum_alloc_str`-ed
    // destination. Remaining, precisely-scoped gap: multi-codepoint
    // expansions (German `ß` -> `"SS"`) structurally cannot happen
    // through `towupper`/`towlower`'s 1-in-1-out C signature, so `ß`
    // stays `ß` — see DESIGN.md's "Strings" section for the language-
    // level caveat this drives. No `_inplace` reuse variants: case
    // mapping can change total byte length, the same soundness hazard
    // `StrReplaceReuse` already found and rejected for its own reuse
    // path (see `codegen.rs`'s doc comment on that arm) — `codegen.rs`'s
    // `StrToUpperReuse`/`StrToLowerReuse` arms instead call these fresh
    // functions and free the old cell directly.
    out.push_str("define ptr @plum_str_to_upper(ptr %s) {\n");
    out.push_str("entry:\n");
    out.push_str("  %slen_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %slen = load i64, ptr %slen_addr\n");
    out.push_str("  %sbase = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  br label %count_check\n");
    out.push_str("count_check:\n");
    out.push_str("  %pos1 = phi i64 [ 0, %entry ], [ %pos1_next, %count_body ]\n");
    out.push_str("  %total = phi i64 [ 0, %entry ], [ %total_next, %count_body ]\n");
    out.push_str("  %cont1 = icmp slt i64 %pos1, %slen\n");
    out.push_str("  br i1 %cont1, label %count_body, label %count_done\n");
    out.push_str("count_body:\n");
    out.push_str("  %cnbytes_slot = alloca i64\n");
    out.push_str("  %ccp = call i64 @plum_utf8_decode(ptr %sbase, i64 %pos1, ptr %cnbytes_slot)\n");
    out.push_str("  %ccp32 = trunc i64 %ccp to i32\n");
    out.push_str("  %cmapped32 = call i32 @towupper(i32 %ccp32)\n");
    out.push_str("  %cmapped = zext i32 %cmapped32 to i64\n");
    out.push_str("  %cmlen = call i64 @plum_utf8_encoded_len(i64 %cmapped)\n");
    out.push_str("  %total_next = add i64 %total, %cmlen\n");
    out.push_str("  %cnbytes = load i64, ptr %cnbytes_slot\n");
    out.push_str("  %pos1_next = add i64 %pos1, %cnbytes\n");
    out.push_str("  br label %count_check\n");
    out.push_str("count_done:\n");
    out.push_str("  %cell = call ptr @plum_alloc_str(i64 %total)\n");
    out.push_str("  %dst = getelementptr i8, ptr %cell, i64 16\n");
    out.push_str("  br label %fill_check\n");
    out.push_str("fill_check:\n");
    out.push_str("  %pos2 = phi i64 [ 0, %count_done ], [ %pos2_next, %fill_body ]\n");
    out.push_str("  %dcur = phi i64 [ 0, %count_done ], [ %dcur_next, %fill_body ]\n");
    out.push_str("  %cont2 = icmp slt i64 %pos2, %slen\n");
    out.push_str("  br i1 %cont2, label %fill_body, label %fill_done\n");
    out.push_str("fill_body:\n");
    out.push_str("  %fnbytes_slot = alloca i64\n");
    out.push_str("  %fcp = call i64 @plum_utf8_decode(ptr %sbase, i64 %pos2, ptr %fnbytes_slot)\n");
    out.push_str("  %fcp32 = trunc i64 %fcp to i32\n");
    out.push_str("  %fmapped32 = call i32 @towupper(i32 %fcp32)\n");
    out.push_str("  %fmapped = zext i32 %fmapped32 to i64\n");
    out.push_str("  %fdaddr = getelementptr i8, ptr %dst, i64 %dcur\n");
    out.push_str("  %fwritten = call i64 @plum_utf8_encode(ptr %fdaddr, i64 %fmapped)\n");
    out.push_str("  %dcur_next = add i64 %dcur, %fwritten\n");
    out.push_str("  %fnbytes = load i64, ptr %fnbytes_slot\n");
    out.push_str("  %pos2_next = add i64 %pos2, %fnbytes\n");
    out.push_str("  br label %fill_check\n");
    out.push_str("fill_done:\n");
    out.push_str("  ret ptr %cell\n");
    out.push_str("}\n\n");

    out.push_str("define ptr @plum_str_to_lower(ptr %s) {\n");
    out.push_str("entry:\n");
    out.push_str("  %slen_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %slen = load i64, ptr %slen_addr\n");
    out.push_str("  %sbase = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  br label %count_check\n");
    out.push_str("count_check:\n");
    out.push_str("  %pos1 = phi i64 [ 0, %entry ], [ %pos1_next, %count_body ]\n");
    out.push_str("  %total = phi i64 [ 0, %entry ], [ %total_next, %count_body ]\n");
    out.push_str("  %cont1 = icmp slt i64 %pos1, %slen\n");
    out.push_str("  br i1 %cont1, label %count_body, label %count_done\n");
    out.push_str("count_body:\n");
    out.push_str("  %cnbytes_slot = alloca i64\n");
    out.push_str("  %ccp = call i64 @plum_utf8_decode(ptr %sbase, i64 %pos1, ptr %cnbytes_slot)\n");
    out.push_str("  %ccp32 = trunc i64 %ccp to i32\n");
    out.push_str("  %cmapped32 = call i32 @towlower(i32 %ccp32)\n");
    out.push_str("  %cmapped = zext i32 %cmapped32 to i64\n");
    out.push_str("  %cmlen = call i64 @plum_utf8_encoded_len(i64 %cmapped)\n");
    out.push_str("  %total_next = add i64 %total, %cmlen\n");
    out.push_str("  %cnbytes = load i64, ptr %cnbytes_slot\n");
    out.push_str("  %pos1_next = add i64 %pos1, %cnbytes\n");
    out.push_str("  br label %count_check\n");
    out.push_str("count_done:\n");
    out.push_str("  %cell = call ptr @plum_alloc_str(i64 %total)\n");
    out.push_str("  %dst = getelementptr i8, ptr %cell, i64 16\n");
    out.push_str("  br label %fill_check\n");
    out.push_str("fill_check:\n");
    out.push_str("  %pos2 = phi i64 [ 0, %count_done ], [ %pos2_next, %fill_body ]\n");
    out.push_str("  %dcur = phi i64 [ 0, %count_done ], [ %dcur_next, %fill_body ]\n");
    out.push_str("  %cont2 = icmp slt i64 %pos2, %slen\n");
    out.push_str("  br i1 %cont2, label %fill_body, label %fill_done\n");
    out.push_str("fill_body:\n");
    out.push_str("  %fnbytes_slot = alloca i64\n");
    out.push_str("  %fcp = call i64 @plum_utf8_decode(ptr %sbase, i64 %pos2, ptr %fnbytes_slot)\n");
    out.push_str("  %fcp32 = trunc i64 %fcp to i32\n");
    out.push_str("  %fmapped32 = call i32 @towlower(i32 %fcp32)\n");
    out.push_str("  %fmapped = zext i32 %fmapped32 to i64\n");
    out.push_str("  %fdaddr = getelementptr i8, ptr %dst, i64 %dcur\n");
    out.push_str("  %fwritten = call i64 @plum_utf8_encode(ptr %fdaddr, i64 %fmapped)\n");
    out.push_str("  %dcur_next = add i64 %dcur, %fwritten\n");
    out.push_str("  %fnbytes = load i64, ptr %fnbytes_slot\n");
    out.push_str("  %pos2_next = add i64 %pos2, %fnbytes\n");
    out.push_str("  br label %fill_check\n");
    out.push_str("fill_done:\n");
    out.push_str("  ret ptr %cell\n");
    out.push_str("}\n\n");

    // `@plum_str_count_matches` — extends `@plum_str_contains`'s
    // existing double-loop precedent, advancing PAST a full match
    // (`%start + %nlen`) rather than stopping at the first one, so it
    // counts non-overlapping matches. Shared by `.replace()`'s
    // non-empty-`from` length computation and `.split()`'s non-empty-
    // `sep` piece-count computation (`piece_count = match_count + 1`).
    // An empty `%needle` returns 0 unconditionally — never actually
    // reached by either of this function's two current callers (both
    // guard the empty case themselves before calling in), but kept for
    // this function to stay correct on its own for ANY input, not just
    // today's call sites.
    out.push_str("define i64 @plum_str_count_matches(ptr %s, ptr %needle) {\n");
    out.push_str("entry:\n");
    out.push_str("  %slen_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %slen = load i64, ptr %slen_addr\n");
    out.push_str("  %nlen_addr = getelementptr i8, ptr %needle, i64 8\n");
    out.push_str("  %nlen = load i64, ptr %nlen_addr\n");
    out.push_str("  %is_empty_needle = icmp eq i64 %nlen, 0\n");
    out.push_str("  br i1 %is_empty_needle, label %zero_result, label %check_fits\n");
    out.push_str("zero_result:\n");
    out.push_str("  ret i64 0\n");
    out.push_str("check_fits:\n");
    out.push_str("  %fits = icmp sge i64 %slen, %nlen\n");
    out.push_str("  br i1 %fits, label %outer_init, label %no_match_return\n");
    out.push_str("no_match_return:\n");
    out.push_str("  ret i64 0\n");
    out.push_str("outer_init:\n");
    out.push_str("  %max_start = sub i64 %slen, %nlen\n");
    out.push_str("  br label %outer\n");
    out.push_str("outer:\n");
    out.push_str("  %start = phi i64 [ 0, %outer_init ], [ %start_next_m, %match_advance ], [ %start_next_x, %mismatch_advance ]\n");
    out.push_str("  %count = phi i64 [ 0, %outer_init ], [ %count_next, %match_advance ], [ %count, %mismatch_advance ]\n");
    out.push_str("  %outer_done = icmp sgt i64 %start, %max_start\n");
    out.push_str("  br i1 %outer_done, label %finished, label %inner_init\n");
    out.push_str("inner_init:\n");
    out.push_str("  br label %inner\n");
    out.push_str("inner:\n");
    out.push_str("  %j = phi i64 [ 0, %inner_init ], [ %j_next, %inner_cont ]\n");
    out.push_str("  %inner_done = icmp eq i64 %j, %nlen\n");
    out.push_str("  br i1 %inner_done, label %found, label %inner_check\n");
    out.push_str("inner_check:\n");
    out.push_str("  %s_idx = add i64 %start, %j\n");
    out.push_str("  %s_base = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  %s_addr = getelementptr i8, ptr %s_base, i64 %s_idx\n");
    out.push_str("  %s_byte = load i8, ptr %s_addr\n");
    out.push_str("  %n_base = getelementptr i8, ptr %needle, i64 16\n");
    out.push_str("  %n_addr = getelementptr i8, ptr %n_base, i64 %j\n");
    out.push_str("  %n_byte = load i8, ptr %n_addr\n");
    out.push_str("  %beq = icmp eq i8 %s_byte, %n_byte\n");
    out.push_str("  br i1 %beq, label %inner_cont, label %mismatch_advance\n");
    out.push_str("inner_cont:\n");
    out.push_str("  %j_next = add i64 %j, 1\n");
    out.push_str("  br label %inner\n");
    out.push_str("found:\n");
    out.push_str("  br label %match_advance\n");
    out.push_str("match_advance:\n");
    out.push_str("  %count_next = add i64 %count, 1\n");
    out.push_str("  %start_next_m = add i64 %start, %nlen\n");
    out.push_str("  br label %outer\n");
    out.push_str("mismatch_advance:\n");
    out.push_str("  %start_next_x = add i64 %start, 1\n");
    out.push_str("  br label %outer\n");
    out.push_str("finished:\n");
    out.push_str("  ret i64 %count\n");
    out.push_str("}\n\n");

    // `@plum_str_replace` — the FRESH-allocation whole of `.replace()`
    // (both the ordinary and `Reuse` IR nodes call this one function;
    // see `codegen.rs`'s `StrReplaceReuse` arm doc comment for why the
    // reuse path does NOT attempt a true realloc-in-place transform).
    // Two-pass for non-empty `%from` (byte lengths of `from`/`to` can
    // differ arbitrarily — count matches first via `@plum_str_count_
    // matches`, size the one allocation exactly, then a SINGLE re-walk
    // copying either one literal byte or all of `to`'s bytes at each
    // position). Empty `%from`: confirmed empirically (a real `rustc`
    // scratch run during design, not assumed) to use the SAME char-
    // boundary insertion logic as `.split("")` — `to` gets inserted at
    // EVERY character boundary, N+1 times for an N-character string.
    out.push_str("define ptr @plum_str_replace(ptr %s, ptr %from, ptr %to) {\n");
    out.push_str("entry:\n");
    out.push_str("  %fromlen_addr = getelementptr i8, ptr %from, i64 8\n");
    out.push_str("  %fromlen = load i64, ptr %fromlen_addr\n");
    out.push_str("  %tolen_addr = getelementptr i8, ptr %to, i64 8\n");
    out.push_str("  %tolen = load i64, ptr %tolen_addr\n");
    out.push_str("  %slen_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %slen = load i64, ptr %slen_addr\n");
    out.push_str("  %sbase = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  %tobase = getelementptr i8, ptr %to, i64 16\n");
    out.push_str("  %is_empty_from = icmp eq i64 %fromlen, 0\n");
    out.push_str("  br i1 %is_empty_from, label %empty_from_path, label %nonempty_from_path\n");
    // --- non-empty `from`: count matches, size exactly, single re-walk ---
    out.push_str("nonempty_from_path:\n");
    out.push_str("  %frombase = getelementptr i8, ptr %from, i64 16\n");
    out.push_str("  %match_count = call i64 @plum_str_count_matches(ptr %s, ptr %from)\n");
    out.push_str("  %delta = sub i64 %tolen, %fromlen\n");
    out.push_str("  %grow = mul i64 %match_count, %delta\n");
    out.push_str("  %newlen = add i64 %slen, %grow\n");
    out.push_str("  %cell = call ptr @plum_alloc_str(i64 %newlen)\n");
    out.push_str("  %dbase = getelementptr i8, ptr %cell, i64 16\n");
    out.push_str("  br label %scan_check\n");
    out.push_str("scan_check:\n");
    out.push_str("  %spos = phi i64 [ 0, %nonempty_from_path ], [ %spos_next, %after_copy ]\n");
    out.push_str("  %dpos = phi i64 [ 0, %nonempty_from_path ], [ %dpos_next, %after_copy ]\n");
    out.push_str("  %scan_cont = icmp slt i64 %spos, %slen\n");
    out.push_str("  br i1 %scan_cont, label %try_match, label %scan_done\n");
    out.push_str("try_match:\n");
    out.push_str("  %remaining = sub i64 %slen, %spos\n");
    out.push_str("  %fits = icmp sge i64 %remaining, %fromlen\n");
    out.push_str("  br i1 %fits, label %match_check_loop_init, label %copy_one_byte\n");
    out.push_str("match_check_loop_init:\n");
    out.push_str("  br label %match_check_loop\n");
    out.push_str("match_check_loop:\n");
    out.push_str("  %j = phi i64 [ 0, %match_check_loop_init ], [ %j_next, %match_check_cont ]\n");
    out.push_str("  %j_done = icmp eq i64 %j, %fromlen\n");
    out.push_str("  br i1 %j_done, label %is_match, label %match_check_body\n");
    out.push_str("match_check_body:\n");
    out.push_str("  %s_idx = add i64 %spos, %j\n");
    out.push_str("  %s_addr = getelementptr i8, ptr %sbase, i64 %s_idx\n");
    out.push_str("  %s_byte = load i8, ptr %s_addr\n");
    out.push_str("  %f_addr = getelementptr i8, ptr %frombase, i64 %j\n");
    out.push_str("  %f_byte = load i8, ptr %f_addr\n");
    out.push_str("  %beq = icmp eq i8 %s_byte, %f_byte\n");
    out.push_str("  br i1 %beq, label %match_check_cont, label %copy_one_byte\n");
    out.push_str("match_check_cont:\n");
    out.push_str("  %j_next = add i64 %j, 1\n");
    out.push_str("  br label %match_check_loop\n");
    out.push_str("is_match:\n");
    out.push_str("  %ddst_m = getelementptr i8, ptr %dbase, i64 %dpos\n");
    out.push_str("  call ptr @memcpy(ptr %ddst_m, ptr %tobase, i64 %tolen)\n");
    out.push_str("  %spos_next_m = add i64 %spos, %fromlen\n");
    out.push_str("  %dpos_next_m = add i64 %dpos, %tolen\n");
    out.push_str("  br label %after_copy\n");
    out.push_str("copy_one_byte:\n");
    out.push_str("  %s1addr = getelementptr i8, ptr %sbase, i64 %spos\n");
    out.push_str("  %s1byte = load i8, ptr %s1addr\n");
    out.push_str("  %d1addr = getelementptr i8, ptr %dbase, i64 %dpos\n");
    out.push_str("  store i8 %s1byte, ptr %d1addr\n");
    out.push_str("  %spos_next_b = add i64 %spos, 1\n");
    out.push_str("  %dpos_next_b = add i64 %dpos, 1\n");
    out.push_str("  br label %after_copy\n");
    out.push_str("after_copy:\n");
    out.push_str("  %spos_next = phi i64 [ %spos_next_m, %is_match ], [ %spos_next_b, %copy_one_byte ]\n");
    out.push_str("  %dpos_next = phi i64 [ %dpos_next_m, %is_match ], [ %dpos_next_b, %copy_one_byte ]\n");
    out.push_str("  br label %scan_check\n");
    out.push_str("scan_done:\n");
    out.push_str("  ret ptr %cell\n");
    // --- empty `from`: char-boundary insertion, N+1 times ---
    out.push_str("empty_from_path:\n");
    out.push_str("  br label %ecount_check\n");
    out.push_str("ecount_check:\n");
    out.push_str("  %epos = phi i64 [ 0, %empty_from_path ], [ %epos_next, %ecount_body ]\n");
    out.push_str("  %echars = phi i64 [ 0, %empty_from_path ], [ %echars_next, %ecount_body ]\n");
    out.push_str("  %econt = icmp slt i64 %epos, %slen\n");
    out.push_str("  br i1 %econt, label %ecount_body, label %ecount_done\n");
    out.push_str("ecount_body:\n");
    out.push_str("  %eclen = call i64 @plum_utf8_len_at(ptr %sbase, i64 %epos)\n");
    out.push_str("  %epos_next = add i64 %epos, %eclen\n");
    out.push_str("  %echars_next = add i64 %echars, 1\n");
    out.push_str("  br label %ecount_check\n");
    out.push_str("ecount_done:\n");
    out.push_str("  %pieces = add i64 %echars, 1\n");
    out.push_str("  %insert_bytes = mul i64 %pieces, %tolen\n");
    out.push_str("  %newlen2 = add i64 %slen, %insert_bytes\n");
    out.push_str("  %cell2 = call ptr @plum_alloc_str(i64 %newlen2)\n");
    out.push_str("  %dbase2 = getelementptr i8, ptr %cell2, i64 16\n");
    out.push_str("  br label %efill_check\n");
    out.push_str("efill_check:\n");
    out.push_str("  %fpos = phi i64 [ 0, %ecount_done ], [ %fpos_next, %efill_body ]\n");
    out.push_str("  %fdst = phi i64 [ 0, %ecount_done ], [ %fdst_next, %efill_body ]\n");
    out.push_str("  %ins_dst = getelementptr i8, ptr %dbase2, i64 %fdst\n");
    out.push_str("  call ptr @memcpy(ptr %ins_dst, ptr %tobase, i64 %tolen)\n");
    out.push_str("  %fdst_ins = add i64 %fdst, %tolen\n");
    out.push_str("  %fcont = icmp slt i64 %fpos, %slen\n");
    out.push_str("  br i1 %fcont, label %efill_body, label %efill_done\n");
    out.push_str("efill_body:\n");
    out.push_str("  %flen = call i64 @plum_utf8_len_at(ptr %sbase, i64 %fpos)\n");
    out.push_str("  %csrc = getelementptr i8, ptr %sbase, i64 %fpos\n");
    out.push_str("  %cdst = getelementptr i8, ptr %dbase2, i64 %fdst_ins\n");
    out.push_str("  call ptr @memcpy(ptr %cdst, ptr %csrc, i64 %flen)\n");
    out.push_str("  %fpos_next = add i64 %fpos, %flen\n");
    out.push_str("  %fdst_next = add i64 %fdst_ins, %flen\n");
    out.push_str("  br label %efill_check\n");
    out.push_str("efill_done:\n");
    out.push_str("  ret ptr %cell2\n");
    out.push_str("}\n\n");

    // `@plum_str_runes` — decodes `%s`'s UTF-8 bytes into one `Int`
    // codepoint per Unicode scalar value, building an `Array[Int]` cell
    // directly (element words are plain `i64` codepoints, no `CgType`-
    // aware conversion needed — see `store_array_elem`'s own doc comment
    // for why `Int` needs none). Two-pass, matching every other new op
    // here that can't know its own final size without a scan: pass 1
    // counts codepoints via `@plum_utf8_len_at` (cheaper than a full
    // decode — the codepoint VALUE isn't needed yet); pass 2 re-walks
    // via the full `@plum_utf8_decode`, storing each codepoint directly.
    out.push_str("define ptr @plum_str_runes(ptr %s) {\n");
    out.push_str("entry:\n");
    out.push_str("  %slen_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %slen = load i64, ptr %slen_addr\n");
    out.push_str("  %sbase = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  br label %count_check\n");
    out.push_str("count_check:\n");
    out.push_str("  %pos1 = phi i64 [ 0, %entry ], [ %pos1_next, %count_body ]\n");
    out.push_str("  %count = phi i64 [ 0, %entry ], [ %count_next, %count_body ]\n");
    out.push_str("  %cont1 = icmp slt i64 %pos1, %slen\n");
    out.push_str("  br i1 %cont1, label %count_body, label %count_done\n");
    out.push_str("count_body:\n");
    out.push_str("  %clen = call i64 @plum_utf8_len_at(ptr %sbase, i64 %pos1)\n");
    out.push_str("  %pos1_next = add i64 %pos1, %clen\n");
    out.push_str("  %count_next = add i64 %count, 1\n");
    out.push_str("  br label %count_check\n");
    out.push_str("count_done:\n");
    out.push_str("  %arr = call ptr @plum_alloc_array(i64 %count)\n");
    out.push_str("  br label %fill_check\n");
    out.push_str("fill_check:\n");
    out.push_str("  %pos2 = phi i64 [ 0, %count_done ], [ %pos2_next, %fill_body ]\n");
    out.push_str("  %idx = phi i64 [ 0, %count_done ], [ %idx_next, %fill_body ]\n");
    out.push_str("  %cont2 = icmp slt i64 %pos2, %slen\n");
    out.push_str("  br i1 %cont2, label %fill_body, label %fill_done\n");
    out.push_str("fill_body:\n");
    out.push_str("  %nbytes_slot = alloca i64\n");
    out.push_str("  %cp = call i64 @plum_utf8_decode(ptr %sbase, i64 %pos2, ptr %nbytes_slot)\n");
    out.push_str("  %off = mul i64 %idx, 8\n");
    out.push_str("  %byteoff = add i64 %off, 16\n");
    out.push_str("  %eaddr = getelementptr i8, ptr %arr, i64 %byteoff\n");
    out.push_str("  store i64 %cp, ptr %eaddr\n");
    out.push_str("  %nbytes = load i64, ptr %nbytes_slot\n");
    out.push_str("  %pos2_next = add i64 %pos2, %nbytes\n");
    out.push_str("  %idx_next = add i64 %idx, 1\n");
    out.push_str("  br label %fill_check\n");
    out.push_str("fill_done:\n");
    out.push_str("  ret ptr %arr\n");
    out.push_str("}\n\n");

    // `@plum_str_split` — builds `Array[Str]`, two-pass for the SAME
    // "final piece count unknowable without a scan" reason `.runes()`
    // is two-pass; deliberately not a growable-array design (see this
    // chunk's own design notes for why fabricating a generic "grow an
    // array of `Str` pointers by one, refcount-safely" primitive for
    // this single caller would be more machinery than two-pass). Runtime
    // branch on `%seplen == 0` (since `%sep` is an arbitrary expression,
    // not necessarily a literal) selects between two genuinely different
    // algorithms:
    //  - non-empty `%sep`: pass 1 reuses `@plum_str_count_matches`
    //    (`piece_count = match_count + 1`); pass 2 walks `%s`, cutting a
    //    fresh `@plum_alloc_str`+`@memcpy` piece at each match and once
    //    more for the tail after the last match.
    //  - empty `%sep`: a char-boundary walk reusing `@plum_utf8_len_at`
    //    (`.runes()`'s own counting loop) — `piece_count = char_count +
    //    2` (empty leading and trailing pieces), confirmed via a real
    //    `rustc` run during design: `"café".split("") ==
    //    ["", "c", "a", "f", "é", ""]`.
    out.push_str("define ptr @plum_str_split(ptr %s, ptr %sep) {\n");
    out.push_str("entry:\n");
    out.push_str("  %slen_addr = getelementptr i8, ptr %s, i64 8\n");
    out.push_str("  %slen = load i64, ptr %slen_addr\n");
    out.push_str("  %seplen_addr = getelementptr i8, ptr %sep, i64 8\n");
    out.push_str("  %seplen = load i64, ptr %seplen_addr\n");
    out.push_str("  %sbase = getelementptr i8, ptr %s, i64 16\n");
    out.push_str("  %is_empty_sep = icmp eq i64 %seplen, 0\n");
    out.push_str("  br i1 %is_empty_sep, label %empty_sep_path, label %nonempty_sep_path\n");
    // --- non-empty sep ---
    out.push_str("nonempty_sep_path:\n");
    out.push_str("  %sepbase = getelementptr i8, ptr %sep, i64 16\n");
    out.push_str("  %match_count = call i64 @plum_str_count_matches(ptr %s, ptr %sep)\n");
    out.push_str("  %piece_count = add i64 %match_count, 1\n");
    out.push_str("  %arr = call ptr @plum_alloc_array(i64 %piece_count)\n");
    out.push_str("  %max_start = sub i64 %slen, %seplen\n");
    out.push_str("  br label %cut_outer\n");
    out.push_str("cut_outer:\n");
    out.push_str("  %cursor = phi i64 [ 0, %nonempty_sep_path ], [ %cursor_next, %found_piece ]\n");
    out.push_str("  %idx = phi i64 [ 0, %nonempty_sep_path ], [ %idx_next, %found_piece ]\n");
    out.push_str("  br label %find_loop\n");
    out.push_str("find_loop:\n");
    out.push_str("  %fstart = phi i64 [ %cursor, %cut_outer ], [ %fstart_next, %mismatch_advance ]\n");
    out.push_str("  %search_ok = icmp sle i64 %fstart, %max_start\n");
    out.push_str("  br i1 %search_ok, label %try_pos, label %last_piece\n");
    out.push_str("try_pos:\n");
    out.push_str("  br label %pos_check_loop\n");
    out.push_str("pos_check_loop:\n");
    out.push_str("  %pj = phi i64 [ 0, %try_pos ], [ %pj_next, %pos_check_cont ]\n");
    out.push_str("  %pj_done = icmp eq i64 %pj, %seplen\n");
    out.push_str("  br i1 %pj_done, label %found_at_pos, label %pos_check_body\n");
    out.push_str("pos_check_body:\n");
    out.push_str("  %ps_idx = add i64 %fstart, %pj\n");
    out.push_str("  %ps_addr = getelementptr i8, ptr %sbase, i64 %ps_idx\n");
    out.push_str("  %ps_byte = load i8, ptr %ps_addr\n");
    out.push_str("  %pn_addr = getelementptr i8, ptr %sepbase, i64 %pj\n");
    out.push_str("  %pn_byte = load i8, ptr %pn_addr\n");
    out.push_str("  %peq = icmp eq i8 %ps_byte, %pn_byte\n");
    out.push_str("  br i1 %peq, label %pos_check_cont, label %mismatch_advance\n");
    out.push_str("pos_check_cont:\n");
    out.push_str("  %pj_next = add i64 %pj, 1\n");
    out.push_str("  br label %pos_check_loop\n");
    out.push_str("found_at_pos:\n");
    out.push_str("  %piecelen = sub i64 %fstart, %cursor\n");
    out.push_str("  %piece_cell = call ptr @plum_alloc_str(i64 %piecelen)\n");
    out.push_str("  %piece_src = getelementptr i8, ptr %sbase, i64 %cursor\n");
    out.push_str("  %piece_dst = getelementptr i8, ptr %piece_cell, i64 16\n");
    out.push_str("  call ptr @memcpy(ptr %piece_dst, ptr %piece_src, i64 %piecelen)\n");
    out.push_str("  %pword = ptrtoint ptr %piece_cell to i64\n");
    out.push_str("  %poff = mul i64 %idx, 8\n");
    out.push_str("  %pbyteoff = add i64 %poff, 16\n");
    out.push_str("  %paddr = getelementptr i8, ptr %arr, i64 %pbyteoff\n");
    out.push_str("  store i64 %pword, ptr %paddr\n");
    out.push_str("  br label %found_piece\n");
    out.push_str("found_piece:\n");
    out.push_str("  %cursor_next = add i64 %fstart, %seplen\n");
    out.push_str("  %idx_next = add i64 %idx, 1\n");
    out.push_str("  br label %cut_outer\n");
    out.push_str("mismatch_advance:\n");
    out.push_str("  %fstart_next = add i64 %fstart, 1\n");
    out.push_str("  br label %find_loop\n");
    out.push_str("last_piece:\n");
    out.push_str("  %lastlen = sub i64 %slen, %cursor\n");
    out.push_str("  %last_cell = call ptr @plum_alloc_str(i64 %lastlen)\n");
    out.push_str("  %last_src = getelementptr i8, ptr %sbase, i64 %cursor\n");
    out.push_str("  %last_dst = getelementptr i8, ptr %last_cell, i64 16\n");
    out.push_str("  call ptr @memcpy(ptr %last_dst, ptr %last_src, i64 %lastlen)\n");
    out.push_str("  %lword = ptrtoint ptr %last_cell to i64\n");
    out.push_str("  %loff = mul i64 %idx, 8\n");
    out.push_str("  %lbyteoff = add i64 %loff, 16\n");
    out.push_str("  %laddr = getelementptr i8, ptr %arr, i64 %lbyteoff\n");
    out.push_str("  store i64 %lword, ptr %laddr\n");
    out.push_str("  ret ptr %arr\n");
    // --- empty sep: char-boundary split, N+2 pieces ---
    out.push_str("empty_sep_path:\n");
    out.push_str("  %empty0 = call ptr @plum_alloc_str(i64 0)\n");
    out.push_str("  br label %ecount_check2\n");
    out.push_str("ecount_check2:\n");
    out.push_str("  %epos2 = phi i64 [ 0, %empty_sep_path ], [ %epos2_next, %ecount_body2 ]\n");
    out.push_str("  %echars2 = phi i64 [ 0, %empty_sep_path ], [ %echars2_next, %ecount_body2 ]\n");
    out.push_str("  %econt2 = icmp slt i64 %epos2, %slen\n");
    out.push_str("  br i1 %econt2, label %ecount_body2, label %ecount_done2\n");
    out.push_str("ecount_body2:\n");
    out.push_str("  %eclen2 = call i64 @plum_utf8_len_at(ptr %sbase, i64 %epos2)\n");
    out.push_str("  %epos2_next = add i64 %epos2, %eclen2\n");
    out.push_str("  %echars2_next = add i64 %echars2, 1\n");
    out.push_str("  br label %ecount_check2\n");
    out.push_str("ecount_done2:\n");
    out.push_str("  %piece_count2 = add i64 %echars2, 2\n");
    out.push_str("  %arr2 = call ptr @plum_alloc_array(i64 %piece_count2)\n");
    out.push_str("  %word0 = ptrtoint ptr %empty0 to i64\n");
    out.push_str("  %addr0 = getelementptr i8, ptr %arr2, i64 16\n");
    out.push_str("  store i64 %word0, ptr %addr0\n");
    out.push_str("  br label %esplit_check\n");
    out.push_str("esplit_check:\n");
    out.push_str("  %epos3 = phi i64 [ 0, %ecount_done2 ], [ %epos3_next, %esplit_body ]\n");
    out.push_str("  %eidx3 = phi i64 [ 1, %ecount_done2 ], [ %eidx3_next, %esplit_body ]\n");
    out.push_str("  %econt3 = icmp slt i64 %epos3, %slen\n");
    out.push_str("  br i1 %econt3, label %esplit_body, label %esplit_tail\n");
    out.push_str("esplit_body:\n");
    out.push_str("  %eclen3 = call i64 @plum_utf8_len_at(ptr %sbase, i64 %epos3)\n");
    out.push_str("  %echar_cell = call ptr @plum_alloc_str(i64 %eclen3)\n");
    out.push_str("  %echar_src = getelementptr i8, ptr %sbase, i64 %epos3\n");
    out.push_str("  %echar_dst = getelementptr i8, ptr %echar_cell, i64 16\n");
    out.push_str("  call ptr @memcpy(ptr %echar_dst, ptr %echar_src, i64 %eclen3)\n");
    out.push_str("  %eword3 = ptrtoint ptr %echar_cell to i64\n");
    out.push_str("  %eoff3 = mul i64 %eidx3, 8\n");
    out.push_str("  %ebyteoff3 = add i64 %eoff3, 16\n");
    out.push_str("  %eaddr3 = getelementptr i8, ptr %arr2, i64 %ebyteoff3\n");
    out.push_str("  store i64 %eword3, ptr %eaddr3\n");
    out.push_str("  %epos3_next = add i64 %epos3, %eclen3\n");
    out.push_str("  %eidx3_next = add i64 %eidx3, 1\n");
    out.push_str("  br label %esplit_check\n");
    out.push_str("esplit_tail:\n");
    out.push_str("  %empty_tail = call ptr @plum_alloc_str(i64 0)\n");
    out.push_str("  %tword3 = ptrtoint ptr %empty_tail to i64\n");
    out.push_str("  %toff3 = mul i64 %eidx3, 8\n");
    out.push_str("  %tbyteoff3 = add i64 %toff3, 16\n");
    out.push_str("  %taddr3 = getelementptr i8, ptr %arr2, i64 %tbyteoff3\n");
    out.push_str("  store i64 %tword3, ptr %taddr3\n");
    out.push_str("  ret ptr %arr2\n");
    out.push_str("}\n\n");

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
        // A `CStr` is never deep-copied for the SAME reason it's
        // rejected from crossing a spawn/channel boundary at all (see
        // `check_no_closure_or_task_fields`'s doc comment) — this arm
        // only exists to keep `deepcopy_fn_for` TOTAL, matching
        // `Closure`/`Task`'s own "never actually reached by any live
        // call" precedent immediately above.
        CgType::CStr => None,
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
            // `CStr` joins `Closure`/`Task` here for a related but
            // distinct reason: a raw, unowned, unsynchronized C pointer
            // aliased across two threads with no shared-ownership
            // protocol at all is strictly WORSE than a `Task`/`Closure`
            // capture (see `CgType::CStr`'s own doc comment) — so it's
            // rejected the same way. In practice this arm is currently
            // unreachable via the ordinary frontend (`plum-types`
            // resolves an ordinary struct field's type annotation
            // through `ast_type_to_type`, which never produces
            // `Type::CStr` at all — only `.as_cstr()`'s own special-
            // cased extern-signature resolution does — so `CStr` can
            // never actually appear in a declared struct/enum field's
            // type), but kept here anyway as defense-in-depth, matching
            // `Task`/`Sender`/`Receiver`'s own established "total over
            // `CgType`, even where a live call is impossible" precedent
            // elsewhere in this module (see e.g. `CgType::mangled`'s
            // `Task`/`Sender` arms).
            if matches!(field_ty, CgType::Closure(..) | CgType::Task(_) | CgType::CStr) {
                return Err(format!(
                    "codegen: struct/enum {tag:?}'s field {i} is closure/task/CStr-shaped ({field_ty:?}) — a \
                     program that uses `spawn` anywhere cannot declare a struct/enum with a closure-, task-, or \
                     CStr-typed field anywhere else either, since such a value could reach a `spawn` \
                     capture through an opaque heap pointer and none of a closure's captured environment, a \
                     task handle, or a raw C string pointer can cross a thread boundary safely (matching the \
                     interpreter's own closure/task restriction — see `plum_interp::Interpreter::to_portable` \
                     — and extending it to `CStr` for the same reason)"
                ));
            }
        }
    }
    Ok(())
}

/// Maps an `ir::ExternType` to the LLVM type used AT THE C ABI BOUNDARY
/// for it — deliberately NOT the same as `CgType::llvm_type()` for
/// `Bool`: C's own `int` is `i32`-wide, while this backend's OWN
/// `CgType::Bool` is `i1` — a real, load-bearing width mismatch that
/// must be bridged via an explicit `zext`/`icmp ne` conversion at every
/// marshaling site (`codegen.rs`'s `ExternCall`/callback-trampoline
/// codegen), never treated as if the two representations were
/// interchangeable. `Struct(name, _)` maps to a bare reference to the
/// named LLVM aggregate type (`%struct.<name>`) — see `collect_extern_
/// struct_types`/`emit_extern_struct_types` for where that type's own
/// `type { ... }` definition gets emitted; named LLVM struct types
/// resolve by name at module scope, so this reference is valid
/// regardless of whether its own `type` line has been emitted yet in
/// the final text (verified directly against a real `clang` compile
/// during planning, not assumed). Returns an owned `String` (not
/// `&'static str`, unlike every other arm) purely because `%struct.
/// <name>` is only known at the call site, not a fixed compile-time
/// constant the way `"i64"`/`"double"`/`"ptr"` are.
fn extern_type_to_llvm(ty: &ir::ExternType) -> Result<String, String> {
    match ty {
        ir::ExternType::Int => Ok("i64".to_string()),
        ir::ExternType::Float => Ok("double".to_string()),
        ir::ExternType::Bool => Ok("i32".to_string()),
        // A validated C string (`Str` here means `ir::ExternType::Str`,
        // i.e. `CStr` on the Plum side — see that variant's own doc
        // comment) and a callback are both bare pointers at the C ABI
        // boundary.
        ir::ExternType::Str | ir::ExternType::Callback { .. } => Ok("ptr".to_string()),
        ir::ExternType::Struct(name, _) => Ok(format!("%struct.{name}")),
    }
}

/// The C ABI return-type counterpart to `extern_type_to_llvm` — `None`
/// (a genuinely `void`-returning C function, e.g. `extern "C" { fn
/// puts(s: CStr); }` with no `-> Type` written) maps to LLVM `void`,
/// deliberately NOT defaulted to `Unit`'s `i1` the way an ordinary Plum
/// function always has SOME return value — a real `void` C function has
/// nothing to hand back at all.
fn extern_ret_type_to_llvm(ret: &Option<ir::ExternType>) -> Result<String, String> {
    match ret {
        None => Ok("void".to_string()),
        Some(ty) => extern_type_to_llvm(ty),
    }
}

/// Walks every extern's param/return types once, collecting every
/// distinct `ExternType::Struct` shape (recursing into each struct's own
/// fields, so a nested struct — e.g. `Outer { inner: Inner, .. }` — gets
/// its own entry too) into a name -> declared-field-types map. A
/// `BTreeMap`, not a `HashMap`: iteration order feeds directly into
/// `emit_extern_struct_types`'s emitted `type` line order, and sorted-
/// by-name output is reproducible across runs, matching `intern_tags`'
/// own "sort purely for reproducible `.ll` output" precedent. Dedup is
/// free (a struct used as both an argument and a return type across
/// different externs, or reachable via two different nesting paths,
/// only ever gets ONE entry) — `check_ffi_safe` already guarantees a
/// struct name always resolves to the same field-type shape everywhere
/// it appears, so silently keeping the first-seen entry for an
/// already-known name is safe, not just convenient. Mirrors scalar
/// FFI's own non-reactive `emit_extern_declares`: `program.externs` is
/// already the complete, explicit list, so no reactive discovery (like
/// `needed_arrays`) is needed here either.
fn collect_extern_struct_types(externs: &[ir::ExternFn]) -> BTreeMap<String, Vec<ir::ExternType>> {
    fn walk(ty: &ir::ExternType, out: &mut BTreeMap<String, Vec<ir::ExternType>>) {
        match ty {
            ir::ExternType::Struct(name, field_types) => {
                if out.contains_key(name) {
                    return;
                }
                out.insert(name.clone(), field_types.clone());
                for f in field_types {
                    walk(f, out);
                }
            }
            // A callback's own params/return can never legitimately be
            // struct-shaped (`plum_ir::lower::resolve_extern_type_inner`
            // only ever recurses into a struct name lookup that itself
            // requires a real declared struct — a callback's OWN scope
            // is narrower, Int/Float/Bool only, per that function's own
            // doc comment) — walked anyway, defensively, purely so this
            // function stays correct even if that restriction ever
            // loosens, at zero cost for every program reachable today.
            ir::ExternType::Callback { params, ret } => {
                for p in params {
                    walk(p, out);
                }
                if let Some(r) = ret {
                    walk(r, out);
                }
            }
            ir::ExternType::Int | ir::ExternType::Float | ir::ExternType::Bool | ir::ExternType::Str => {}
        }
    }
    let mut out = BTreeMap::new();
    for f in externs {
        for p in &f.param_types {
            walk(p, &mut out);
        }
        if let Some(r) = &f.ret_type {
            walk(r, &mut out);
        }
    }
    out
}

/// Emits one `%struct.<name> = type { <field llvm types> }` per entry in
/// `structs` (see `collect_extern_struct_types`) — the LLVM-aggregate-
/// type declaration half of struct-by-value FFI support. Each field's
/// LLVM type reuses `extern_type_to_llvm`'s own C-ABI width mapping
/// (`Int`->`i64`, `Float`->`double`, `Bool`->`i32`), so a nested struct
/// field naturally becomes a bare `%struct.<nested_name>` reference —
/// exactly the "named types resolve by name, order doesn't matter"
/// mechanism this whole feature leans on (see `extern_type_to_llvm`'s
/// own doc comment).
fn emit_extern_struct_types(structs: &BTreeMap<String, Vec<ir::ExternType>>) -> Result<String, String> {
    let mut out = String::new();
    for (name, field_types) in structs {
        let fields = field_types
            .iter()
            .map(extern_type_to_llvm)
            .collect::<Result<Vec<_>, _>>()?;
        out.push_str(&format!("%struct.{name} = type {{ {} }}\n", fields.join(", ")));
    }
    if !structs.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The small, fixed set of names this backend's OWN generated code
/// already claims (every libc function it declares directly —
/// `emit_runtime`/`emit_spawn_pthread_decls`/`emit_channel_runtime` —
/// plus every `plum_*`-prefixed runtime function it defines itself, via
/// a prefix check rather than an exhaustive list, since new `plum_*`
/// runtime functions are added over time and a prefix check can never
/// silently go stale the way a hand-maintained exhaustive list could).
/// `main` is reserved too, even though this list is otherwise about
/// codegen's OWN declares: `plumc`'s `emit_main` always defines a real
/// `@main` as the compiled binary's native entry point (see
/// `plumc::codegen_cli::emit_main`), so an extern named `main` would
/// collide with that just as surely as one named `malloc` would collide
/// with this crate's own declare.
fn is_reserved_extern_name(name: &str) -> bool {
    if name.starts_with("plum_") {
        return true;
    }
    const RESERVED: &[&str] = &[
        "malloc",
        "free",
        "memcpy",
        "memmove",
        "realloc",
        "exit",
        "printf",
        "snprintf",
        "strlen",
        "memchr",
        "pthread_create",
        "pthread_join",
        "pthread_mutex_init",
        "pthread_mutex_lock",
        "pthread_mutex_unlock",
        "pthread_cond_init",
        "pthread_cond_wait",
        "pthread_cond_signal",
        "usleep",
        "main",
    ];
    RESERVED.contains(&name)
}

/// Emits one `declare` per `extern "C"` function the program declares —
/// see this crate's module-level `emit_program` doc comment for the
/// supported `ExternType` scope. Unlike the spawn/channel runtime
/// (`needs_spawn_runtime`/`needs_channel_runtime`, gated reactively
/// because those helpers are only implicitly discoverable by walking
/// expression trees), NO such gating is needed here: `program.externs`
/// is already the complete, explicit, small list of every extern
/// function the program declares, so this can just iterate it directly.
/// Every name is checked against `is_reserved_extern_name` AND against
/// `signatures` (every declared Plum top-level function) — either
/// collision would otherwise silently produce a `declare`/`define`
/// clash for the SAME LLVM symbol name, a real `clang`-level error, not
/// just a style nit — surfaced here instead as a clear, specific
/// "reserved name" error before that ever happens.
/// The exact `(ret, params)` LLVM shape of every reserved runtime name
/// that ALSO happens to have a signature genuinely expressible through
/// `ExternType` (a real user-facing extern win — e.g. `strlen`, a common
/// libc function a Plum program might legitimately want to declare and
/// call directly, matching this crate's own internal `declare i64
/// @strlen(ptr)` byte-for-byte). Used ONLY to let a user's OWN extern
/// declaration for one of these names PASS instead of being rejected —
/// LLVM IR rejects a truly duplicate `declare` line for the same symbol
/// (confirmed directly via a real `clang` compile, not assumed), so
/// `emit_extern_declares` SKIPS re-emitting a second `declare` for one
/// of these once it's confirmed to match exactly, relying on `emit_
/// runtime`'s own unconditional declare to cover it. Deliberately NOT
/// exhaustive over every reserved name — `printf`/`snprintf` are
/// variadic (no `ExternType` shape can ever express that), and `malloc`/
/// `free`/`memcpy`/`memmove`/`realloc`/`pthread_create`/the mutex/cond
/// functions/`main` have signatures no REALISTIC user extern
/// declaration would ever legitimately reproduce — those simply keep
/// falling through to the plain "collides" rejection below, which is
/// the correct, safe default for a name this table doesn't cover.
fn reserved_extern_signature(name: &str) -> Option<(&'static str, &'static [&'static str])> {
    match name {
        "malloc" => Some(("ptr", &["i64"])),
        "free" => Some(("void", &["ptr"])),
        "memcpy" => Some(("ptr", &["ptr", "ptr", "i64"])),
        "memmove" => Some(("ptr", &["ptr", "ptr", "i64"])),
        "realloc" => Some(("ptr", &["ptr", "i64"])),
        "exit" => Some(("void", &["i32"])),
        "strlen" => Some(("i64", &["ptr"])),
        "memchr" => Some(("ptr", &["ptr", "i32", "i64"])),
        "usleep" => Some(("i32", &["i32"])),
        _ => None,
    }
}

fn emit_extern_declares(externs: &[ir::ExternFn], signatures: &HashMap<String, FnSig>) -> Result<String, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = String::new();
    for f in externs {
        let params = f
            .param_types
            .iter()
            .map(extern_type_to_llvm)
            .collect::<Result<Vec<_>, _>>()?;
        let ret = extern_ret_type_to_llvm(&f.ret_type)?;
        if is_reserved_extern_name(&f.name) {
            // Still a genuine collision UNLESS this is one of the small
            // set of reserved names whose C ABI shape is ALSO exactly
            // reproducible through `ExternType` (see `reserved_extern_
            // signature`'s own doc comment) — in that one case, the
            // user's declaration is accepted, but no SECOND `declare`
            // line is emitted for it (this crate's own unconditional one
            // in `emit_runtime` already covers the exact same symbol).
            // `params_str`: a `&str`-borrowed view of `params` (now
            // `Vec<String>` since `extern_type_to_llvm` widened to
            // support `%struct.<name>` — see that function's own doc
            // comment) purely so this can compare against `expected_
            // params`'s `&'static [&'static str]` without an owned-vs-
            // borrowed slice type mismatch.
            let params_str: Vec<&str> = params.iter().map(String::as_str).collect();
            match reserved_extern_signature(&f.name) {
                Some((expected_ret, expected_params)) if expected_ret == ret && expected_params == params_str.as_slice() => {
                    if !seen.insert(f.name.clone()) {
                        return Err(format!("codegen: extern function {:?} is declared more than once", f.name));
                    }
                    continue;
                }
                _ => {
                    return Err(format!(
                        "codegen: extern function {:?} collides with a name this backend's own generated runtime \
                         already uses (a libc function it declares itself, one of its own `plum_*` runtime \
                         functions, or the native `main` entry point) — rename it",
                        f.name
                    ));
                }
            }
        }
        if signatures.contains_key(&f.name) {
            return Err(format!(
                "codegen: extern function {:?} has the same name as a declared Plum function — this would \
                 collide with that function's own `@{}` symbol in the generated LLVM IR",
                f.name, f.name
            ));
        }
        if !seen.insert(f.name.clone()) {
            return Err(format!("codegen: extern function {:?} is declared more than once", f.name));
        }
        out.push_str(&format!("declare {ret} @{}({})\n", f.name, params.join(", ")));
    }
    if !externs.is_empty() {
        out.push('\n');
    }
    Ok(out)
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
/// `program.globals` (top-level, non-function `let`s) IS supported —
/// each gets its own LLVM global slot (`@global.<name>`, see below) plus
/// a generated `@plum_init_globals()` function that codegens every
/// initializer, in declaration order, using this exact same machinery
/// (`codegen_expr`) any function body already uses; `global_types` must
/// contain an entry for every `program.globals` entry, mirroring
/// `signatures`'s own "must contain an entry for every function called"
/// contract. Every other `ir::Expr` variant not otherwise mentioned
/// above (closures inside generics, more concurrency shapes, ...) is
/// still out of scope for now and produces a clear error naming what's
/// missing, never a panic — see this crate's tests for the exact error
/// shapes. A global whose initializer calls a still-generic function is
/// fully supported: `plumc::codegen_cli` threads `plum_ir::monomorphize`'s
/// own plan through globals too, rewriting each global's initializer to
/// reference the concrete, mangled instantiation before this crate ever
/// sees it — so by the time `program.globals` reaches here, every callee
/// name is already one `signatures` has an entry for, same as an
/// ordinary function body. `program.externs`
/// (`extern "C"` FFI) IS supported, scoped to scalar (`Int`/`Float`/
/// `Bool`) + `CStr` + C-callback parameters/returns + struct-by-value
/// (real named LLVM aggregate types, letting LLVM's own backend handle
/// System V ABI classification — see `collect_extern_struct_types`/
/// `codegen::build_c_struct_value`/`codegen::build_ctor_from_c_struct`)
/// — the last deferred FFI piece, now closed.
pub fn emit_program(
    program: &ir::Program,
    signatures: &HashMap<String, FnSig>,
    tag_fields: &TagFields,
    global_types: &HashMap<String, CgType>,
) -> Result<String, String> {
    let extern_struct_types = collect_extern_struct_types(&program.externs);
    let extern_struct_type_decls = emit_extern_struct_types(&extern_struct_types)?;
    let extern_declares = emit_extern_declares(&program.externs, signatures)?;
    let externs: HashMap<String, ir::ExternFn> =
        program.externs.iter().map(|f| (f.name.clone(), f.clone())).collect();
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
    // Same reasoning as the two loops above, applied to globals: an
    // Array-typed global needs its own element-release function even if
    // no function signature/struct field happens to mention that exact
    // element type either.
    for ty in global_types.values() {
        register_array_elem_type(&mut needed_arrays.borrow_mut(), ty);
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
    // The C-callback-trampoline counterpart to `trampolines` — kept in
    // its OWN table, not merged into `trampolines`, because the two
    // memoize genuinely DIFFERENT function shapes for the same target
    // function name: an ordinary closure trampoline always takes a
    // leading `ptr %env` (part of every Plum-level closure's own
    // calling convention, unused or not), while a C-callback trampoline
    // (`codegen::emit_c_callback_trampoline_fn`) has NO env parameter at
    // all — a real C API has no way to supply one. Conflating the two
    // tables would risk a genuine calling-convention bug: the same
    // target function referenced BOTH as an ordinary higher-order Plum
    // value AND as a C callback argument in the same program must get
    // two DIFFERENT generated trampoline functions, not one reused for
    // both shapes.
    let c_callback_trampolines = std::cell::RefCell::new(HashMap::new());

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
            &externs,
            &c_callback_trampolines,
            global_types,
        )?);
        bodies.push('\n');
    }

    // `@plum_init_globals()` — codegens every `program.globals`
    // initializer, in declaration order, into ONE generated function
    // using the exact same `codegen_expr` machinery any ordinary
    // function body already uses, storing each result into its own
    // slot (`@global.<name>`, declared alongside). Built and walked
    // HERE, alongside the ordinary function bodies above (not after —
    // same "collect everything while emitting bodies, then finalize"
    // convention `needed_arrays`/`needs_spawn_runtime`/etc. themselves
    // already established), so a global initializer that itself spawns/
    // channels/uses an array is correctly reflected in all those shared
    // tables before the whole-program checks and runtime-emission
    // decisions right below run. Only built at all when `program.
    // globals` is non-empty, matching the "pay only for what's used"
    // convention already established for the spawn/channel runtime.
    let (global_slots, init_globals_fn) =
        emit_init_globals(&program.globals, global_types, signatures, &tag_ids, tag_fields, &needed_arrays, &closure_counter, &closure_defs, &trampolines, &needs_spawn_runtime, &needs_channel_runtime, &externs, &c_callback_trampolines)?;

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
    out.push_str(&extern_struct_type_decls);
    out.push_str(&extern_declares);
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
    // `@global.<name>` slot declarations — placed here, before every
    // `define`, purely for human-readability of the generated `.ll`
    // (LLVM IR itself doesn't require a global's declaration to
    // textually precede whatever references it).
    out.push_str(&global_slots);
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
    // `@plum_init_globals()` itself — placed just before the ordinary
    // function bodies, matching `emit_main`'s own call-site ordering
    // requirement (`plumc::codegen_cli::emit_main`'s `has_globals`
    // parameter calls this BEFORE the resolved entry function).
    out.push_str(&init_globals_fn);
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
    externs: &HashMap<String, ir::ExternFn>,
    c_callback_trampolines: &std::cell::RefCell<HashMap<String, String>>,
    global_types: &HashMap<String, CgType>,
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
        externs,
        c_callback_trampolines,
        globals: global_types,
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

/// Builds `@plum_init_globals()` (plus its `@global.<name>` slot
/// declarations) — the direct structural counterpart to `emit_function`
/// above, adapted for a whole PROGRAM's worth of independent top-level
/// initializer expressions instead of one function's single body:
/// each `globals` entry is codegen'd with a FRESH, empty `Env` (a
/// global initializer never sees any OTHER global as a local binding —
/// only through `Ctx::globals`'s own third `Var`-resolution tier, which
/// is exactly what makes a later global's reference to an earlier one a
/// `load`, never a re-evaluation) via `codegen_expr` (not
/// `codegen_value` directly — a global initializer can legally contain
/// the full statement grammar, e.g. a `Let`/`If`, that `codegen_value`
/// alone doesn't handle), then the result is `store`d into its own
/// slot. Returns `(String::new(), String::new())` for an empty
/// `globals` slice — the caller (`emit_program`) still always calls
/// this (rather than gating the call itself) so the "collect shared
/// side-table state before finalizing" ordering stays uniform, but
/// pays zero output cost for a program with no globals at all.
#[allow(clippy::too_many_arguments)]
fn emit_init_globals(
    globals: &[ir::Global],
    global_types: &HashMap<String, CgType>,
    signatures: &HashMap<String, FnSig>,
    tag_ids: &HashMap<String, i64>,
    tag_fields: &TagFields,
    needed_arrays: &std::cell::RefCell<HashMap<String, CgType>>,
    closure_counter: &std::cell::RefCell<usize>,
    closure_defs: &std::cell::RefCell<Vec<String>>,
    trampolines: &std::cell::RefCell<HashMap<String, String>>,
    needs_spawn_runtime: &std::cell::RefCell<bool>,
    needs_channel_runtime: &std::cell::RefCell<bool>,
    externs: &HashMap<String, ir::ExternFn>,
    c_callback_trampolines: &std::cell::RefCell<HashMap<String, String>>,
) -> Result<(String, String), String> {
    if globals.is_empty() {
        return Ok((String::new(), String::new()));
    }

    let mut slot_decls = String::new();
    for g in globals {
        let ty = global_types
            .get(&g.name)
            .ok_or_else(|| format!("codegen: no type known for global {:?}", g.name))?;
        slot_decls.push_str(&format!("@global.{} = global {} zeroinitializer\n", g.name, ty.llvm_type()));
    }
    slot_decls.push('\n');

    // `caller_sig` only matters for deciding `musttail` eligibility on a
    // tail-position `Call` (see `Ctx::caller_sig`'s own doc comment) —
    // irrelevant here since every global initializer is codegen'd with
    // `tail=false` below, so a throwaway placeholder signature is always
    // safe, matching `codegen_spawn_literal`'s own `dummy_sig` precedent.
    let dummy_sig = FnSig { params: vec![], ret: CgType::Unit };
    let ctx = codegen::Ctx {
        sigs: signatures,
        caller_sig: &dummy_sig,
        tag_ids,
        tag_fields,
        fn_name: "plum_init_globals",
        needed_arrays,
        closure_counter,
        closure_defs,
        trampolines,
        needs_spawn_runtime,
        needs_channel_runtime,
        externs,
        c_callback_trampolines,
        globals: global_types,
    };

    let mut em = codegen::Emitter::new();
    let empty_env: HashMap<String, (String, CgType)> = HashMap::new();
    for g in globals {
        let (result, _) = codegen::codegen_expr(&g.value, &empty_env, &mut em, &ctx, false)?;
        let (reg, ty) = result.ok_or_else(|| {
            format!(
                "internal codegen error: global {:?}'s initializer produced no result (codegen_expr with \
                 tail=false should always return Some)",
                g.name
            )
        })?;
        em.lines.push(format!("  store {} {}, ptr @global.{}", ty.llvm_type(), reg, g.name));
    }
    em.lines.push("  ret void".to_string());

    let mut out = String::new();
    for sg in &em.string_globals {
        out.push_str(sg);
        out.push('\n');
    }
    out.push_str("define void @plum_init_globals() {\n");
    for line in &em.lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");

    Ok((slot_decls, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_ir::ir::{BinOp, Expr, ExternFn, ExternType, Function, MatchArm, PrimTy, Program, RcOp, SelectArm, UnOp};

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

    fn program_with_externs(functions: Vec<Function>, externs: Vec<ExternFn>) -> Program {
        Program { functions, globals: vec![], externs }
    }

    fn emit(prog: &Program, s: &HashMap<String, FnSig>, t: &TagFields) -> Result<String, String> {
        emit_program(prog, s, t, &HashMap::new())
    }

    fn emit_with_globals(
        prog: &Program,
        s: &HashMap<String, FnSig>,
        t: &TagFields,
        g: &HashMap<String, CgType>,
    ) -> Result<String, String> {
        emit_program(prog, s, t, g)
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
    fn plum_str_eq_and_memcmp_are_always_declared_in_the_runtime() {
        // `@plum_str_eq`/`@memcmp` are emitted unconditionally by
        // `emit_runtime`, the same "always present" style as `@memcpy`/
        // `@strlen` — this just confirms both actually show up in real
        // output, regardless of whether the program itself uses Str
        // equality.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::Int(0),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        assert!(ir.contains("declare i32 @memcmp(ptr, ptr, i64)"), "{ir}");
        assert!(ir.contains("define i1 @plum_str_eq(ptr %a, ptr %b)"), "{ir}");
        assert!(ir.contains("call i32 @memcmp"), "{ir}");
    }

    #[test]
    fn str_equality_emits_a_call_to_plum_str_eq() {
        // `"a" == "b"`-shaped source: previously `Str` fell through to a
        // hard `Err` in `codegen_binop` (no arm existed for it at all).
        // This proves the new Str-specific branch fires and emits a
        // `call`-shaped instruction to `@plum_str_eq`, not a bare `icmp`.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: Expr::Binary(BinOp::Eq, Box::new(Expr::Var("a".to_string())), Box::new(Expr::Var("b".to_string()))),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str, CgType::Str], CgType::Bool)]), &TagFields::new()).unwrap();
        assert!(ir.contains("call i1 @plum_str_eq(ptr %a, ptr %b)"), "{ir}");
    }

    #[test]
    fn str_inequality_emits_a_call_to_plum_str_eq_then_negates_it() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: Expr::Binary(BinOp::Ne, Box::new(Expr::Var("a".to_string())), Box::new(Expr::Var("b".to_string()))),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str, CgType::Str], CgType::Bool)]), &TagFields::new()).unwrap();
        assert!(ir.contains("call i1 @plum_str_eq(ptr %a, ptr %b)"), "{ir}");
        let call_idx = ir.find("call i1 @plum_str_eq").unwrap();
        assert!(ir[call_idx..].contains("xor i1"), "{ir}");
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
        // Scoped to `@go`'s OWN body, not the whole emitted program —
        // as of this chunk's Unicode string runtime, `emit_runtime`
        // itself legitimately uses plain `and i1` elsewhere (e.g.
        // `@plum_is_unicode_whitespace`'s range checks), so a whole-
        // program substring check would be a false positive unrelated
        // to whether `@go`'s OWN `&&` short-circuits.
        let go_start = ir.find("define i1 @go(").expect("`@go` should be emitted");
        let go_body = &ir[go_start..];
        assert!(!go_body.contains(" and i1 "), "{go_body}");
    }

    #[test]
    fn unsupported_construct_is_a_clear_error_not_a_panic() {
        // All six Unicode-aware string ops (`.runes()`/`.trim()`/
        // `.split()`/`.to_upper()`/`.to_lower()`/`.replace()`) are
        // supported as of this chunk (see the dedicated tests for each
        // below) — `RefNew` (a genuinely separate, still-unimplemented
        // feature: `ref(v)`'s shared-mutable-cell runtime has no
        // codegen at all yet) is still a clear, unsupported-construct
        // error instead.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::RefNew { value: Box::new(Expr::Int(1)) },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Unit)]), &TagFields::new()).expect_err("expected a clear error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    // --- Non-constant global initializers ---

    /// The now-stale full-rejection test, rewritten into a real
    /// success-path assertion: a single `Int` global gets its own
    /// `@global.x` slot (zero-initialized) plus a `@plum_init_globals()`
    /// that stores `1` into it.
    #[test]
    fn a_single_global_gets_a_slot_and_an_init_function_store() {
        let prog = Program {
            functions: vec![],
            globals: vec![plum_ir::ir::Global { name: "x".to_string(), value: Expr::Int(1) }],
            externs: vec![],
        };
        let ir = emit_with_globals(&prog, &HashMap::new(), &TagFields::new(), &HashMap::from([("x".to_string(), CgType::Int)]))
            .unwrap_or_else(|e| panic!("expected globals to be supported: {e}"));
        assert!(ir.contains("@global.x = global i64 zeroinitializer"), "{ir}");
        assert!(ir.contains("define void @plum_init_globals() {"), "{ir}");
        // The init function's own body must store `1` into `@global.x`
        // — extract just its body text so this assertion can't be
        // satisfied by some unrelated `store` elsewhere in the output.
        let init_start = ir.find("define void @plum_init_globals() {").unwrap();
        let init_body = &ir[init_start..];
        assert!(init_body.contains("store i64 1, ptr @global.x"), "{init_body}");
    }

    /// A second global that references the first must LOAD the
    /// already-stored slot, not re-evaluate the first global's own
    /// initializer expression — proven by asserting the generated text
    /// contains a `load` from `@global.a`, not a second `store` of `1`
    /// into it.
    #[test]
    fn a_second_global_loads_rather_than_reevaluates_the_first() {
        let prog = Program {
            functions: vec![],
            globals: vec![
                plum_ir::ir::Global { name: "a".to_string(), value: Expr::Int(1) },
                plum_ir::ir::Global {
                    name: "b".to_string(),
                    value: Expr::Binary(BinOp::Add, Box::new(Expr::Var("a".to_string())), Box::new(Expr::Int(1))),
                },
            ],
            externs: vec![],
        };
        let global_types = HashMap::from([("a".to_string(), CgType::Int), ("b".to_string(), CgType::Int)]);
        let ir = emit_with_globals(&prog, &HashMap::new(), &TagFields::new(), &global_types).unwrap();
        let init_start = ir.find("define void @plum_init_globals() {").unwrap();
        let init_body = &ir[init_start..];
        assert!(init_body.contains("load i64, ptr @global.a"), "{init_body}");
        assert!(init_body.contains("store i64 1, ptr @global.a"), "{init_body}");
        // Exactly ONE store into `@global.a` — the only way `b`'s
        // reference to `a` could "re-evaluate" it would be a SECOND
        // store into the same slot (re-running `Expr::Int(1)`'s own
        // codegen a second time).
        let store_a_count = init_body.lines().filter(|l| l.contains("store") && l.contains("@global.a")).count();
        assert_eq!(store_a_count, 1, "{init_body}");
    }

    /// A zero-globals program (the overwhelmingly common case) must
    /// emit NEITHER `@plum_init_globals` nor any `@global.*` slot —
    /// regression guard against the new code paying any cost at all
    /// when unused.
    #[test]
    fn zero_globals_emits_no_init_function_or_slots() {
        let prog = program(vec![Function { name: "go".to_string(), params: vec![], body: Expr::Int(1) }]);
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Int)]), &TagFields::new()).unwrap();
        assert!(!ir.contains("@plum_init_globals"), "{ir}");
        assert!(!ir.contains("@global."), "{ir}");
    }

    /// A function referencing a global must ALSO go through the third
    /// `Var`-resolution tier (a `load`), proving the new tier serves
    /// both `@plum_init_globals` itself and ordinary function bodies.
    #[test]
    fn a_function_loads_a_global_through_the_third_var_resolution_tier() {
        let prog = Program {
            functions: vec![Function {
                name: "use_it".to_string(),
                params: vec![],
                body: Expr::Var("x".to_string()),
            }],
            globals: vec![plum_ir::ir::Global { name: "x".to_string(), value: Expr::Int(5) }],
            externs: vec![],
        };
        let global_types = HashMap::from([("x".to_string(), CgType::Int)]);
        let ir = emit_with_globals(&prog, &sigs(&[("use_it", vec![], CgType::Int)]), &TagFields::new(), &global_types).unwrap();
        let fn_start = ir.find("define i64 @use_it(").unwrap();
        let fn_body = &ir[fn_start..];
        assert!(fn_body.contains("load i64, ptr @global.x"), "{fn_body}");
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
    fn a_still_unsupported_construct_is_a_clear_error() {
        // `RefGet`/`RefSet` are the other two thirds of the still-
        // wholly-unimplemented `ref`/`.get()`/`.set()` feature (see
        // `unsupported_construct_is_a_clear_error_not_a_panic`'s own
        // updated doc comment for why `RefNew` moved here from a
        // now-supported string op).
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::RefGet { base: Box::new(Expr::Var("s".to_string())) },
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![CgType::Heap], CgType::Int)]), &TagFields::new())
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

    // --- FFI: extern declares, ExternCall, AsCStr, C callbacks ---

    #[test]
    fn extern_declare_uses_i32_for_bool_never_i1() {
        // `extern "C" { fn is_ready(x: Bool) -> Bool; }` — a real, load-
        // bearing width mismatch against this backend's OWN `CgType::
        // Bool` (`i1`): C's `int` is `i32`-wide, so the `declare` itself
        // must say `i32`, not `i1`, on BOTH the parameter and the return.
        let prog = program_with_externs(
            vec![],
            vec![ExternFn { name: "is_ready".to_string(), param_types: vec![ExternType::Bool], ret_type: Some(ExternType::Bool) }],
        );
        let ir = emit(&prog, &sigs(&[]), &TagFields::new()).unwrap();
        assert!(ir.contains("declare i32 @is_ready(i32)"), "{ir}");
        assert!(!ir.contains("declare i1 @is_ready"), "{ir}");
    }

    #[test]
    fn extern_declare_maps_int_float_str_and_callback_correctly() {
        let prog = program_with_externs(
            vec![],
            vec![ExternFn {
                name: "mixed".to_string(),
                param_types: vec![
                    ExternType::Int,
                    ExternType::Float,
                    ExternType::Str,
                    ExternType::Callback { params: vec![ExternType::Int], ret: Some(Box::new(ExternType::Int)) },
                ],
                ret_type: None,
            }],
        );
        let ir = emit(&prog, &sigs(&[]), &TagFields::new()).unwrap();
        assert!(ir.contains("declare void @mixed(i64, double, ptr, ptr)"), "{ir}");
    }

    #[test]
    fn declaring_an_extern_with_a_reserved_runtime_name_is_a_clear_error() {
        let prog = program_with_externs(
            vec![],
            vec![ExternFn { name: "malloc".to_string(), param_types: vec![ExternType::Int], ret_type: None }],
        );
        let err = emit(&prog, &sigs(&[]), &TagFields::new())
            .expect_err("expected a reserved-name collision to be rejected, not silently double-declared");
        assert!(err.contains("malloc") && err.contains("collides"), "unexpected error: {err}");
    }

    #[test]
    fn extern_call_zexts_a_bool_argument_and_uses_icmp_ne_not_trunc_on_the_bool_return() {
        // let go (b: Bool) -> Bool = unsafe { check(b) }
        let prog = program_with_externs(
            vec![Function {
                name: "go".to_string(),
                params: vec!["b".to_string()],
                body: Expr::ExternCall { name: "check".to_string(), args: vec![Expr::Var("b".to_string())] },
            }],
            vec![ExternFn { name: "check".to_string(), param_types: vec![ExternType::Bool], ret_type: Some(ExternType::Bool) }],
        );
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Bool], CgType::Bool)]), &TagFields::new()).unwrap();
        // Argument direction: `i1 -> i32` via `zext`, never a bitcast/
        // truncation pretending the two widths are interchangeable.
        assert!(ir.contains("zext i1 %b to i32"), "{ir}");
        // Return direction: `icmp ne i32 .., 0` — matching C's "any
        // nonzero is true" convention — NEVER `trunc`, which would only
        // look at the low bit.
        let call_idx = ir.find("call i32 @check(").expect("expected a direct i32 call to @check");
        let after_call = &ir[call_idx..];
        assert!(after_call.contains("icmp ne i32"), "{after_call}");
        assert!(!ir.contains("trunc i32"), "extern Bool-return marshaling must never use trunc: {ir}");
    }

    #[test]
    fn extern_call_with_a_null_str_return_aborts_via_the_runtime_check_mechanism() {
        // let go () -> Str = unsafe { get_message() }
        let prog = program_with_externs(
            vec![Function {
                name: "go".to_string(),
                params: vec![],
                body: Expr::ExternCall { name: "get_message".to_string(), args: vec![] },
            }],
            vec![ExternFn { name: "get_message".to_string(), param_types: vec![], ret_type: Some(ExternType::Str) }],
        );
        let ir = emit(&prog, &sigs(&[("go", vec![], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(ir.contains("call ptr @get_message()"), "{ir}");
        assert!(ir.contains("icmp eq ptr"), "{ir}");
        assert!(ir.contains("call void @plum_abort("), "{ir}");
        assert!(ir.contains("call i64 @strlen("), "{ir}");
        assert!(ir.contains("call ptr @plum_alloc_str("), "{ir}");
    }

    #[test]
    fn as_cstr_validates_copies_and_decs_the_original_str_register() {
        // let go (s: Str) -> CStr = s.as_cstr()
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::AsCStr(Box::new(Expr::Var("s".to_string()))),
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str], CgType::CStr)]), &TagFields::new()).unwrap();
        // Embedded-NUL validation via a real libc `memchr`, matching the
        // `strlen` precedent, never a hand-rolled scan loop.
        assert!(ir.contains("call ptr @memchr(ptr"), "{ir}");
        assert!(ir.contains("call void @plum_abort("), "{ir}");
        // A FRESH allocation — `malloc` + `memcpy` + a manually stored
        // trailing NUL — never a pointer aliased into the existing `Str`
        // cell (see `codegen::codegen_as_cstr`'s doc comment for why
        // that shortcut would be unsound).
        assert!(ir.contains("call ptr @malloc("), "{ir}");
        assert!(ir.contains("call ptr @memcpy("), "{ir}");
        assert!(ir.contains("store i8 0, ptr"), "{ir}");
        // The mandatory ownership-discharge dec MUST reference the
        // ORIGINAL Str cell's own register — a function parameter named
        // `s` is bound directly to `%s` (see `emit_function`'s param
        // binding), so this is exactly `%s`, never the fresh `CStr`
        // buffer register.
        assert!(ir.contains("call void @plum_rc_dec_str(ptr %s)"), "{ir}");
    }

    #[test]
    fn as_cstr_on_a_non_str_value_is_a_clear_codegen_error() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["n".to_string()],
            body: Expr::AsCStr(Box::new(Expr::Var("n".to_string()))),
        }]);
        let err = emit(&prog, &sigs(&[("go", vec![CgType::Int], CgType::CStr)]), &TagFields::new())
            .expect_err("expected `.as_cstr()` on a non-Str value to be rejected");
        assert!(err.contains("as_cstr") && err.contains("Str"), "unexpected error: {err}");
    }

    #[test]
    fn a_c_callback_trampoline_has_no_env_parameter_unlike_an_ordinary_closure_trampoline() {
        // let add(a, b) = a + b
        // let go () -> Int = unsafe { call_with_10_and_20(add) }
        let add_fn = Function {
            name: "add".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: Expr::Binary(BinOp::Add, Box::new(Expr::Var("a".to_string())), Box::new(Expr::Var("b".to_string()))),
        };
        let go_fn = Function {
            name: "go".to_string(),
            params: vec![],
            body: Expr::ExternCall { name: "call_with_10_and_20".to_string(), args: vec![Expr::Var("add".to_string())] },
        };
        let prog = program_with_externs(
            vec![add_fn, go_fn],
            vec![ExternFn {
                name: "call_with_10_and_20".to_string(),
                param_types: vec![ExternType::Callback {
                    params: vec![ExternType::Int, ExternType::Int],
                    ret: Some(Box::new(ExternType::Int)),
                }],
                ret_type: Some(ExternType::Int),
            }],
        );
        let ir = emit(
            &prog,
            &sigs(&[
                ("add", vec![CgType::Int, CgType::Int], CgType::Int),
                ("go", vec![], CgType::Int),
            ]),
            &TagFields::new(),
        )
        .unwrap();
        // The trampoline symbol is referenced DIRECTLY as the call
        // argument — no `ptrtoint`/`inttoptr` round-trip, unlike an
        // ordinary closure-value's cell-and-code-pointer dance.
        assert!(ir.contains("ptr @c_trampoline$add"), "{ir}");
        let def_start = ir.find("define i64 @c_trampoline$add(").expect("expected a generated c_trampoline$add definition");
        let def_end = ir[def_start..].find("\n}\n").map(|i| def_start + i).unwrap_or(ir.len());
        let def_text = &ir[def_start..def_end];
        // The defining structural difference from `emit_trampoline_fn`'s
        // ordinary closure trampoline: NO leading `ptr %env` parameter
        // at all — a real C API has no way to supply one.
        assert!(!def_text.contains("%env"), "C callback trampoline must never take an env parameter: {def_text}");
        assert!(def_text.contains("i64 %p0"), "{def_text}");
        assert!(def_text.contains("i64 %p1"), "{def_text}");
        assert!(def_text.contains("call i64 @add(i64 %p0, i64 %p1)"), "{def_text}");
    }

    #[test]
    fn a_callback_argument_that_is_not_a_bare_function_name_is_a_clear_codegen_error() {
        let prog = program_with_externs(
            vec![Function {
                name: "go".to_string(),
                params: vec![],
                // `5` isn't a bare top-level function name.
                body: Expr::ExternCall { name: "call_it".to_string(), args: vec![Expr::Int(5)] },
            }],
            vec![ExternFn {
                name: "call_it".to_string(),
                param_types: vec![ExternType::Callback { params: vec![], ret: None }],
                ret_type: None,
            }],
        );
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Unit)]), &TagFields::new())
            .expect_err("expected a non-Var callback argument to be rejected");
        assert!(err.contains("callback"), "unexpected error: {err}");
    }

    #[test]
    fn struct_by_value_extern_param_emits_a_named_struct_type_and_insertvalue_sequence() {
        // let go (p: Point) -> Unit = unsafe { takes_point(p) }
        // (`Point` is a 2-field Int struct — the simplest by-value shape.)
        let prog = program_with_externs(
            vec![Function {
                name: "go".to_string(),
                params: vec!["p".to_string()],
                body: Expr::ExternCall { name: "takes_point".to_string(), args: vec![Expr::Var("p".to_string())] },
            }],
            vec![ExternFn {
                name: "takes_point".to_string(),
                param_types: vec![ExternType::Struct("Point".to_string(), vec![ExternType::Int, ExternType::Int])],
                ret_type: None,
            }],
        );
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Unit)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        // The named LLVM aggregate type declaration.
        assert!(ir.contains("%struct.Point = type { i64, i64 }"), "{ir}");
        // Two `insertvalue`s, building the aggregate up from `undef`,
        // one per field — reading each field's word out of the Ctor cell
        // first via `load_field_word`'s own `getelementptr`+`load` shape.
        assert!(ir.contains("insertvalue %struct.Point undef, i64"), "{ir}");
        assert!(ir.matches("insertvalue %struct.Point").count() == 2, "{ir}");
        // The call itself passes the aggregate BY VALUE, never a pointer.
        assert!(ir.contains("call void @takes_point(%struct.Point"), "{ir}");
    }

    #[test]
    fn struct_by_value_extern_return_extracts_fields_into_a_fresh_ctor_cell() {
        // let go () -> Point = unsafe { make_point() }
        let prog = program_with_externs(
            vec![Function {
                name: "go".to_string(),
                params: vec![],
                body: Expr::ExternCall { name: "make_point".to_string(), args: vec![] },
            }],
            vec![ExternFn {
                name: "make_point".to_string(),
                param_types: vec![],
                ret_type: Some(ExternType::Struct("Point".to_string(), vec![ExternType::Int, ExternType::Int])),
            }],
        );
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![], CgType::Heap)]),
            &tags(&[("Point", vec![CgType::Int, CgType::Int])]),
        )
        .unwrap();
        assert!(ir.contains("%struct.Point = type { i64, i64 }"), "{ir}");
        // The call returns the aggregate BY VALUE.
        assert!(ir.contains("call %struct.Point @make_point()"), "{ir}");
        // Two `extractvalue`s pull the fields back out of the aggregate...
        assert!(ir.matches("extractvalue %struct.Point").count() == 2, "{ir}");
        // ...stored into a FRESH cell allocated via the same `@plum_alloc`
        // every ordinary `Ctor` construction uses (reusing the tag's
        // already-interned id), never mutating anything the C call
        // itself produced.
        assert!(ir.contains("call ptr @plum_alloc(i64"), "{ir}");
    }

    #[test]
    fn mixed_bool_int_struct_field_shape_and_order_is_correct() {
        // The deliberately-padding-inducing shape: `Mixed { flag: Bool,
        // big: Int }` — `Bool` maps to `i32` (4 bytes) at the C ABI
        // boundary, `Int` to `i64` (8 bytes), so a real `clang`/System V
        // classification of this exact shape induces 4 bytes of padding
        // between the two fields. This test asserts ONLY the emitted
        // type's field order/widths (`i32` then `i64`, matching the
        // struct's OWN declared field order) — never byte offsets/
        // padding, which a named LLVM aggregate type carries no explicit
        // text for at all; that's precisely why LLVM's own backend (not
        // this codegen) is trusted to get the real padding right — see
        // `plumc::codegen_cli`'s compile-and-run test of this exact
        // shape for the actual correctness proof.
        let prog = program_with_externs(
            vec![],
            vec![ExternFn {
                name: "takes_mixed".to_string(),
                param_types: vec![ExternType::Struct("Mixed".to_string(), vec![ExternType::Bool, ExternType::Int])],
                ret_type: None,
            }],
        );
        let ir = emit(&prog, &sigs(&[]), &tags(&[("Mixed", vec![CgType::Bool, CgType::Int])])).unwrap();
        assert!(ir.contains("%struct.Mixed = type { i32, i64 }"), "{ir}");
    }

    #[test]
    fn nested_struct_argument_emits_both_types_and_a_nested_insertvalue() {
        // struct Inner { a: Int }
        // struct Outer { inner: Inner, b: Int }
        // let go (o: Outer) -> Unit = unsafe { takes_outer(o) }
        let prog = program_with_externs(
            vec![Function {
                name: "go".to_string(),
                params: vec!["o".to_string()],
                body: Expr::ExternCall { name: "takes_outer".to_string(), args: vec![Expr::Var("o".to_string())] },
            }],
            vec![ExternFn {
                name: "takes_outer".to_string(),
                param_types: vec![ExternType::Struct(
                    "Outer".to_string(),
                    vec![ExternType::Struct("Inner".to_string(), vec![ExternType::Int]), ExternType::Int],
                )],
                ret_type: None,
            }],
        );
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Heap], CgType::Unit)]),
            &tags(&[("Outer", vec![CgType::Heap, CgType::Int]), ("Inner", vec![CgType::Int])]),
        )
        .unwrap();
        // Both named struct types are emitted — the nested one referenced
        // by name from the outer one's own field list, regardless of
        // which `type` line appears first in the text (named LLVM struct
        // types resolve by name at module scope).
        assert!(ir.contains("%struct.Inner = type { i64 }"), "{ir}");
        assert!(ir.contains("%struct.Outer = type { %struct.Inner, i64 }"), "{ir}");
        // The nested aggregate is built up (one `insertvalue` into
        // `%struct.Inner`), then ITSELF `insertvalue`d into the outer
        // aggregate as a genuine sub-aggregate value — never passed as a
        // pointer at the C boundary.
        assert!(ir.contains("insertvalue %struct.Inner undef, i64"), "{ir}");
        assert!(ir.contains("insertvalue %struct.Outer undef, %struct.Inner"), "{ir}");
    }

    #[test]
    fn a_cstr_typed_field_would_be_rejected_when_a_program_uses_spawn() {
        // Defense-in-depth: `check_no_closure_or_task_fields` also
        // rejects a `CStr`-shaped struct/enum field once `spawn`/
        // channels are used anywhere in the program — see that
        // function's own doc comment for why this is currently
        // unreachable via the ordinary frontend (an ordinary struct
        // field's type annotation can never resolve to `CStr`) but kept
        // as a hand-built-`TagFields` defensive check regardless,
        // mirroring `Closure`/`Task`'s own established precedent.
        let body = Expr::Spawn { block: Box::new(Expr::Int(0)) };
        let prog = program(vec![Function { name: "go".to_string(), params: vec![], body }]);
        let err = emit(&prog, &sigs(&[("go", vec![], CgType::Task(Box::new(CgType::Int)))]), &tags(&[("Holder", vec![CgType::CStr])]))
            .expect_err("expected a CStr-shaped struct field to be rejected once `spawn` is used");
        assert!(err.contains("Holder"), "unexpected error: {err}");
        assert!(err.contains("CStr"), "unexpected error: {err}");
    }

    // --- Unicode string ops: mechanical shape tests ---
    //
    // IR-text shape only, not correctness (real UTF-8 decoding/
    // boundary/piece-count correctness is proven by `plumc`'s own
    // compile-and-run tests instead — IR inspection can't tell a
    // correct decoder from a subtly wrong one, only its overall SHAPE).

    #[test]
    fn str_runes_emits_a_count_loop_then_a_fill_loop_sized_by_a_register() {
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrRunes { base: Box::new(Expr::Var("s".to_string())) },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str], CgType::Array(Box::new(CgType::Int)))]), &TagFields::new()).unwrap();
        assert!(ir.contains("define ptr @plum_str_runes(ptr %s)"), "{ir}");
        // Count pass uses the cheap classify-only primitive; fill pass
        // uses the full decode.
        assert!(ir.contains("call i64 @plum_utf8_len_at("), "{ir}");
        assert!(ir.contains("call i64 @plum_utf8_decode("), "{ir}");
        // The array is allocated at a COUNTED size (a register from the
        // count loop), never a literal immediate.
        assert!(ir.contains("%arr = call ptr @plum_alloc_array(i64 %count)"), "{ir}");
    }

    #[test]
    fn to_upper_and_to_lower_call_libc_case_mapping_not_the_old_ascii_loop() {
        // Proves the rewrite: `@plum_str_to_upper`/`@plum_str_to_lower`
        // now call real libc `towupper`/`towlower` and use the two-pass
        // count-then-fill shape (`@plum_utf8_decode`/`@plum_utf8_encode`/
        // `@plum_utf8_encoded_len`), NOT the old fixed-length, per-BYTE
        // ASCII-range `select` loop (`icmp uge i8 %b, 97`, etc. — no
        // longer present anywhere in emitted IR).
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrToUpper { base: Box::new(Expr::Var("s".to_string())) },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(ir.contains("declare i32 @towupper(i32)"), "{ir}");
        assert!(ir.contains("declare i32 @towlower(i32)"), "{ir}");
        assert!(ir.contains("declare ptr @setlocale(i32, ptr)"), "{ir}");
        assert!(ir.contains("define void @plum_locale_init()"), "{ir}");
        assert!(ir.contains("define i64 @plum_utf8_encoded_len(i64 %cp)"), "{ir}");
        assert!(ir.contains("define i64 @plum_utf8_encode(ptr %dst, i64 %cp)"), "{ir}");
        assert!(ir.contains("call i32 @towupper(i32"), "{ir}");
        assert!(!ir.contains("icmp uge i8 %b, 97"), "{ir}");
        assert!(!ir.contains("icmp ule i8 %b, 122"), "{ir}");

        let prog2 = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrToLower { base: Box::new(Expr::Var("s".to_string())) },
        }]);
        let ir2 = emit(&prog2, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(ir2.contains("call i32 @towlower(i32"), "{ir2}");
        assert!(!ir2.contains("icmp uge i8 %b, 65"), "{ir2}");
        assert!(!ir2.contains("icmp ule i8 %b, 90"), "{ir2}");
    }

    #[test]
    fn to_upper_reuse_and_to_lower_reuse_free_and_call_fresh_not_inplace() {
        // Case mapping can now change a string's byte length (real
        // Unicode `towupper`/`towlower`, not a fixed one-byte-for-one-
        // byte ASCII transform) — the `_inplace` variants are gone
        // entirely, and the reuse branch instead calls the fresh
        // function then `@free`s the old cell (mirroring
        // `StrReplaceReuse`'s own reuse branch).
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrToUpperReuse { reuse_of: "s".to_string() },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(!ir.contains("plum_str_to_upper_inplace"), "{ir}");
        let go_body = &ir[ir.find("define ptr @go(").unwrap()..];
        assert!(go_body.contains("call ptr @plum_str_to_upper(ptr %s)"), "{go_body}");
        assert!(go_body.contains("call void @free(ptr %s)"), "{go_body}");

        let prog2 = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrToLowerReuse { reuse_of: "s".to_string() },
        }]);
        let ir2 = emit(&prog2, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(!ir2.contains("plum_str_to_lower_inplace"), "{ir2}");
        let go_body2 = &ir2[ir2.find("define ptr @go(").unwrap()..];
        assert!(go_body2.contains("call ptr @plum_str_to_lower(ptr %s)"), "{go_body2}");
        assert!(go_body2.contains("call void @free(ptr %s)"), "{go_body2}");
    }

    #[test]
    fn str_trim_reuse_calls_no_realloc_either_only_memmove() {
        // Trimming only ever SHRINKS — same "no @realloc needed" shape
        // as case-mapping's reuse branch, just via `@memmove` (the
        // trimmed range can overlap `[0, newlen)`) instead of a byte
        // loop.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrTrimReuse { reuse_of: "s".to_string() },
        }]);
        let ir = emit(&prog, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(ir.contains("call void @plum_str_trim_inplace(ptr %s)"), "{ir}");
        let go_body = &ir[ir.find("define ptr @go(").unwrap()..];
        assert!(!go_body.contains("call ptr @realloc"), "{go_body}");
    }

    #[test]
    fn str_replace_reuse_deliberately_does_not_realloc_in_place() {
        // Documented deviation from this chunk's original design notes
        // (see the `StrReplaceReuse` codegen arm's own doc comment for
        // the full aliasing-hazard reasoning): the reuse branch calls
        // the SAME fresh-allocating `@plum_str_replace` — safe in every
        // case, including growth — then frees the OLD cell directly,
        // rather than attempting an unsound in-place forward copy.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string(), "from".to_string(), "to".to_string()],
            body: Expr::StrReplaceReuse {
                reuse_of: "s".to_string(),
                from: Box::new(Expr::Var("from".to_string())),
                to: Box::new(Expr::Var("to".to_string())),
            },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Str, CgType::Str, CgType::Str], CgType::Str)]),
            &TagFields::new(),
        )
        .unwrap();
        // Both branches call the same fresh-building runtime function...
        let call_count = ir.matches("call ptr @plum_str_replace(ptr %s, ptr %from, ptr %to)").count();
        assert_eq!(call_count, 2, "{ir}");
        // ...and the reuse branch additionally frees the old cell.
        assert!(ir.contains("call void @free(ptr %s)"), "{ir}");
    }

    #[test]
    fn str_trim_split_and_replace_call_their_own_new_runtime_functions() {
        let trim_prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string()],
            body: Expr::StrTrim { base: Box::new(Expr::Var("s".to_string())) },
        }]);
        let trim_ir = emit(&trim_prog, &sigs(&[("go", vec![CgType::Str], CgType::Str)]), &TagFields::new()).unwrap();
        assert!(trim_ir.contains("call ptr @plum_str_trim(ptr %s)"), "{trim_ir}");

        let split_prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string(), "sep".to_string()],
            body: Expr::StrSplit { base: Box::new(Expr::Var("s".to_string())), sep: Box::new(Expr::Var("sep".to_string())) },
        }]);
        let split_ir = emit(
            &split_prog,
            &sigs(&[("go", vec![CgType::Str, CgType::Str], CgType::Array(Box::new(CgType::Str)))]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(split_ir.contains("call ptr @plum_str_split(ptr %s, ptr %sep)"), "{split_ir}");

        let replace_prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string(), "from".to_string(), "to".to_string()],
            body: Expr::StrReplace {
                base: Box::new(Expr::Var("s".to_string())),
                from: Box::new(Expr::Var("from".to_string())),
                to: Box::new(Expr::Var("to".to_string())),
            },
        }]);
        let replace_ir = emit(
            &replace_prog,
            &sigs(&[("go", vec![CgType::Str, CgType::Str, CgType::Str], CgType::Str)]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(replace_ir.contains("call ptr @plum_str_replace(ptr %s, ptr %from, ptr %to)"), "{replace_ir}");
    }

    #[test]
    fn str_split_registers_array_of_str_release_function() {
        // `.split()` builds `Array[Str]` — its element-release function
        // must be registered (`needed_arrays`) even though no struct
        // field or function signature anywhere else in this tiny program
        // mentions `Array[Str]`.
        let prog = program(vec![Function {
            name: "go".to_string(),
            params: vec!["s".to_string(), "sep".to_string()],
            body: Expr::StrSplit { base: Box::new(Expr::Var("s".to_string())), sep: Box::new(Expr::Var("sep".to_string())) },
        }]);
        let ir = emit(
            &prog,
            &sigs(&[("go", vec![CgType::Str, CgType::Str], CgType::Array(Box::new(CgType::Str)))]),
            &TagFields::new(),
        )
        .unwrap();
        assert!(ir.contains("define void @plum_rc_dec_array_Str(ptr %p)"), "{ir}");
    }
}
