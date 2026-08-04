mod codegen_cli;
mod modules;
mod project;
#[cfg(test)]
mod test_util;
pub use codegen_cli::{compile_and_run, compile_ir_to_binary, compile_program_to_ir, emit_main, reject_unprintable_return, CgValue};
pub use modules::{resolve_modules, typecheck_and_run_modules};
pub use project::{resolve_project, typecheck_and_run_project};

use plum_interp::{Interpreter, Value};
use plum_ir::fbip::optimize_program;
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::ast;
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;

/// `Option[T]`/`Result[T, E]` — DESIGN.md's "no null, anywhere, ever"
/// story specs these as ORDINARY generic enums, "under the hood," not
/// as compiler-magic types. This is exactly that: real Plum source,
/// prepended to every program before anything else sees it, rather
/// than a special-cased builtin type baked into `plum-types`/`plum-ir`
/// directly. A user program is free to pattern-match `Some`/`None`/
/// `Ok`/`Err` with no declaration of its own, exactly as if it had
/// written this itself at the top of the file.
const PRELUDE_SRC: &str = "\
enum Option[T] { Some(T), None }
enum Result[T, E] { Ok(T), Err(E) }
";

/// The very first stdlib piece — see DESIGN.md's "Standard library"
/// section. `println` and `print` are deliberately ORDINARY Plum
/// source, not new compiler/backend builtins: `.to_string()` already
/// dispatches correctly on a still-unresolved generic type parameter
/// (proven empirically in both backends before writing this —
/// monomorphization fully specializes a generic function's body before
/// either backend ever sees it, so by the time codegen/the interpreter
/// evaluates `x.to_string()` here, `T` is always a concrete, resolved
/// type), and `unsafe { extern-call }` inside a still-generic function
/// body works with zero special-casing (the `in_unsafe` gate and
/// monomorphization's own per-instantiation rewrite are both orthogonal
/// to genericity).
///
/// **BOTH `print`/`println` are built on the raw POSIX `write(2)`
/// syscall** (`write(fd: Int, buf: CStr, count: Int) -> Int` — every
/// parameter/return type already Int/CStr, both already fully
/// supported, no new type-system work needed) — DELIBERATELY not
/// libc's `puts`/`fputs`/`printf`, and this was a real, two-step design
/// correction, not the original plan:
/// - `println` FIRST used `puts` (which conveniently always appends a
///   newline on its own). But adding `print` (no newline) alongside it
///   surfaced a genuine cross-function bug once both were tested
///   together, not predicted in advance: `puts` goes through C's
///   block-buffered stdio, while `write` is unbuffered and reaches the
///   OS immediately — mixing the two in one program (`print("a");
///   println("b"); print("c")`) produced OUT-OF-ORDER output
///   (`"acb\n"` instead of `"ab\nc"`), since `write`'s calls could
///   reach the terminal/pipe before an EARLIER, still-buffered `puts`
///   call's output flushed. This is the exact same class of problem
///   `println`'s own `fflush` fix (see below) already solved ONCE, for
///   the interpreter CLI specifically — but that fix only covers the
///   CLI's own final print, not two DIFFERENT buffering strategies
///   fighting each other WITHIN a single running Plum program. Fixed
///   properly by putting `println`/`print` on the exact SAME
///   mechanism (`write`) instead of patching around the mismatch —
///   `println` builds its newline into the string itself
///   (`.concat("\n")`, a core-language builtin) before one `write`
///   call, rather than a second syscall or a different C function.
/// - `fputs(s, stdout)`/variadic `printf("%s", s)` were considered
///   first and rejected as real dead ends for THIS codebase
///   specifically (confirmed by reading the actual extern-type
///   machinery, not assumed): `fputs` needs a `FILE*` parameter, and
///   this codebase's extern type system is a CLOSED list (`Int`/
///   `Float`/`Bool`/`CStr`/callback/struct-of-those — see `plum-types/
///   src/context.rs`'s `check_ffi_safe`) with no raw-pointer/opaque
///   type and no extern-GLOBAL-variable grammar at all (only `fn`s can
///   be declared in an `extern` block) — `stdout` itself isn't even
///   referenceable. `printf` needs C-variadic call support, which
///   doesn't exist anywhere in this codebase for USER-declared externs
///   (the LLVM backend emits a few HARDCODED internal variadic calls of
///   its own, e.g. for `Float.to_string()`, but nothing threads that
///   through to Plum-source-declared externs, and `plum-interp`'s
///   `libffi`-based extern-call path has no variadic-CIF support at
///   all). Both would need genuine new type-system/backend work in at
///   least one backend, non-trivial in the interpreter's case.
///
/// The byte count for `write` comes from `.len()` on the `Str` BEFORE
/// converting to `CStr` (a core-language builtin, not a new extern) —
/// DELIBERATELY not a separate `strlen` extern call: an earlier draft
/// used one, but `strlen` is common enough that more than one EXISTING
/// test in this codebase already declares its own extern `strlen` for
/// unrelated reasons, and prelude-merged source shares the SAME flat
/// top-level namespace as ordinary declarations (real "already
/// declared" collisions resulted, caught by the existing test suite
/// immediately) — `.len()` sidesteps the collision risk entirely AND
/// avoids an extra FFI round-trip. `write` itself was checked against
/// every existing extern declaration in this codebase first; no
/// collision. Verified empirically in BOTH backends (real, throwaway
/// compiled-and-run probes) that discarding a non-`Unit` extern return
/// value (`write` returns `Int`) works fine inside a block ending in
/// `()`.
///
/// **A real use-after-free found by testing, not caught by the type
/// checker** (it's a memory-ownership bug, not a type error): an
/// earlier draft called `write(1, s.as_cstr(), s.len())` directly.
/// `.as_cstr()` CONSUMES its `Str` (its own lowering calls `@plum_rc_
/// dec_str` on the original cell — confirmed by reading the actual
/// generated LLVM IR, not assumed), and arguments evaluate left to
/// right, so `s.len()` — evaluated THIRD, after `.as_cstr()` already
/// ran — read `s`'s length field after it may already have been freed.
/// Silent, not a crash: `write` just wrote zero (or garbage) bytes,
/// which is why the very first real compile-and-run test caught it
/// immediately (`print`'s output was simply MISSING from captured
/// stdout, not obviously "wrong"). Fixed by binding the length to a
/// local (`let n = s.len()`) BEFORE calling `.as_cstr()`, so the read
/// happens while `s` is still guaranteed alive. The SAME ordering
/// applies to `println`'s `.concat("\n")` result.
///
/// The interpreter CLI's own `fflush(NULL)` fix (in `main.rs`, from
/// when `println` still used `puts`) is KEPT regardless of this
/// switch to `write` — it's still good, general defensive practice for
/// any OTHER extern call a user's own program might make through
/// buffered C stdio (`printf`, `fputs`, ...), even though `println`/
/// `print` themselves no longer need it.
///
/// Merged into `with_prelude` (not a real `use`-based module) for now,
/// deliberately: the existing `compile_and_run` test harness (used by
/// nearly this whole workspace's codegen test suite) goes through
/// `with_prelude` alone, never `modules.rs`/`project.rs` — a real
/// module would need that harness extended to drive a temp project
/// through `resolve_project` first, a bigger, separate piece of work.
/// Kept as its OWN constant (not folded into `PRELUDE_SRC` itself) so
/// it can be deleted/moved wholesale into a real `io` module later
/// without having tangled its source into the `Option`/`Result`
/// sugar-type story.
const STDLIB_IO_SRC: &str = "\
extern \"C\" {
    fn write(fd: Int, buf: CStr, count: Int) -> Int;
}

let print[T] (x: T): Unit = unsafe {
    let s = x.to_string();
    let n = s.len();
    write(1, s.as_cstr(), n);
    ()
}

let println[T] (x: T): Unit = unsafe {
    let s = x.to_string().concat(\"\\n\");
    let n = s.len();
    write(1, s.as_cstr(), n);
    ()
}
";

/// Basic file I/O — chunk 8 of the standard library. `read_file_raw`/
/// `write_file_raw` are low-level core-language builtins (see `ir::
/// Expr::ReadFileRaw`'s own doc comment for the full design), NOT
/// `extern "C"` declarations — unlike `write` above, `read` needs a
/// mutable out-buffer, which the closed extern FFI type system (`Int`/
/// `Float`/`Bool`/`CStr`/callback/struct-of-those, see `plum-types::
/// context::check_ffi_safe`) has no way to express at all. Both
/// evaluate to `__FileIoResult { ok: Bool, payload: String }` directly
/// — `read_file`/`write_file` below are ORDINARY Plum source (like
/// `print`/`println` above), translating that into `Result[T, String]`
/// via a plain `if`, so `Ok`/`Err` construction goes through the exact
/// same monomorphization-discovery path any user program's `Result`
/// usage already would; no special-cased tag registration needed.
/// `write_file` always truncates + creates (matches the interpreter's
/// own `std::fs::write`, which does the same — both backends agree by
/// construction, not extra coordination). No `unsafe {}` needed for
/// either wrapper — like `.to_string()`/`ref(v)`, these are genuinely
/// new core-language builtins, not extern calls.
const STDLIB_FILE_SRC: &str = "\
struct __FileIoResult { ok: Bool, payload: String }

let read_file (path: String): Result[String, String] = {
    let r = read_file_raw(path);
    if r.ok { Ok(r.payload) } else { Err(r.payload) }
}

let write_file (path: String) (contents: String): Result[Unit, String] = {
    let r = write_file_raw(path, contents);
    if r.ok { Ok(()) } else { Err(r.payload) }
}
";

/// `Map[K,V]`/`Set[T]` — chunk 2 of the standard library. Built as
/// ORDINARY recursive generic enums (association lists), the same
/// proven-safe shape as `List[T]` elsewhere in this codebase's own test
/// suite — NOT `Array[Tuple[K,V]]`, which isn't safe today
/// (`plum_type_to_cg_type` has no real `Type::Tuple` arm; every tuple
/// collapses to one flat synthetic tag once reached through a type
/// signature rather than always fully destructured locally). Depends on
/// the `Str`-equality LLVM-backend fix landed alongside this chunk —
/// before that fix, `==`/`!=` on `Str` didn't even compile natively, so
/// no Str-keyed `Map`/`Set` could have worked in the native backend at
/// all.
///
/// Deliberate, DOCUMENTED v1 simplifications (not bugs — see DESIGN.md):
/// - `map_insert` always PREPENDS (O(1), no scan-to-replace). `map_get`/
///   `map_contains` scan from the head, so the MOST RECENTLY inserted
///   entry for a key wins. `map_remove` removes only the FIRST (most
///   recent) matching node — removing a twice-inserted key once
///   uncovers the OLDER value rather than erasing all trace of the key.
///   `map_len` counts NODES, not unique keys.
/// - `set_insert` DOES dedupe (checks `set_contains` first) — a set has
///   no duplicates by definition, unlike a map's multi-node-per-key
///   allowance.
/// - Linear (`O(n)`) lookup/insert/contains/remove throughout — no
///   hashing. A hash-table-backed version is a natural, separate future
///   chunk if/when it matters.
/// - Keys/elements are only really safe at `Int`/`Float`/`Bool`/`Str` in
///   practice — the `[K: Eq]` bound doesn't actually ENFORCE this at the
///   type-checker level (a pre-existing `satisfies_bound` gap, not fixed
///   here): a struct/array/tuple key will still type-check but then
///   fail at codegen/runtime.
/// - No `println`/`.to_string()` support for `Map`/`Set` values — still
///   scoped to `Int`/`Float`/`Bool`/`Str` per chunk 1.
///
/// **Chunk 4 addition: `Set` algebra (`set_union`/`set_intersection`/
/// `set_difference`), `set_from_array`, `map_from_arrays`.** All built
/// from the existing primitives above, no new backend work. Two real,
/// previously-unknown compiler bugs were found and explicitly NOT
/// worked around while building this — deferred to their own future
/// chunk (now fixed — see "Chunk 5" below), not silently patched or
/// hidden:
/// 1. An empty array literal (`[]`) passed as an argument INTO a
///    generic function hit an internal codegen error ("an empty array
///    literal reached the non-empty array-literal codegen path") —
///    even with an explicit type annotation forcing it concrete
///    beforehand. This blocked any `Map`/`Set` -> FRESH `Array`
///    conversion (`map_keys`/`map_values`/`set_to_array`) that would
///    need to start from `[]` inside a generic function.
/// 2. A closure passed to `.fold()` that calls a CURRIED (multi-param)
///    function produced invalid LLVM IR — `clang` rejected it outright
///    (`cannot guarantee tail call due to mismatched parameter
///    counts`). `set_from_array`/`map_from_arrays` below use a plain
///    index-based recursive loop (no closure) specifically to route
///    around this — kept even after the fix landed, since it's still
///    simple, correct, and was already proven working; not worth
///    churning back to a `.fold()`-based form with no functional
///    benefit.
///
/// **Chunk 5: both bugs fixed.** Bug 1 (`musttail` from inside a
/// closure body): `Ctx::is_closure_body`, a new field disallowing
/// `musttail` unconditionally from a closure body's own tail calls —
/// see `plum-codegen/src/codegen.rs`'s own doc comment on that field
/// for the full root-cause writeup. Bug 2 (empty array literal across
/// a generic boundary) needed TWO distinct fixes, both landed: (a)
/// `monomorphize::plan` now threads `empty_array_elem_types` through
/// exactly like `closure_types` already was (the plumbing gap that
/// caused the ORIGINAL reported failure); (b) `Infer::empty_array_
/// elem_types`/`resolve_empty_array_elem_types` gained a genuine tier-2
/// template fallback mirroring `resolve_closure_types`'s existing one
/// (reusing `resolve_closure_component` directly), plus a matching
/// `extra_empty_array_elem_types` per-instantiation side-channel in
/// `monomorphize.rs`, so `let f[T](): Array[T] = []` — an empty array
/// literal pinned only to ITS OWN enclosing generic function's type
/// param — now resolves correctly instead of a hard ambiguity error.
/// `map_keys`/`map_values`/`set_to_array` below are the direct,
/// concrete payoff: exactly the shape both bugs used to block.
const STDLIB_COLLECTIONS_SRC: &str = "\
enum Map[K, V] { MapNode(K, V, Map[K, V]), MapEnd }

let map_new[K, V] (): Map[K, V] = MapEnd

let map_insert[K: Eq, V] (m: Map[K, V]) (k: K) (v: V): Map[K, V] =
    MapNode(k, v, m)

let map_get[K: Eq, V] (m: Map[K, V]) (k: K): Option[V] = match m {
    MapNode(k2, v, rest) => if k == k2 { Some(v) } else { map_get(rest, k) },
    MapEnd => None,
}

let map_contains[K: Eq, V] (m: Map[K, V]) (k: K): Bool = match m {
    MapNode(k2, _, rest) => if k == k2 { true } else { map_contains(rest, k) },
    MapEnd => false,
}

let map_remove[K: Eq, V] (m: Map[K, V]) (k: K): Map[K, V] = match m {
    MapNode(k2, v, rest) => if k == k2 { rest } else { MapNode(k2, v, map_remove(rest, k)) },
    MapEnd => MapEnd,
}

let map_len[K, V] (m: Map[K, V]): Int = match m {
    MapNode(_, _, rest) => 1 + map_len(rest),
    MapEnd => 0,
}

enum Set[T] { SetNode(T, Set[T]), SetEnd }

let set_new[T] (): Set[T] = SetEnd

let set_contains[T: Eq] (s: Set[T]) (x: T): Bool = match s {
    SetNode(y, rest) => if x == y { true } else { set_contains(rest, x) },
    SetEnd => false,
}

let set_insert[T: Eq] (s: Set[T]) (x: T): Set[T] =
    if set_contains(s, x) { s } else { SetNode(x, s) }

let set_remove[T: Eq] (s: Set[T]) (x: T): Set[T] = match s {
    SetNode(y, rest) => if x == y { rest } else { SetNode(y, set_remove(rest, x)) },
    SetEnd => SetEnd,
}

let set_len[T] (s: Set[T]): Int = match s {
    SetNode(_, rest) => 1 + set_len(rest),
    SetEnd => 0,
}

let set_union[T: Eq] (a: Set[T]) (b: Set[T]): Set[T] = match a {
    SetNode(x, rest) => set_union(rest, set_insert(b, x)),
    SetEnd => b,
}

let set_intersection_acc[T: Eq] (a: Set[T]) (b: Set[T]) (acc: Set[T]): Set[T] = match a {
    SetNode(x, rest) => if set_contains(b, x) { set_intersection_acc(rest, b, set_insert(acc, x)) } else { set_intersection_acc(rest, b, acc) },
    SetEnd => acc,
}
let set_intersection[T: Eq] (a: Set[T]) (b: Set[T]): Set[T] = set_intersection_acc(a, b, set_new(()))

let set_difference_acc[T: Eq] (a: Set[T]) (b: Set[T]) (acc: Set[T]): Set[T] = match a {
    SetNode(x, rest) => if set_contains(b, x) { set_difference_acc(rest, b, acc) } else { set_difference_acc(rest, b, set_insert(acc, x)) },
    SetEnd => acc,
}
let set_difference[T: Eq] (a: Set[T]) (b: Set[T]): Set[T] = set_difference_acc(a, b, set_new(()))

let set_from_array_acc[T: Eq] (arr: Array[T]) (i: Int) (acc: Set[T]): Set[T] =
    if i >= arr.len() { acc } else { set_from_array_acc(arr, i + 1, set_insert(acc, arr[i])) }
let set_from_array[T: Eq] (arr: Array[T]): Set[T] = set_from_array_acc(arr, 0, set_new(()))

let map_from_arrays_acc[K: Eq, V] (keys: Array[K]) (values: Array[V]) (i: Int) (acc: Map[K, V]): Map[K, V] =
    if i >= keys.len() { acc } else { map_from_arrays_acc(keys, values, i + 1, map_insert(acc, keys[i], values[i])) }
let map_from_arrays[K: Eq, V] (keys: Array[K]) (values: Array[V]): Map[K, V] =
    map_from_arrays_acc(keys, values, 0, map_new(()))

let map_keys[K, V] (m: Map[K, V]): Array[K] = match m {
    MapNode(k, _, rest) => map_keys(rest).push(k),
    MapEnd => [],
}

let map_values[K, V] (m: Map[K, V]): Array[V] = match m {
    MapNode(_, v, rest) => map_values(rest).push(v),
    MapEnd => [],
}

let set_to_array[T] (s: Set[T]): Array[T] = match s {
    SetNode(x, rest) => set_to_array(rest).push(x),
    SetEnd => [],
}
";

/// Parses the prelude + stdlib sources once and prepends their items
/// to `program`'s own — items earlier in the list are declared FIRST,
/// but `TypeContext`'s two-phase construction (see its doc comment)
/// already makes declaration order not matter for name resolution. A
/// user program declaring its OWN `Option`/`Result`/`println` (or
/// anything else with the SAME name) is now a real "already declared"
/// error, same as redeclaring any other name — see `TypeContext::
/// from_items`'s duplicate-name check.
pub(crate) fn with_prelude(program: ast::Program) -> ast::Program {
    let mut items = Vec::new();
    let mut base = 0usize;
    for src in [PRELUDE_SRC, STDLIB_IO_SRC, STDLIB_FILE_SRC, STDLIB_COLLECTIONS_SRC] {
        let tokens = Lexer::with_base_offset(src, base).tokenize();
        let parsed_items = Parser::new(tokens)
            .parse_program()
            .unwrap_or_else(|e| panic!("prelude/stdlib source is fixed, valid Plum: {e}"))
            .items;
        items.extend(parsed_items);
        base += src.len();
    }
    items.extend(program.items);
    ast::Program { items }
}

/// The combined byte length of every prelude/stdlib source fragment
/// `with_prelude` merges in, in the SAME order — every entry point that
/// lexes a user's OWN top-level Plum source (as opposed to a fragment
/// `with_prelude` itself already offsets) must start ITS OWN `Lexer` at
/// this base offset, so its `Span`s can never collide with a prelude
/// fragment's — see `Lexer::base`'s own doc comment for why this
/// matters at all (a real, previously-latent bug found while adding
/// `STDLIB_FILE_SRC`: two UNRELATED prelude fragments' call sites
/// coincidentally landed at the same byte offset, silently colliding in
/// `Infer::generic_sites`, a `HashMap<Span, _>`). A compile-time
/// constant — no lexing needed to compute it, just summed `&str` byte
/// lengths.
const PRELUDE_TOTAL_LEN: usize = PRELUDE_SRC.len() + STDLIB_IO_SRC.len() + STDLIB_FILE_SRC.len() + STDLIB_COLLECTIONS_SRC.len();

/// Runs the whole pipeline — parse, type-check, lower, optimize, load,
/// call — and returns the result of calling `fn_name` with `args`.
///
/// Type-checking is a hard gate: a program that fails to type-check is
/// rejected here and never reaches lowering or the interpreter at all.
/// Before this function existed, `plum-types` was fully implemented and
/// tested but nothing in the compiler ever called it — a type error
/// (like adding a `Bool` to an `Int`) only ever surfaced as a confusing
/// runtime error deep in `Interpreter::eval`, if it surfaced at all.
pub fn typecheck_and_run(src: &str, fn_name: &str, args: Vec<Value>) -> Result<Value, String> {
    // Base-offset the user's own source PAST every prelude fragment's
    // span range — see `PRELUDE_TOTAL_LEN`'s own doc comment for why.
    let tokens = Lexer::with_base_offset(src, PRELUDE_TOTAL_LEN).tokenize();
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program().map_err(|e| format!("parse error: {e}"))?;
    let program = with_prelude(program);
    run_resolved_program(program, fn_name, args)
}

/// The shared back half of both `typecheck_and_run` (single-file, no
/// module qualification — used exactly as before) and `modules::
/// typecheck_and_run_modules` (which hands this a single MERGED,
/// already-fully-qualified `ast::Program` — see that module's doc
/// comment for how cross-module names get folded into one flat
/// program before reaching here). Everything below this point has
/// ALWAYS operated on a single flat `ast::Program` with no module
/// concept of its own — that stays true; module resolution is
/// entirely a pre-pass that happens before this function ever runs.
pub(crate) fn run_resolved_program(program: ast::Program, fn_name: &str, args: Vec<Value>) -> Result<Value, String> {
    let type_ctx = TypeContext::from_items(&program.items).map_err(|e| format!("type error: {e}"))?;
    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(&program).map_err(|e| format!("type error: {e}"))?;

    // A second, independent static gate — DESIGN.md's "channel send is
    // a move": reusing a value after `tx.send(v)` is a compile error.
    // Runs on the AST (see `movecheck`'s own doc comment for why), so
    // it doesn't need to wait for lowering; placed after type-checking
    // simply to keep the "cheapest/most-fundamental check first" order,
    // not because either gate depends on the other.
    plum_ir::movecheck::check_moves(&program).map_err(|e| format!("move error: {e}"))?;

    // `p.x` needs to know WHICH struct `p` is to lower correctly —
    // lowering has no type information of its own, so this carries
    // inference's own answer across as a span-keyed side-channel. See
    // `Infer::field_owners`/`LoweringContext::field_owners`'s doc
    // comments for the full reasoning.
    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone());
    let ir_program = lower_program(&program, &lowering_ctx).map_err(|e| format!("lowering error: {e}"))?;
    let ir_program = optimize_program(ir_program);

    let mut interp = Interpreter::new();
    interp.set_struct_field_names(lowering_ctx.struct_fields().clone());
    interp.load_program(&ir_program).map_err(|e| format!("load error: {e}"))?;
    interp.call(fn_name, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn println_is_available_with_no_declaration_of_its_own_through_the_interpreter() {
        // Mirrors the existing `the_prelude_option_type_is_available_
        // with_no_declaration_of_its_own`-style tests for `println`:
        // proves it's reachable via `with_prelude` with no `use`/
        // declaration, and that calling it for every `.to_string()`-
        // supported type actually succeeds end to end through the real
        // interpreter (extern call via `libffi`, not just type-
        // checking). Real `puts()` output goes to the OS's actual
        // stdout, not something a Rust-level assertion can capture here
        // (confirmed no existing interpreter-path extern test tries to
        // — they all assert successful execution/return values, same
        // as this one does) — visually confirmed by hand during design
        // that real `42`/`hi` lines do appear on stdout when run.
        let src = "let go (): Int = { println(42); println(3.5); println(true); println(\"hi\"); 0 }";
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0)));
    }

    #[test]
    fn print_is_available_with_no_declaration_of_its_own_through_the_interpreter() {
        // Same shape as `println`'s own interpreter-path test above —
        // `print` uses a real `write(2)` syscall via `libffi`, not
        // `puts`; proves successful execution end to end (same
        // limitation on capturing real stdout applies here too — see
        // that test's own comment).
        let src = "let go (): Int = { print(\"hi\"); print(\" there\"); 0 }";
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0)));
    }

    // --- standard library: Str equality / Map / Set, interpreter path ---
    //
    // Mirrors chunk 1's "prove both backends independently" pattern:
    // the interpreter's own `values_equal` already handled `Str`
    // correctly before this chunk (only the LLVM backend needed a real
    // fix), but these still prove the FULL `with_prelude`-injected
    // `Map`/`Set` stdlib source works end to end through the
    // interpreter, independently of the native-backend proof in
    // `codegen_cli::tests`.

    #[test]
    fn str_equality_and_inequality_work_through_the_interpreter() {
        let src = "let go (): Bool = \"abc\" == \"abc\" && \"abc\" != \"abd\"";
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn map_basic_insert_get_contains_remove_work_through_the_interpreter() {
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 100), 2, 200);\n\
                let got = match map_get(m, 2) { Some(v) => v, None => -1 };\n\
                let has1 = map_contains(m, 1);\n\
                let m2 = map_remove(m, 2);\n\
                let has2_after = map_contains(m2, 2);\n\
                got + (if has1 { 10 } else { 0 }) + (if has2_after { 1000 } else { 0 })\n\
            }\n\
        ";
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(210)));
    }

    #[test]
    fn set_basic_insert_dedupe_contains_remove_len_work_through_the_interpreter() {
        let src = "\
            let go (): Int = {\n\
                let s = set_insert(set_insert(set_insert(set_new(()), \"x\"), \"x\"), \"y\");\n\
                let n = set_len(s);\n\
                let has_x = set_contains(s, \"x\");\n\
                let s2 = set_remove(s, \"x\");\n\
                let has_x_after = set_contains(s2, \"x\");\n\
                n * 100 + (if has_x { 10 } else { 0 }) + (if has_x_after { 1 } else { 0 })\n\
            }\n\
        ";
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        // n = 2 (dedup), has_x = true, has_x_after = false.
        assert_eq!(result, Ok(Value::Int(210)));
    }

    #[test]
    fn set_algebra_and_map_from_arrays_work_through_the_interpreter() {
        let src = "\
            let go (): Int = {\n\
                let a = set_from_array([1, 2, 2, 3]);\n\
                let b = set_from_array([2, 3, 4]);\n\
                let m = map_from_arrays([1, 2], [10, 20]);\n\
                let got = match map_get(m, 2) { Some(v) => v, None => -1 };\n\
                set_len(set_union(a, b)) * 100 + set_len(set_intersection(a, b)) * 10 + set_len(set_difference(a, b)) + got\n\
            }\n\
        ";
        // union len 4, intersection len 2, difference len 1, got = 20.
        // Total: 4*100 + 2*10 + 1 + 20 = 441.
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(441)));
    }

    #[test]
    fn map_keys_map_values_and_set_to_array_work_through_the_interpreter() {
        // The interpreter was never affected by either chunk-5 bug
        // (neither `monomorphize.rs`'s missing plumbing nor `Infer`'s
        // missing tier-2 template fallback are ever reached from this
        // path — `run_resolved_program` never calls `resolve_empty_
        // array_elem_types`/`monomorphize::plan` at all), but this
        // still proves the new stdlib functions themselves work
        // end-to-end here too, independently of the native-backend
        // proof in `codegen_cli::tests`.
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 10), 2, 20);\n\
                let ks = map_keys(m);\n\
                let vs = map_values(m);\n\
                let s = set_to_array(set_from_array([1, 2, 3]));\n\
                ks[0] + ks[1] + vs[0] + vs[1] + s.len() * 100\n\
            }\n\
        ";
        let result = typecheck_and_run(src, "go", vec![Value::Unit]);
        // ks = [1, 2], vs = [10, 20] (map_keys/map_values recurse to
        // the tail first, so the OLDEST-inserted key/value ends up
        // FIRST in the resulting array), s.len() = 3.
        // Total: 1 + 2 + 10 + 20 + 300 = 333.
        assert_eq!(result, Ok(Value::Int(333)));
    }

    #[test]
    fn well_typed_recursive_program_runs_through_the_full_gated_pipeline() {
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        let result = typecheck_and_run(src, "sum", vec![Value::Int(5), Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(15)));
    }

    #[test]
    fn a_unit_param_entry_point_runs_through_the_full_gated_pipeline() {
        // `let main () = ...` — the entry-point convention `plumc`'s
        // own CLI (main.rs) and `examples/overview.plum`'s `example()`
        // both use; this was broken end to end (both type-checking and
        // lowering independently rejected the empty-tuple/Unit pattern)
        // until fixed alongside the module-system CLI work.
        let result = typecheck_and_run("let main () = 21 + 21", "main", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn extern_call_inside_unsafe_runs_through_the_full_gated_pipeline() {
        let src = r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let main x = unsafe { sqrt(x) }
        "#;
        let result = typecheck_and_run(src, "main", vec![Value::Float(9.0)]);
        assert_eq!(result, Ok(Value::Float(3.0)));
    }

    #[test]
    fn extern_call_outside_unsafe_is_rejected_before_it_ever_reaches_the_interpreter() {
        let src = r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let main x = sqrt(x)
        "#;
        let err = typecheck_and_run(src, "main", vec![Value::Float(9.0)]).expect_err("expected a type error");
        assert!(err.contains("unsafe"), "unexpected error: {err}");
    }

    #[test]
    fn extern_call_with_a_cstr_argument_runs_through_the_full_gated_pipeline() {
        let src = r#"
            extern "C" {
                fn strlen(s: CStr) -> Int;
            }
            let go unused = unsafe { strlen("hello".as_cstr()) }
        "#;
        let result = typecheck_and_run(src, "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn extern_call_returning_a_struct_by_value_runs_through_the_full_gated_pipeline() {
        // Real libc `div(int, int) -> div_t` (`div_t { int quot; int
        // rem; }`), resolved through the REAL `Library::this()` symbol
        // path `Interpreter::load_program` always uses (unlike plum-
        // interp's own struct tests, which bypass resolution entirely
        // via a direct `ExternFnHandle`, an escape hatch this crate
        // doesn't have access to). `Bool` is used for `quot`/`rem`
        // rather than `Int` DELIBERATELY — `div_t`'s fields are C
        // `int` (4 bytes), matching `ExternType::Bool`'s C-ABI mapping,
        // not `ExternType::Int` (8 bytes, C `long long`) — a real,
        // semantically-odd-looking-but-ABI-necessary consequence of
        // v1's fixed-width scope (see DESIGN.md's "FFI and C interop"
        // section). This is genuinely testing our layout math against
        // an unrelated, real, externally-verified C ABI struct, not
        // just our own Rust mirror of one.
        let src = r#"
            struct DivResult { quot: Bool, rem: Bool }
            extern "C" {
                fn div(numer: Bool, denom: Bool) -> DivResult;
            }
            let go unused = unsafe { div(true, true) }
        "#;
        let result = typecheck_and_run(src, "go", vec![Value::Int(0)]);
        assert!(result.is_ok(), "expected the pipeline to accept and run this program, got: {result:?}");
    }

    #[test]
    fn passing_an_ordinary_str_where_extern_expects_cstr_is_rejected_before_it_ever_reaches_the_interpreter() {
        let src = r#"
            extern "C" {
                fn strlen(s: CStr) -> Int;
            }
            let go unused = unsafe { strlen("hello") }
        "#;
        let err = typecheck_and_run(src, "go", vec![Value::Int(0)]).expect_err("expected a type error");
        assert!(err.contains("type error"), "unexpected error: {err}");
    }

    #[test]
    fn callback_argument_as_a_closure_literal_is_rejected_before_it_ever_reaches_the_interpreter() {
        // Real symbol resolution can't be exercised end-to-end here the
        // way the CStr/struct tests do (no real, already-dynamically-
        // linked libc function takes a plain Int/Float/Bool-signature
        // callback — every realistic C callback API deals in pointers,
        // outside v1's scope) — but the REJECTION path needs no symbol
        // resolution at all, since it fails at type-checking, before
        // lowering or the interpreter ever run. The successful, real-
        // trampoline-invocation path is covered exhaustively in
        // plum-interp's own tests instead.
        let src = r#"
            extern "C" {
                fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int;
            }
            let go unused = unsafe { call_with_10_and_20(|a, b| a + b) }
        "#;
        let err = typecheck_and_run(src, "go", vec![Value::Int(0)]).expect_err("expected a type error");
        assert!(err.contains("bare reference"), "unexpected error: {err}");
    }

    #[test]
    fn ill_typed_program_is_rejected_before_it_ever_reaches_the_interpreter() {
        // `n` is inferred Int from `n == 0`, so passing it to `want_bool`
        // (which requires a Bool) is a real type error. Before wiring,
        // this would have been silently accepted here (since nothing
        // called plum-types) and only misbehaved once actually run.
        let src = "let want_bool b = if b { 1 } else { 0 }\n\
                    let bad n = if n == 0 { want_bool(n) } else { 0 }";
        let err = typecheck_and_run(src, "bad", vec![Value::Int(1)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn parse_errors_are_still_reported_as_such_not_type_errors() {
        let err = typecheck_and_run("let (((", "f", vec![]).expect_err("expected a parse error");
        assert!(err.starts_with("parse error:"), "expected a parse error, got: {err}");
    }

    #[test]
    fn function_bodies_are_fbip_optimized_before_running() {
        // Construct-then-immediately-match-and-reconstruct is exactly
        // the reuse-in-place shape FBIP recognizes (`CtorReuse`). This
        // proves it fires correctly for a NAMED function loaded via
        // `load_program` and invoked via `call` — not just for a single
        // expression handed straight to `eval`, which is all the
        // capstone tests in plum-interp exercised before `optimize_program`
        // existed. If FBIP weren't wired in, this would still produce
        // the right VALUE (2) since reuse is a memory optimization, not
        // a behavior change — so this test is really about the pipeline
        // not panicking/erroring on the RcAnnotated/CtorReuse nodes it
        // now actually loads and evaluates.
        let src = "struct Point { x: Int, y: Int }\n\
                    let run dummy = match (match (Point { x: 1, y: 2 }) { Point(x, y) => Point { x: y, y: x } }) { Point(a, b) => a }";
        let result = typecheck_and_run(src, "run", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn for_and_unsafe_run_through_the_full_gated_pipeline() {
        // `for`/`unsafe` are the newest lowered forms — this proves
        // they're accepted by the type-check gate (not just runnable if
        // it were skipped) AND actually execute correctly together.
        let src = "let count n = unsafe { for i in 0..n { i } }";
        let result = typecheck_and_run(src, "count", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Unit));
    }

    #[test]
    fn closures_run_through_the_full_gated_pipeline_including_as_arguments() {
        let src = "let apply f x = f(x)\n\
                    let use_it n = apply(|x| x + 1, n)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn the_classic_for_loop_accumulator_runs_through_the_full_gated_pipeline() {
        // DESIGN.md's own motivating example for `let mut`, proven all
        // the way through: parse -> type-check -> lower -> FBIP
        // optimize -> run.
        let src = "let sum_to n = { let mut sum = 0; for i in 0..n { sum = sum + i; }; sum }";
        let result = typecheck_and_run(src, "sum_to", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn for_over_an_array_bound_to_a_local_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let arr = [1, 2, 3, 4]; let mut sum = 0; for x in arr { sum = sum + x; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn for_over_an_array_literal_directly_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let mut sum = 0; for x in [10, 20, 30] { sum = sum + x; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(60)));
    }

    #[test]
    fn for_over_an_empty_array_runs_zero_iterations() {
        let src = "let use_it dummy = { let mut sum = 0; for x in [] { sum = sum + x; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0)));
    }

    #[test]
    fn array_pop_set_remove_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let a = [1, 2, 3]; a.pop().len() + a.set(0, 99)[0] + a.remove(1)[1] }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        // pop().len() = 2, set(0,99)[0] = 99, remove(1)[1] = 3
        assert_eq!(result, Ok(Value::Int(2 + 99 + 3)));
    }

    #[test]
    fn array_map_filter_fold_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = [1, 2, 3, 4, 5].map(|x| x * 2).filter(|x| x > 4).fold(0, |acc, x| acc + x)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        // [2,4,6,8,10] -> filter >4 -> [6,8,10] -> fold sum -> 24
        assert_eq!(result, Ok(Value::Int(24)));
    }

    #[test]
    fn array_map_can_change_the_element_type() {
        let src = "let use_it dummy = [1, 2, 3].map(|x| x > 1).filter(|b| b).len()";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn array_push_pop_set_remove_reuse_in_place_end_to_end() {
        // The reuse-in-place path (uniquely-owned `a` fed straight into
        // `.push()`/`.pop()`/`.set()`/`.remove()`) must produce the same
        // RESULTS as the always-copy path — reuse is purely a memory
        // optimization, never a semantic change.
        let src = "let use_it dummy = { let a = [1, 2, 3]; a.push(4).set(0, 99).remove(1).pop().len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        // [1,2,3] -> push 4 -> [1,2,3,4] -> set(0,99) -> [99,2,3,4] ->
        // remove(1) -> [99,3,4] -> pop -> [99,3] -> len -> 2
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn array_reused_after_a_reuse_in_place_operation_still_sees_its_original_contents() {
        // `a` is used again after `.push()` — the exact case that must
        // NOT reuse in place (see the plum-ir/plum-interp tests proving
        // this at the mark_reuse/heap-refcount level); this proves it
        // holds true through the entire real pipeline too.
        let src = "let use_it dummy = { let a = [1, 2, 3]; let b = a.push(4); a.len() + b.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn string_concat_and_len_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = \"hello, \".concat(\"world\").len()";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(12)));
    }

    #[test]
    fn string_equality_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = if \"abc\" == \"abc\" { 1 } else { 0 }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn string_concat_argument_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = \"abc\".concat(5)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_struct_field_can_hold_a_string_end_to_end() {
        let src = "struct Person { name: String, age: Int }\n\
                    let greet (p: Person) = \"hi \".concat(p.name)\n\
                    let use_it dummy = greet(Person { name: \"Ada\", age: 30 }).len()";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn string_reused_after_a_reuse_in_place_concat_still_sees_its_original_contents() {
        // Same "used again later, so reuse must not fire" regression
        // proof as arrays', now for the heap-backed string
        // representation.
        let src = "let use_it dummy = { let s = \"ab\"; let t = s.concat(\"cd\"); s.len() + t.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn string_indexing_and_runes_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let s = \"café\"; s.runes().len() * 10 + s[0] - 87 }";
        // runes().len() = 4 -> 40; s[0] = 'c' = 99; 99 - 87 = 12; 40+12 = 52
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(52)));
    }

    #[test]
    fn string_runes_can_be_iterated_with_for_over_arrays() {
        // Combines two chunks from tonight: `.runes()` (Array[Int]) fed
        // straight into `for x in arr` iteration.
        let src = "let use_it dummy = { let mut sum = 0; for r in \"abc\".runes() { sum = sum + r; }; sum }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int('a' as i64 + 'b' as i64 + 'c' as i64)));
    }

    #[test]
    fn string_index_out_of_bounds_is_a_runtime_error_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = \"abc\"[10]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("out of bounds"), "expected an out-of-bounds error, got: {err}");
    }

    #[test]
    fn indexing_with_a_non_int_index_is_rejected_before_running() {
        let src = "let use_it dummy = \"abc\"[true]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn string_trim_and_split_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let parts = \"  a, b, c  \".trim().split(\", \"); parts.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(3)));
    }

    #[test]
    fn string_split_parts_can_be_further_processed() {
        // Combines this evening's chunks: `.split()` produces
        // `Array[Str]`, which `.map()`/`.filter()`/`for` all already
        // handle for free, same as `.runes()`.
        let src = "let use_it dummy = { let mut total = 0; for part in \"ab,cde,f\".split(\",\") { total = total + part.len(); }; total }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(6)));
    }

    #[test]
    fn string_trim_reused_after_a_reuse_in_place_operation_still_sees_its_original_contents() {
        let src = "let use_it dummy = { let s = \"  ab  \"; let t = s.trim(); s.len() + t.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(8)));
    }

    #[test]
    fn split_argument_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = \"a,b\".split(5)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn string_case_conversion_and_predicates_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let s = \"Hello World\";\
                    if s.to_lower().contains(\"world\") && s.starts_with(\"Hello\") && s.ends_with(\"World\") { 1 } else { 0 } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn string_replace_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = \"2026-07-28\".replace(\"-\", \"/\") == \"2026/07/28\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn string_replace_reused_after_a_reuse_in_place_operation_still_sees_its_original_contents() {
        let src = "let use_it dummy = { let s = \"a-b\"; let t = s.replace(\"-\", \"_\"); s == \"a-b\" && t == \"a_b\" }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn to_string_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let n = 42; \"the answer is \".concat(n.to_string()) == \"the answer is 42\" }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn to_string_on_a_still_unresolved_generic_parameter_is_permitted_and_works_when_called() {
        // Regression coverage: `.to_string()` used inside a generic
        // function body (where the parameter's type isn't resolved
        // until a call site pins it) must type-check and run
        // correctly — this is exactly the shape `.map(|x| x.to_string())`
        // needs, just spelled out as an ordinary top-level function.
        let src = "let stringify x = x.to_string()\n\
                    let use_it dummy = stringify(42) == \"42\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn to_string_on_an_unsupported_type_reached_only_through_a_generic_parameter_is_a_runtime_error() {
        // The other half of the permissive-at-compile-time tradeoff:
        // `stringify`'s own body type-checks `x.to_string()` once,
        // while `x`'s type is still an unresolved `Var` (permissively
        // allowed — this check is never re-run per call-site
        // instantiation, since this isn't full parametric
        // generalization). Struct/enum/array/`Tuple` are all still
        // caught the SAME permissive way but now actually WORK at
        // runtime (the whole point of this chunk, `Tuple` included —
        // the interpreter itself has no tuple limitation, only
        // codegen does). A `Closure`, though, is excluded by the
        // `.to_string()` gate outright and genuinely unsupported by
        // the interpreter's own rendering — this is what still
        // demonstrates the runtime check catching what the
        // permissive compile-time path let through.
        let src = "let stringify x = x.to_string()\n\
                    let use_it dummy = stringify(|y| y)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("not yet supported"), "expected a not-yet-supported error, got: {err}");
    }

    #[test]
    fn to_string_composes_with_map_and_fold_to_build_a_string() {
        // Combines several of tonight's chunks: build a string out of
        // numbers via .map()/.to_string()/.concat()/.fold().
        let src = "let use_it dummy = [1, 2, 3].map(|x| x.to_string()).fold(\"\", |acc, s| acc.concat(s)) == \"123\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn ref_aliasing_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let a = ref(1); let b = a; b.set(99); a.get() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(99)));
    }

    #[test]
    fn ref_used_as_a_counter_through_a_for_loop() {
        // A real-world-shaped use case: a Ref accumulator threaded
        // through a `for` loop, mutated on every iteration.
        let src = "let use_it dummy = { let counter = ref(0); for i in 0..5 { counter.set(counter.get() + i); }; counter.get() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0 + 1 + 2 + 3 + 4)));
    }

    #[test]
    fn ref_set_argument_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = { let r = ref(5); r.set(true) }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_struct_field_can_hold_a_ref() {
        let src = "struct Counter { value: Ref[Int] }\n\
                    let increment (c: Counter) = c.value.set(c.value.get() + 1)\n\
                    let use_it dummy = { let c = Counter { value: ref(0) }; increment(c); increment(c); c.value.get() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(2)));
    }

    #[test]
    fn capturing_a_ref_across_a_spawn_boundary_is_rejected_at_runtime() {
        let src = "let use_it dummy = { let r = ref(5); spawn { r.get() }.join() }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("Ref"), "expected a Ref-boundary error, got: {err}");
    }

    #[test]
    fn literal_match_runs_through_the_full_gated_pipeline() {
        let src = "let classify n = match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" }\n\
                    let use_it dummy = classify(1) == \"one\" && classify(5) == \"many\"";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn literal_match_without_a_trailing_catchall_is_rejected_before_running() {
        let src = "let use_it dummy = match 1 { 0 => \"zero\", 1 => \"one\" }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn match_single_wildcard_arm_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 1, y: 2 }) { _ => 42 }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn trailing_catchall_mixed_into_a_variant_match_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let area shape = match shape { Circle(r) => 3.14 * r * r, _ => 0.0 }\n\
                    let use_it dummy = area(Square(5.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(0.0)));
    }

    #[test]
    fn non_last_catchall_mixed_into_a_variant_match_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { _ => 0.0, Circle(r) => r }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn or_pattern_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float), Square(Float), Triangle(Float) }\n\
                    let area shape = match shape { Circle(r) | Square(r) => r, Triangle(r) => 0.0 }\n\
                    let use_it dummy = area(Circle(3.0)) + area(Square(4.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(7.0)));
    }

    #[test]
    fn or_pattern_with_mismatched_bindings_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { Circle(v) | Square(w) => v }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_non_exhaustive_match_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { Circle(r) => r }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
        assert!(err.contains("Square"), "expected the missing variant named, got: {err}");
    }

    #[test]
    fn an_exhaustive_match_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let area shape = match shape { Circle(r) => 3.14 * r * r, Square(s) => s * s }\n\
                    let use_it dummy = area(Square(4.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(16.0)));
    }

    #[test]
    fn a_match_missing_none_on_the_prelude_option_is_rejected_before_running() {
        let src = "let use_it dummy = match Some(5) { Some(x) => x }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
        assert!(err.contains("None"), "expected None named as missing, got: {err}");
    }

    #[test]
    fn a_self_referential_global_closure_runs_through_the_full_gated_pipeline() {
        let src = "let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }\n\
                    let use_it dummy = fib(10)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(55)));
    }

    #[test]
    fn a_self_referential_local_closure_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }; fib(10) }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(55)));
    }

    #[test]
    fn a_range_for_loop_still_works_after_the_array_for_loop_side_channel_was_added() {
        // Regression coverage alongside `a_range_stored_and_passed_around_
        // runs_through_the_full_gated_pipeline` above: a genuinely
        // Range-typed, still-generic-at-the-point-of-inference loop
        // variable must not get misclassified as array-typed.
        let src = "let sum_range r = { let mut sum = 0; for i in r { sum = sum + i; }; sum }\n\
                    let use_it dummy = sum_range(0..5)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn globals_run_through_the_full_gated_pipeline() {
        let src = "let pi_ish = 3\nlet area r = pi_ish * r * r";
        let result = typecheck_and_run(src, "area", vec![Value::Int(2)]);
        assert_eq!(result, Ok(Value::Int(12)));
    }

    #[test]
    fn a_global_forward_reference_is_rejected_before_running() {
        let src = "let a = b\nlet b = 1";
        let err = typecheck_and_run(src, "a", vec![]).expect_err("expected a type error");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn nested_patterns_run_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 3, y: 4 }, 10) { (Point { x, y }, n) => x + y + n }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(17)));
    }

    #[test]
    fn variant_construction_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float) }\n\
                    let area shape = match shape { Circle(r) => r * r }\n\
                    let use_it dummy = area(Circle(3.0))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(9.0)));
    }

    #[test]
    fn struct_field_referencing_another_struct_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Line { start: Point, end: Point }\n\
                    let dx (Line { start: Point { x: x0, .. }, end: Point { x: x1, .. } }) = x1 - x0\n\
                    let use_it dummy = dx(Line { start: Point { x: 1, y: 0 }, end: Point { x: 9, y: 0 } })";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(8)));
    }

    #[test]
    fn struct_destructuring_params_run_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let area (Point { x, y }) = x * y\n\
                    let use_it dummy = area(Point { x: 3, y: 4 })";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(12)));
    }

    #[test]
    fn the_swap_example_runs_through_the_full_gated_pipeline() {
        // Tuples now have real type-checker support (previously they
        // could run at the interpreter level but were rejected by the
        // type-check gate) — this is the same flagship
        // `let swap (a, b) = (b, a)` example proven at the interpreter
        // level, now proven through the FULL pipeline including the
        // type-check gate.
        let src = "let swap (a, b) = (b, a)\n\
                    let use_it n = match swap((n, true)) { (x, y) => x }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(7)]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn struct_update_spread_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; let q = Point { x: 9, ..p }; match q { Point { x, y } => x + y } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(13)));
    }

    #[test]
    fn struct_update_spread_with_a_mismatched_struct_type_is_rejected_before_running() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Color { r: Int, g: Int, b: Int }\n\
                    let use_it dummy = { let c = Color { r: 1, g: 2, b: 3 }; Point { x: 9, ..c } }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn match_guards_run_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float) }\n\
                    let classify r = match (Circle(r)) { Circle(r) if r > 5.0 => 1, Circle(r) => 0 }";
        let result = typecheck_and_run(src, "classify", vec![Value::Float(10.0)]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn a_non_bool_match_guard_is_rejected_before_running() {
        let src = "enum Shape { Circle(Float) }\n\
                    let classify r = match (Circle(r)) { Circle(r) if r => 1, Circle(r) => 0 }";
        let err = typecheck_and_run(src, "classify", vec![Value::Float(10.0)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_range_stored_and_passed_around_runs_through_the_full_gated_pipeline() {
        let src = "let sum_range r = { let mut sum = 0; for i in r { sum = sum + i; }; sum }\n\
                    let use_it dummy = { let r = 0..5; sum_range(r) }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn for_over_a_non_range_value_is_rejected_before_running() {
        let src = "let use_it dummy = for i in 5 { i }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_correct_return_type_annotation_runs_through_the_full_gated_pipeline() {
        let src = "let square x: Int = x * x";
        let result = typecheck_and_run(src, "square", vec![Value::Int(4)]);
        assert_eq!(result, Ok(Value::Int(16)));
    }

    #[test]
    fn a_mismatched_return_type_annotation_is_rejected_before_running() {
        // Previously silently accepted: `ret_ty` was parsed but never
        // consulted by `plum-types` at all.
        let src = "let square x: Bool = x * x";
        let err = typecheck_and_run(src, "square", vec![Value::Int(4)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_bare_top_level_function_name_as_a_value_runs_through_the_full_gated_pipeline() {
        let src = "let square x = x * x\n\
                    let f = square\n\
                    let use_it n = f(n)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(25)));
    }

    #[test]
    fn a_bare_function_name_passed_as_a_higher_order_argument_runs_through_the_full_gated_pipeline() {
        let src = "let square x = x * x\n\
                    let apply f x = f(x)\n\
                    let use_it n = apply(square, n)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Int(4)]);
        assert_eq!(result, Ok(Value::Int(16)));
    }

    #[test]
    fn a_bare_variant_constructor_as_a_value_runs_through_the_full_gated_pipeline() {
        let src = "enum Shape { Circle(Float) }\n\
                    let apply f x = f(x)\n\
                    let use_it dummy = match apply(Circle, 5.0) { Circle(r) => r }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Float(5.0)));
    }

    #[test]
    fn field_access_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; p.x + p.y }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn field_access_on_a_function_call_result() {
        // Unlike a bare function PARAMETER (which has no annotation
        // syntax to pin its type — see this session's memory notes on
        // why `let f p = p.x` alone is correctly rejected as ambiguous
        // in a nominal type system with no row polymorphism), a
        // function's RETURN type is always fully determined from its
        // own body, independent of field access — so `.x` on a call
        // result works with no extra help.
        let src = "struct Point { x: Int, y: Int }\n\
                    let make_point dummy = Point { x: 5, y: 6 }\n\
                    let use_it dummy = make_point(dummy).x";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn chained_field_access_through_a_nested_struct() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Line { start: Point, end: Point }\n\
                    let use_it dummy = { let l = Line { start: Point { x: 1, y: 2 }, end: Point { x: 9, y: 9 } }; l.start.x }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1)));
    }

    #[test]
    fn field_access_on_an_unknown_field_is_rejected_before_running() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 1, y: 2 }; p.z }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn the_prelude_option_type_is_available_with_no_declaration_of_its_own() {
        // No `enum Option[T] { .. }` anywhere in `src` — DESIGN.md's
        // "no null, anywhere, ever" story means `Option`/`Result` are
        // always available, injected via `with_prelude` before this
        // source is even parsed against.
        let src = "let unwrap_or default o = match o { Some(x) => x, None => default }\n\
                    let use_it dummy = unwrap_or(0, Some(42))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn the_prelude_none_case_works_with_no_declaration_of_its_own() {
        let src = "let unwrap_or default o = match o { Some(x) => x, None => default }\n\
                    let use_it dummy = unwrap_or(7, None)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn the_prelude_result_type_is_available_with_no_declaration_of_its_own() {
        let src = "let unwrap_or default r = match r { Ok(x) => x, Err(e) => default }\n\
                    let use_it dummy = unwrap_or(0, Ok(42))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn the_prelude_result_err_case_works_with_no_declaration_of_its_own() {
        let src = "let unwrap_or default r = match r { Ok(x) => x, Err(e) => default }\n\
                    let use_it dummy = unwrap_or(0, Err(true))";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(0)));
    }

    #[test]
    fn a_program_redeclaring_option_itself_is_now_a_real_error() {
        // Previously silently shadowed the prelude's `Option` (no
        // duplicate-declaration detection existed anywhere) — now that
        // detection exists, redeclaring a prelude type is caught the
        // same as redeclaring anything else.
        let src = "enum Option[T] { Some(T), None, Neither }\n\
                    let use_it dummy = match (Neither) { Some(x) => 1, None => 2, Neither => 3 }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }

    // --- standard library: basic file I/O (see `plumc::STDLIB_FILE_SRC`) ---
    //
    // The FIRST chunk where any Plum program touches a real file on
    // disk — `std::env::temp_dir()` + a unique per-test filename,
    // mirroring `codegen_cli::unique_temp_dir`'s own naming convention.

    #[test]
    fn write_file_then_read_file_round_trips_through_the_interpreter() {
        let path = std::env::temp_dir().join(format!("plum-file-io-{}-a.txt", std::process::id()));
        let path_str = path.to_str().unwrap();
        let src = format!(
            "let use_it dummy = {{ \
                let w = write_file(\"{path_str}\", \"hello file io\"); \
                match w {{ \
                    Ok(_) => match read_file(\"{path_str}\") {{ Ok(s) => s == \"hello file io\", Err(_) => false }}, \
                    Err(_) => false \
                }} \
            }}"
        );
        let result = typecheck_and_run(&src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_file_on_a_nonexistent_path_returns_err_through_the_interpreter() {
        let path = std::env::temp_dir().join(format!("plum-file-io-{}-missing.txt", std::process::id()));
        let src = format!(
            "let use_it dummy = match read_file(\"{}\") {{ Ok(_) => false, Err(_) => true }}",
            path.to_str().unwrap()
        );
        let result = typecheck_and_run(&src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn write_file_to_an_invalid_path_returns_err_through_the_interpreter() {
        let src = "let use_it dummy = match write_file(\"/plum_test_nonexistent_dir_xyz/f.txt\", \"x\") { \
                    Ok(_) => false, Err(_) => true }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Bool(true)));
    }

    #[test]
    fn a_generic_struct_runs_through_the_full_gated_pipeline() {
        let src = "struct Pair[T] { first: T, second: T }\n\
                    let use_it dummy = { let p = Pair { first: 3, second: 4 }; match p { Pair(a, b) => a + b } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn mismatched_generic_type_arguments_are_rejected_before_running() {
        let src = "struct Pair[T] { first: T, second: T }\n\
                    let use_it dummy = Pair { first: 1, second: true }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn spawn_and_join_run_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; let t = spawn { match p { Point(a, b) => a + b } }; t.join() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn channel_send_and_recv_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); tx.send(42); rx.recv() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn a_value_crosses_a_real_thread_boundary_via_spawn_and_a_channel() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let (tx, rx) = channel[Point]();\
                    let t = spawn { tx.send(Point { x: 5, y: 6 }) };\
                    let p = rx.recv();\
                    t.join();\
                    match p { Point(a, b) => a + b } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(11)));
    }

    #[test]
    fn sending_the_wrong_element_type_is_rejected_before_running() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); tx.send(true) }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_bound_satisfying_generic_struct_runs_through_the_full_gated_pipeline() {
        let src = "struct Box[T: Num] { val: T }\n\
                    let use_it dummy = match (Box { val: 5 }) { Box(v) => v }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn a_bound_violating_generic_struct_is_rejected_before_running() {
        let src = "struct Box[T: Num] { val: T }\n\
                    let use_it dummy = Box { val: true }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_redeclared_function_is_rejected_before_running() {
        let src = "let square x = x * x\nlet square x = x + x\nlet use_it dummy = square(3)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn using_a_channel_send_value_afterward_is_rejected_before_running() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); let p = 5; tx.send(p); p }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a move error, not a successful run");
        assert!(err.starts_with("move error:"), "expected a move error, got: {err}");
    }

    #[test]
    fn sending_a_value_and_never_reusing_it_runs_normally() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); let p = 5; tx.send(p); rx.recv() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn a_correctly_annotated_parameter_runs_through_the_full_gated_pipeline() {
        let src = "let square (x: Int) = x * x";
        let result = typecheck_and_run(src, "square", vec![Value::Int(6)]);
        assert_eq!(result, Ok(Value::Int(36)));
    }

    #[test]
    fn a_mismatched_parameter_annotation_is_rejected_before_running() {
        let src = "let use_it (x: Bool) = x + 1";
        let err = typecheck_and_run(src, "use_it", vec![Value::Bool(true)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_struct_typed_parameter_annotation_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let dx (p: Point) = match p { Point(a, b) => a }\n\
                    let use_it dummy = dx(Point { x: 7, y: 0 })";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn an_array_typed_parameter_annotation_runs_through_the_full_gated_pipeline() {
        // This is the gap flagged when `for x in arr` landed: `arr`'s
        // type couldn't previously be pinned via an explicit `Array[T]`
        // annotation, only inferred structurally.
        let src = "let sum_array (arr: Array[Int]) = { let mut sum = 0; for x in arr { sum = sum + x; }; sum }\n\
                    let use_it dummy = sum_array([1, 2, 3, 4])";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(10)));
    }

    #[test]
    fn a_mismatched_array_typed_parameter_annotation_is_rejected_before_running() {
        let src = "let sum_array (arr: Array[Int]) = arr.len()\n\
                    let use_it dummy = sum_array([true, false])";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn an_array_typed_return_annotation_runs_through_the_full_gated_pipeline() {
        let src = "let doubled (arr: Array[Int]): Array[Int] = arr.map(|x| x * 2)\n\
                    let use_it dummy = doubled([1, 2, 3])[1]";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(4)));
    }

    #[test]
    fn a_generic_function_annotation_runs_through_the_full_gated_pipeline() {
        let src = "let identity[T] (x: T): T = x\nlet use_it dummy = identity(42)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn mismatched_shared_generic_parameters_are_rejected_before_running() {
        let src = "let pair[T] (a: T) (b: T): T = a\nlet use_it dummy = pair(1, true)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn a_bound_satisfying_generic_function_call_runs_through_the_full_gated_pipeline() {
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(21)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(21)));
    }

    #[test]
    fn a_bound_violating_generic_function_call_is_rejected_before_running() {
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(true)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn select_runs_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let (tx1, rx1) = channel[Int]();\
                    let (tx2, rx2) = channel[Int]();\
                    tx2.send(7);\
                    select { v = rx1.recv() => v, w = rx2.recv() => w } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn select_waits_on_a_spawned_task_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int]();\
                    let t = spawn { tx.send(11) };\
                    let v = select { v = rx.recv() => v };\
                    t.join();\
                    v }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(11)));
    }

    #[test]
    fn select_arms_with_mismatched_result_types_are_rejected_before_running() {
        let src = "let use_it dummy = { let (tx, rx) = channel[Int](); tx.send(1);\
                    select { v = rx.recv() => v, w = rx.recv() => true } }";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn array_construction_indexing_and_push_run_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = { let a = [1, 2, 3]; let b = a.push(4); a[0] + b[3] + b.len() }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(1 + 4 + 4)));
    }

    #[test]
    fn an_array_of_structs_runs_through_the_full_gated_pipeline() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let arr = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];\
                    match arr[1] { Point(a, b) => a + b } }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Int(7)));
    }

    #[test]
    fn array_element_type_mismatch_is_rejected_before_running() {
        let src = "let use_it dummy = [1, true]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn array_index_out_of_bounds_is_a_runtime_error_through_the_full_gated_pipeline() {
        let src = "let use_it dummy = [1, 2, 3][10]";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a runtime error, not a successful run");
        assert!(err.contains("out of bounds"), "expected an out-of-bounds error, got: {err}");
    }

    #[test]
    fn joining_a_non_task_value_is_rejected_before_running() {
        let src = "let use_it dummy = 5.join()";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }

    #[test]
    fn for_loop_with_mistyped_range_bounds_is_rejected_before_running() {
        let src = "let bad n = for i in true..n { i }";
        let err = typecheck_and_run(src, "bad", vec![Value::Int(5)])
            .expect_err("expected a type error, not a successful run");
        assert!(err.starts_with("type error:"), "expected a type error, got: {err}");
    }
}
