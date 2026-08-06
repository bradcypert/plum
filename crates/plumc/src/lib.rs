mod assoc_fns;
mod codegen_cli;
mod diagnostics;
mod modules;
mod project;
#[cfg(test)]
mod test_util;
mod testing;
pub use codegen_cli::{
    compile_and_run, compile_ir_to_binary, compile_program_to_ir, compile_program_to_ir_diag, emit_main, reject_unprintable_return, CgValue,
};
pub use diagnostics::ModuleSources;
pub use modules::{resolve_modules, typecheck_and_run_modules};
pub use project::{
    collect_project_files, resolve_project, resolve_project_diag, typecheck_and_run_project, typecheck_and_run_project_diag,
};
pub use testing::{discover_tests, run_tests_interpreted, run_tests_native, TestOutcome};

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

/// JSON parsing/serialization — chunk 9 of the standard library, pure
/// Plum prelude source (no new IR/codegen work at all, unlike file
/// I/O's `read` primitive — everything here is expressible with
/// existing recursion, `Array`/`Str` operations, generic structs, and
/// self-referential enums). `JsonEntry` (a dedicated struct for object
/// key-value pairs), not `Tuple[Str, JsonValue]`, mirrors `Map[K,V]`'s
/// own established precedent exactly — `CgType` has no `Tuple`
/// variant, the same reason `Map`/`Set` are recursive generic enums,
/// not `Array[Tuple[K,V]]`.
///
/// There is no substring/slice primitive in Plum at all, and no
/// `Int`-to-`Str` (codepoint-to-one-character) builder either — parsing
/// works over `chars_of(s): Array[Str]` (one-character `Str` elements
/// via `s.split("").filter(...)`) instead of `.runes(): Array[Int]`,
/// sidestepping both gaps: every structural/content comparison becomes
/// an ordinary `Str == Str` check, and building output is ordinary
/// `.concat()`. Number parsing accumulates directly in `Float` (`digit_
/// value` maps each of `"0"`..`"9"` straight to a `Float` literal), so
/// no `Int`-to-`Float` cast is ever needed either (none exists).
///
/// Escaping scope is deliberately narrower than full JSON, for the
/// identical "no codepoint-to-string" reason, and documented as such —
/// matching this project's established "narrow, honest gap" style:
/// only `\"`, `\\`, `\/`, `\n`, `\r`, `\t` are supported (exactly the
/// escapes Plum's OWN string-literal lexer can itself produce/consume).
/// `\b`, `\f`, and `\uXXXX` unicode escapes are NOT supported —
/// `json_parse` returns a clear `Err` on one rather than mishandling
/// it silently; `json_stringify` never emits `\uXXXX` and leaves raw
/// control characters below `\n`/`\r`/`\t` un-escaped.
///
/// No max-nesting-depth guard (a deliberate choice, confirmed with the
/// user) — pathological deeply-nested input could in principle
/// overflow the native stack (recursive descent isn't tail-recursive
/// here), but realistic JSON is never remotely close to that deep.
///
/// Found and fixed, while building this (not narrow gaps in JSON
/// itself, but real, previously-latent COMPILER bugs this was the
/// first thing to ever exercise): (1) `monomorphize::validate_field_
/// type` didn't recognize `Array`/`Task`/`Sender`/`Receiver`/`Ref` as
/// opaque pseudo-generic builtin types the way `ast_type_to_type`
/// already does, so an `Array[T]`-typed field reached through a
/// GENERIC struct instantiation (`ParseResult[JsonValue]`, whose own
/// `T` has an `Array[JsonValue]`-shaped field) failed with "unknown
/// generic struct \"Array\"". (2) TEN separate reuse-in-place codegen
/// sites (`CtorReuse`/`ArrayPushReuse`/`ArrayPopReuse`/`ArraySetReuse`/
/// `ArrayRemoveReuse`/`StrConcatReuse`/`StrTrimReuse`/`StrToUpperReuse`/
/// `StrToLowerReuse`/`StrReplaceReuse`) built their final merge `phi`
/// using the ORIGINAL branch-start label, not the block actually
/// reached after that branch's own codegen — invisible until a branch
/// itself opened further nested blocks (`inc_copied_array_elements`'s
/// loop, for an `Array`-fresh-copy needing to `Inc` heap-shaped
/// elements), which `.push()`ing onto `Array[JsonValue]` inside a
/// generic-struct-returning function was the first thing to trigger,
/// producing a real "PHI node entries do not match predecessors"
/// `clang` rejection. Both fixed at the root (see `plum-ir::
/// monomorphize::validate_field_type` and every `codegen.rs` reuse
/// site's own `reuse_end`/`alloc_end` capture), not worked around.
const STDLIB_JSON_SRC: &str = "\
struct JsonEntry { key: String, value: JsonValue }
enum JsonValue {
    JsonNull,
    JsonBool(Bool),
    JsonNumber(Float),
    JsonString(String),
    JsonArray(Array[JsonValue]),
    JsonObject(Array[JsonEntry]),
}
struct ParseResult[T] { value: T, next_pos: Int }

let chars_of (s: String): Array[String] = s.split(\"\").filter(|c| c != \"\")

let skip_ws (chars: Array[String]) (pos: Int): Int =
    if pos >= chars.len() { pos }
    else {
        let c = chars[pos];
        if c == \" \" || c == \"\\t\" || c == \"\\n\" || c == \"\\r\" { skip_ws(chars, pos + 1) } else { pos }
    }

let digit_value (c: String): Result[Float, String] =
    if c == \"0\" { Ok(0.0) }
    else if c == \"1\" { Ok(1.0) }
    else if c == \"2\" { Ok(2.0) }
    else if c == \"3\" { Ok(3.0) }
    else if c == \"4\" { Ok(4.0) }
    else if c == \"5\" { Ok(5.0) }
    else if c == \"6\" { Ok(6.0) }
    else if c == \"7\" { Ok(7.0) }
    else if c == \"8\" { Ok(8.0) }
    else if c == \"9\" { Ok(9.0) }
    else { Err(\"not a digit\") }

let pow10 (n: Int): Float = if n <= 0 { 1.0 } else { pow10(n - 1) * 10.0 }
let pow10_float (n: Float): Float = if n <= 0.0 { 1.0 } else { pow10_float(n - 1.0) * 10.0 }

let parse_digits_acc (chars: Array[String]) (pos: Int) (acc: Float) (any: Bool): Result[ParseResult[Float], String] =
    if pos >= chars.len() {
        if any { Ok(ParseResult { value: acc, next_pos: pos }) } else { Err(\"expected digit\") }
    } else {
        match digit_value(chars[pos]) {
            Ok(d) => parse_digits_acc(chars, pos + 1, acc * 10.0 + d, true),
            Err(_) => if any { Ok(ParseResult { value: acc, next_pos: pos }) } else { Err(\"expected digit\") },
        }
    }

let parse_digits (chars: Array[String]) (pos: Int): Result[ParseResult[Float], String] = parse_digits_acc(chars, pos, 0.0, false)

let parse_number (chars: Array[String]) (pos: Int): Result[ParseResult[Float], String] = {
    let neg = pos < chars.len() && chars[pos] == \"-\";
    let start = if neg { pos + 1 } else { pos };
    match parse_digits(chars, start) {
        Err(e) => Err(e),
        Ok(int_r) => {
            let has_frac = int_r.next_pos < chars.len() && chars[int_r.next_pos] == \".\";
            match (if has_frac { parse_digits(chars, int_r.next_pos + 1) } else { Ok(ParseResult { value: 0.0, next_pos: int_r.next_pos }) }) {
                Err(e) => Err(e),
                Ok(frac_r) => {
                    let frac_digit_count = if has_frac { frac_r.next_pos - (int_r.next_pos + 1) } else { 0 };
                    let frac_val = if has_frac { frac_r.value / pow10(frac_digit_count) } else { 0.0 };
                    let mag_no_exp = int_r.value + frac_val;
                    let exp_pos = frac_r.next_pos;
                    let has_exp = exp_pos < chars.len() && (chars[exp_pos] == \"e\" || chars[exp_pos] == \"E\");
                    if !has_exp {
                        let signed = if neg { 0.0 - mag_no_exp } else { mag_no_exp };
                        Ok(ParseResult { value: signed, next_pos: exp_pos })
                    } else {
                        let after_e = exp_pos + 1;
                        let exp_neg = after_e < chars.len() && chars[after_e] == \"-\";
                        let exp_pos_sign = after_e < chars.len() && chars[after_e] == \"+\";
                        let exp_digit_start = if exp_neg || exp_pos_sign { after_e + 1 } else { after_e };
                        match parse_digits(chars, exp_digit_start) {
                            Err(e) => Err(e),
                            Ok(exp_r) => {
                                let exp_count = exp_r.value;
                                let factor = pow10_float(exp_count);
                                let mag = if exp_neg { mag_no_exp / factor } else { mag_no_exp * factor };
                                let signed = if neg { 0.0 - mag } else { mag };
                                Ok(ParseResult { value: signed, next_pos: exp_r.next_pos })
                            }
                        }
                    }
                }
            }
        }
    }
}

let match_literal_at (chars: Array[String]) (pos: Int) (lit: Array[String]) (i: Int): Bool =
    if i >= lit.len() { true }
    else if pos + i >= chars.len() { false }
    else if chars[pos + i] == lit[i] { match_literal_at(chars, pos, lit, i + 1) }
    else { false }

let parse_literal (chars: Array[String]) (pos: Int) (word: String) (value: JsonValue): Result[ParseResult[JsonValue], String] = {
    let lit = chars_of(word);
    if match_literal_at(chars, pos, lit, 0) { Ok(ParseResult { value: value, next_pos: pos + lit.len() }) }
    else { Err(\"invalid literal\") }
}

let parse_string_body (chars: Array[String]) (pos: Int) (acc: String): Result[ParseResult[String], String] =
    if pos >= chars.len() { Err(\"unterminated string\") }
    else {
        let c = chars[pos];
        if c == \"\\\"\" { Ok(ParseResult { value: acc, next_pos: pos + 1 }) }
        else if c == \"\\\\\" {
            if pos + 1 >= chars.len() { Err(\"unterminated escape\") }
            else {
                let e = chars[pos + 1];
                if e == \"\\\"\" { parse_string_body(chars, pos + 2, acc.concat(\"\\\"\")) }
                else if e == \"\\\\\" { parse_string_body(chars, pos + 2, acc.concat(\"\\\\\")) }
                else if e == \"/\" { parse_string_body(chars, pos + 2, acc.concat(\"/\")) }
                else if e == \"n\" { parse_string_body(chars, pos + 2, acc.concat(\"\\n\")) }
                else if e == \"r\" { parse_string_body(chars, pos + 2, acc.concat(\"\\r\")) }
                else if e == \"t\" { parse_string_body(chars, pos + 2, acc.concat(\"\\t\")) }
                else { Err(\"unsupported escape sequence\") }
            }
        }
        else { parse_string_body(chars, pos + 1, acc.concat(c)) }
    }

let parse_string (chars: Array[String]) (pos: Int): Result[ParseResult[String], String] =
    if pos >= chars.len() || chars[pos] != \"\\\"\" { Err(\"expected opening quote\") }
    else { parse_string_body(chars, pos + 1, \"\") }

let parse_value (chars: Array[String]) (pos: Int): Result[ParseResult[JsonValue], String] =
    if pos >= chars.len() { Err(\"unexpected end of input\") }
    else {
        let c = chars[pos];
        if c == \"\\\"\" {
            match parse_string(chars, pos) {
                Ok(r) => Ok(ParseResult { value: JsonString(r.value), next_pos: r.next_pos }),
                Err(e) => Err(e),
            }
        }
        else if c == \"{\" { parse_object(chars, pos) }
        else if c == \"[\" { parse_array(chars, pos) }
        else if c == \"t\" { parse_literal(chars, pos, \"true\", JsonBool(true)) }
        else if c == \"f\" { parse_literal(chars, pos, \"false\", JsonBool(false)) }
        else if c == \"n\" { parse_literal(chars, pos, \"null\", JsonNull) }
        else {
            match digit_value(c) {
                Ok(_) => match parse_number(chars, pos) {
                    Ok(r) => Ok(ParseResult { value: JsonNumber(r.value), next_pos: r.next_pos }),
                    Err(e) => Err(e),
                },
                Err(_) => if c == \"-\" {
                    match parse_number(chars, pos) {
                        Ok(r) => Ok(ParseResult { value: JsonNumber(r.value), next_pos: r.next_pos }),
                        Err(e) => Err(e),
                    }
                } else { Err(\"unexpected character\") },
            }
        }
    }

let parse_array (chars: Array[String]) (pos: Int): Result[ParseResult[JsonValue], String] = {
    let p1 = skip_ws(chars, pos + 1);
    if p1 < chars.len() && chars[p1] == \"]\" { Ok(ParseResult { value: JsonArray([]), next_pos: p1 + 1 }) }
    else { parse_array_entries(chars, p1, []) }
}

let parse_array_entries (chars: Array[String]) (pos: Int) (acc: Array[JsonValue]): Result[ParseResult[JsonValue], String] =
    match parse_value(chars, pos) {
        Err(e) => Err(e),
        Ok(val_r) => {
            let new_acc = acc.push(val_r.value);
            let p1 = skip_ws(chars, val_r.next_pos);
            if p1 < chars.len() && chars[p1] == \",\" { parse_array_entries(chars, skip_ws(chars, p1 + 1), new_acc) }
            else if p1 < chars.len() && chars[p1] == \"]\" { Ok(ParseResult { value: JsonArray(new_acc), next_pos: p1 + 1 }) }
            else { Err(\"expected ',' or ']'\") }
        }
    }

let parse_object (chars: Array[String]) (pos: Int): Result[ParseResult[JsonValue], String] = {
    let p1 = skip_ws(chars, pos + 1);
    if p1 < chars.len() && chars[p1] == \"}\" { Ok(ParseResult { value: JsonObject([]), next_pos: p1 + 1 }) }
    else { parse_object_entries(chars, p1, []) }
}

let parse_object_entries (chars: Array[String]) (pos: Int) (acc: Array[JsonEntry]): Result[ParseResult[JsonValue], String] =
    match parse_string(chars, pos) {
        Err(e) => Err(e),
        Ok(key_r) => {
            let p1 = skip_ws(chars, key_r.next_pos);
            if p1 >= chars.len() || chars[p1] != \":\" { Err(\"expected ':'\") }
            else {
                let p2 = skip_ws(chars, p1 + 1);
                match parse_value(chars, p2) {
                    Err(e) => Err(e),
                    Ok(val_r) => {
                        let new_acc = acc.push(JsonEntry { key: key_r.value, value: val_r.value });
                        let p3 = skip_ws(chars, val_r.next_pos);
                        if p3 < chars.len() && chars[p3] == \",\" { parse_object_entries(chars, skip_ws(chars, p3 + 1), new_acc) }
                        else if p3 < chars.len() && chars[p3] == \"}\" { Ok(ParseResult { value: JsonObject(new_acc), next_pos: p3 + 1 }) }
                        else { Err(\"expected ',' or '}'\") }
                    }
                }
            }
        }
    }

let json_parse (s: String): Result[JsonValue, String] = {
    let chars = chars_of(s);
    let start = skip_ws(chars, 0);
    match parse_value(chars, start) {
        Err(e) => Err(e),
        Ok(r) => {
            let after = skip_ws(chars, r.next_pos);
            if after == chars.len() { Ok(r.value) } else { Err(\"trailing characters after JSON value\") }
        }
    }
}

let json_escape_chars (chars: Array[String]) (pos: Int) (acc: String): String =
    if pos >= chars.len() { acc }
    else {
        let c = chars[pos];
        let escaped =
            if c == \"\\\"\" { \"\\\\\\\"\" }
            else if c == \"\\\\\" { \"\\\\\\\\\" }
            else if c == \"\\n\" { \"\\\\n\" }
            else if c == \"\\r\" { \"\\\\r\" }
            else if c == \"\\t\" { \"\\\\t\" }
            else { c };
        json_escape_chars(chars, pos + 1, acc.concat(escaped))
    }

let json_escape (s: String): String = json_escape_chars(chars_of(s), 0, \"\")

let json_quote (s: String): String = \"\\\"\".concat(json_escape(s)).concat(\"\\\"\")

let join_with_commas (parts: Array[String]) (pos: Int) (acc: String): String =
    if pos >= parts.len() { acc }
    else if pos == 0 { join_with_commas(parts, pos + 1, parts[pos]) }
    else { join_with_commas(parts, pos + 1, acc.concat(\",\").concat(parts[pos])) }

let json_stringify (v: JsonValue): String = match v {
    JsonNull => \"null\",
    JsonBool(b) => if b { \"true\" } else { \"false\" },
    JsonNumber(n) => n.to_string(),
    JsonString(s) => json_quote(s),
    JsonArray(arr) => \"[\".concat(join_with_commas(arr.map(|x: JsonValue| json_stringify(x)), 0, \"\")).concat(\"]\"),
    JsonObject(entries) => \"{\".concat(join_with_commas(entries.map(|e: JsonEntry| json_quote(e.key).concat(\":\").concat(json_stringify(e.value))), 0, \"\")).concat(\"}\"),
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
/// `Map.keys`/`Map.values`/`Set.to_array` below are the direct,
/// concrete payoff: exactly the shape both bugs used to block. (Named
/// via `let Type.func (...)`, real associated functions — `plumc::
/// assoc_fns` — not the old flat `map_keys`/`set_to_array` naming,
/// removed entirely; see DESIGN.md's \"Standard library\" chunk 14.)
const STDLIB_COLLECTIONS_SRC: &str = "\
enum Map[K, V] { MapNode(K, V, Map[K, V]), MapEnd }

let Map.new[K, V] (): Map[K, V] = MapEnd

let Map.insert[K: Eq, V] (m: Map[K, V]) (k: K) (v: V): Map[K, V] =
    MapNode(k, v, m)

let Map.get[K: Eq, V] (m: Map[K, V]) (k: K): Option[V] = match m {
    MapNode(k2, v, rest) => if k == k2 { Some(v) } else { Map.get(rest, k) },
    MapEnd => None,
}

let Map.contains[K: Eq, V] (m: Map[K, V]) (k: K): Bool = match m {
    MapNode(k2, _, rest) => if k == k2 { true } else { Map.contains(rest, k) },
    MapEnd => false,
}

let Map.remove[K: Eq, V] (m: Map[K, V]) (k: K): Map[K, V] = match m {
    MapNode(k2, v, rest) => if k == k2 { rest } else { MapNode(k2, v, Map.remove(rest, k)) },
    MapEnd => MapEnd,
}

let Map.len[K, V] (m: Map[K, V]): Int = match m {
    MapNode(_, _, rest) => 1 + Map.len(rest),
    MapEnd => 0,
}

enum Set[T] { SetNode(T, Set[T]), SetEnd }

let Set.new[T] (): Set[T] = SetEnd

let Set.contains[T: Eq] (s: Set[T]) (x: T): Bool = match s {
    SetNode(y, rest) => if x == y { true } else { Set.contains(rest, x) },
    SetEnd => false,
}

let Set.insert[T: Eq] (s: Set[T]) (x: T): Set[T] =
    if Set.contains(s, x) { s } else { SetNode(x, s) }

let Set.remove[T: Eq] (s: Set[T]) (x: T): Set[T] = match s {
    SetNode(y, rest) => if x == y { rest } else { SetNode(y, Set.remove(rest, x)) },
    SetEnd => SetEnd,
}

let Set.len[T] (s: Set[T]): Int = match s {
    SetNode(_, rest) => 1 + Set.len(rest),
    SetEnd => 0,
}

let Set.union[T: Eq] (a: Set[T]) (b: Set[T]): Set[T] = match a {
    SetNode(x, rest) => Set.union(rest, Set.insert(b, x)),
    SetEnd => b,
}

let set_intersection_acc[T: Eq] (a: Set[T]) (b: Set[T]) (acc: Set[T]): Set[T] = match a {
    SetNode(x, rest) => if Set.contains(b, x) { set_intersection_acc(rest, b, Set.insert(acc, x)) } else { set_intersection_acc(rest, b, acc) },
    SetEnd => acc,
}
let Set.intersection[T: Eq] (a: Set[T]) (b: Set[T]): Set[T] = set_intersection_acc(a, b, Set.new(()))

let set_difference_acc[T: Eq] (a: Set[T]) (b: Set[T]) (acc: Set[T]): Set[T] = match a {
    SetNode(x, rest) => if Set.contains(b, x) { set_difference_acc(rest, b, acc) } else { set_difference_acc(rest, b, Set.insert(acc, x)) },
    SetEnd => acc,
}
let Set.difference[T: Eq] (a: Set[T]) (b: Set[T]): Set[T] = set_difference_acc(a, b, Set.new(()))

let set_from_array_acc[T: Eq] (arr: Array[T]) (i: Int) (acc: Set[T]): Set[T] =
    if i >= arr.len() { acc } else { set_from_array_acc(arr, i + 1, Set.insert(acc, arr[i])) }
let Set.from_array[T: Eq] (arr: Array[T]): Set[T] = set_from_array_acc(arr, 0, Set.new(()))

let map_from_arrays_acc[K: Eq, V] (keys: Array[K]) (values: Array[V]) (i: Int) (acc: Map[K, V]): Map[K, V] =
    if i >= keys.len() { acc } else { map_from_arrays_acc(keys, values, i + 1, Map.insert(acc, keys[i], values[i])) }
let Map.from_arrays[K: Eq, V] (keys: Array[K]) (values: Array[V]): Map[K, V] =
    map_from_arrays_acc(keys, values, 0, Map.new(()))

let Map.keys[K, V] (m: Map[K, V]): Array[K] = match m {
    MapNode(k, _, rest) => Map.keys(rest).push(k),
    MapEnd => [],
}

let Map.values[K, V] (m: Map[K, V]): Array[V] = match m {
    MapNode(_, v, rest) => Map.values(rest).push(v),
    MapEnd => [],
}

let Set.to_array[T] (s: Set[T]): Array[T] = match s {
    SetNode(x, rest) => Set.to_array(rest).push(x),
    SetEnd => [],
}
";

/// The testing framework's own assertions — pure Plum prelude source
/// (no new IR/codegen work at all here; the one genuinely new piece,
/// the low-level `panic_raw` primitive these build on, is a core-
/// language builtin implemented directly in `plum-ir`/`plum-interp`/
/// `plum-codegen`, not part of this string — see `ir::Expr::PanicRaw`'s
/// own doc comment).
///
/// `assert_eq`/`assert_ne` are bounded `[T: Eq + Show]` — both bounds
/// were ALREADY real, enforced trait bounds before this (`plum_types::
/// infer::satisfies_bound`), and multi-bound syntax (`[T: A + B]`)
/// already parsed — so no new type-system work was needed to write
/// these, only to USE what already existed. `Show` is what makes
/// `.to_string()` callable on `a`/`b` for the failure message; `Eq` is
/// what makes `==`/`!=` callable at all.
const STDLIB_ASSERT_SRC: &str = "\
let assert (cond: Bool): Unit =
    if cond { () } else { panic_raw(\"assertion failed\") }

let assert_eq[T: Eq + Show] (a: T) (b: T): Unit =
    if a == b { () } else {
        panic_raw(\"assertion failed: left != right\\n  left:  \".concat(a.to_string()).concat(\"\\n  right: \").concat(b.to_string()))
    }

let assert_ne[T: Eq + Show] (a: T) (b: T): Unit =
    if a != b { () } else {
        panic_raw(\"assertion failed: left == right\\n  left:  \".concat(a.to_string()).concat(\"\\n  right: \").concat(b.to_string()))
    }
";

/// `Option[T]`/`Result[T, E]` combinators — pure Plum source, no new
/// primitives. Declared via `let Type.func (...)` — real, per-type
/// associated functions (`plumc::assoc_fns`), called as `Option.
/// map(o, f)`/`Result.and_then(r, f)`, not the old flat `option_map`/
/// `result_and_then` naming (removed entirely, no aliases kept — see
/// DESIGN.md's \"Standard library\" chunk 14). Two `let Type.map` — one
/// under `Option`, one under `Result` — coexist with zero collision,
/// since `LetDef.name` ends up as the two different strings `\"Option.
/// map\"`/`\"Result.map\"`, exactly like `Point.add`/`Circle.add` would.
const STDLIB_OPTION_RESULT_SRC: &str = "\
let Option.map[T, U] (o: Option[T]) (f: (T) -> U): Option[U] = match o {
    Some(x) => Some(f(x)),
    None => None,
}

let Option.and_then[T, U] (o: Option[T]) (f: (T) -> Option[U]): Option[U] = match o {
    Some(x) => f(x),
    None => None,
}

let Option.unwrap_or[T] (o: Option[T]) (default: T): T = match o {
    Some(x) => x,
    None => default,
}

let Option.unwrap_or_else[T] (o: Option[T]) (f: () -> T): T = match o {
    Some(x) => x,
    None => f(),
}

let Option.is_some[T] (o: Option[T]): Bool = match o {
    Some(_) => true,
    None => false,
}

let Option.is_none[T] (o: Option[T]): Bool = match o {
    Some(_) => false,
    None => true,
}

let Option.ok_or[T, E] (o: Option[T]) (err: E): Result[T, E] = match o {
    Some(x) => Ok(x),
    None => Err(err),
}

let Result.map[T, E, U] (r: Result[T, E]) (f: (T) -> U): Result[U, E] = match r {
    Ok(x) => Ok(f(x)),
    Err(e) => Err(e),
}

let Result.map_err[T, E, F] (r: Result[T, E]) (f: (E) -> F): Result[T, F] = match r {
    Ok(x) => Ok(x),
    Err(e) => Err(f(e)),
}

let Result.and_then[T, E, U] (r: Result[T, E]) (f: (T) -> Result[U, E]): Result[U, E] = match r {
    Ok(x) => f(x),
    Err(e) => Err(e),
}

let Result.unwrap_or[T, E] (r: Result[T, E]) (default: T): T = match r {
    Ok(x) => x,
    Err(_) => default,
}

let Result.unwrap_or_else[T, E] (r: Result[T, E]) (f: (E) -> T): T = match r {
    Ok(x) => x,
    Err(e) => f(e),
}

let Result.is_ok[T, E] (r: Result[T, E]): Bool = match r {
    Ok(_) => true,
    Err(_) => false,
}

let Result.is_err[T, E] (r: Result[T, E]): Bool = match r {
    Ok(_) => false,
    Err(_) => true,
}
";

/// `Int`/`Float` numeric utilities — declared via `let Type.func (...)`,
/// real associated functions (`plumc::assoc_fns`), called as `Int.
/// min(a, b)`/`Float.sqrt(x)`, not the old flat `int_min`/`float_sqrt`
/// naming (removed entirely — see DESIGN.md's \"Standard library\"
/// chunk 14). `Int`/`Float` are never declared `ast::Item`s of their
/// own (they're hardcoded primitive-type string matches in
/// `plum_types::infer::ast_type_to_type`) — `assoc_fns::resolve_
/// associated_calls` seeds its own type registry with them directly to
/// still recognize `Int.func`/`Float.func` correctly.
///
/// One generic `let min`/`max`/`abs`/`clamp` still couldn't serve both
/// `Int` and `Float`: `<`/`>` (and therefore any `if`-based `min`/`max`)
/// only type-check against a CONCRETE numeric type (`infer_binary`'s
/// `default_numeric` call, not a generic `Ord`-style bound — no such
/// bound exists in this language yet) — but `Int.min`/`Float.min` now
/// coexist with zero collision regardless, since `Type.func` gives each
/// its own distinct name (`\"Int.min\"`/`\"Float.min\"`) the same way
/// `Option.map`/`Result.map` already do.
///
/// `floor`/`ceil`/`round`/`pow`/`sqrt` are ordinary `extern \"C\"`
/// declarations against libm (already unconditionally linked via
/// `clang -lm`, confirmed in `codegen_cli::clang_compile`, and already
/// resolvable through the interpreter's own dynamic extern-call path —
/// both proven by this crate's own pre-existing `sqrt`/`abs` extern-
/// call tests) — genuinely no pure-Plum way to compute these
/// (`floor`/`ceil`/`round` need bit-level float manipulation, `pow`
/// needs a real transcendental algorithm), unlike everything else here
/// which is ordinary `if`/comparison logic. Wrapped in safe functions
/// exactly like `print`/`println` wrap the raw `write` syscall.
const STDLIB_NUMBER_SRC: &str = "\
extern \"C\" {
    fn floor(x: Float) -> Float;
    fn ceil(x: Float) -> Float;
    fn round(x: Float) -> Float;
    fn pow(base: Float, exp: Float) -> Float;
    fn sqrt(x: Float) -> Float;
}

let Int.min (a: Int) (b: Int): Int = if a < b { a } else { b }
let Int.max (a: Int) (b: Int): Int = if a > b { a } else { b }
let Int.abs (a: Int): Int = if a < 0 { -a } else { a }
let Int.clamp (x: Int) (lo: Int) (hi: Int): Int = Int.min(Int.max(x, lo), hi)

let Float.min (a: Float) (b: Float): Float = if a < b { a } else { b }
let Float.max (a: Float) (b: Float): Float = if a > b { a } else { b }
let Float.abs (a: Float): Float = if a < 0.0 { -a } else { a }
let Float.clamp (x: Float) (lo: Float) (hi: Float): Float = Float.min(Float.max(x, lo), hi)
let Float.floor (x: Float): Float = unsafe { floor(x) }
let Float.ceil (x: Float): Float = unsafe { ceil(x) }
let Float.round (x: Float): Float = unsafe { round(x) }
let Float.pow (base: Float) (exp: Float): Float = unsafe { pow(base, exp) }
let Float.sqrt (x: Float): Float = unsafe { sqrt(x) }
";

/// `Array[T]` utilities — pure Plum, built entirely on the existing
/// builtin surface (`.len()`/`arr[i]` indexing/`.push()`/`.fold()`, all
/// already real), no new IR/codegen work. Declared via `let Array.func
/// (...)`, real associated functions (`plumc::assoc_fns`), called as
/// `Array.reverse(arr)` — not the old flat `array_reverse` naming
/// (removed entirely, see DESIGN.md's \"Standard library\" chunk 14).
/// Private recursive helpers (`array_reverse_acc` and its siblings)
/// deliberately keep their plain flat names — they're implementation
/// detail, never called from outside this file, so there's no public
/// API surface to namespace.
///
/// **Originally shipped NARROWER than this** — `Array.sort_by`,
/// `Array.zip`, `Array.sum_int`/`Array.sum_float` were all cut from the
/// first pass after surfacing two real, previously-latent `plum-types`
/// bugs, both now root-caused and fixed (see DESIGN.md's \"Standard
/// library\" chunk 12/13 for the full story):
/// - `Subst::compose` could produce a self-referential `id -> Var(id)`
///   binding when merging two substitutions that were each individually
///   acyclic but cross-referenced each other through exactly one key —
///   `Subst::apply` would then recurse on that binding forever (a real
///   `gdb` backtrace showed 100,000+ frames before the process
///   aborted). Hit by `array_drop_acc`'s original two-recursive-call-
///   site shape and by `array_sort_by`/`array_sort_insert`'s three-
///   helper-combination shape. Fixed in `subst.rs`: `compose` now
///   drops (rather than inserts) any merged binding that resolves back
///   to `Var(k)` for its own key `k` — see `Subst::compose`'s own doc
///   comment for the full correctness argument.
/// - `default_numeric` (the \"an unconstrained numeric type defaults to
///   `Int`\" rule, needed for e.g. `let x = 1 + 2`) fired too early
///   inside an unannotated closure passed DIRECTLY to `.fold()`/`.map()`
///   /`.filter()`: the closure's own params started as brand-new fresh
///   variables, completely disconnected from what the call site already
///   knew about them (`.fold()`'s `init`/the array's element type) —
///   only connected by a LATER `unify` call, AFTER the closure body was
///   already fully inferred. A body combining two still-fresh params
///   arithmetically with no literal to pin either side (fold's own
///   `|acc, x| acc + x` idiom) got BOTH defaulted to `Int` right then,
///   permanently — breaking a same-shaped `Float` accumulator later.
///   Fixed via `Infer::infer_expr_as_callback`: `.map`/`.filter`/
///   `.fold`'s own builtin-call arms now seed a closure-literal
///   argument's params from the ALREADY-KNOWN expected types before
///   inferring its body, instead of leaving them independently fresh.
const STDLIB_ARRAY_SRC: &str = "\
let Array.is_empty[T] (arr: Array[T]): Bool = arr.len() == 0

let Array.first[T] (arr: Array[T]): Option[T] = if Array.is_empty(arr) { None } else { Some(arr[0]) }

let Array.last[T] (arr: Array[T]): Option[T] = if Array.is_empty(arr) { None } else { Some(arr[arr.len() - 1]) }

let array_reverse_acc[T] (arr: Array[T]) (i: Int) (acc: Array[T]): Array[T] =
    if i < 0 { acc } else { array_reverse_acc(arr, i - 1, acc.push(arr[i])) }

let Array.reverse[T] (arr: Array[T]): Array[T] = array_reverse_acc(arr, arr.len() - 1, [])

let array_concat_acc[T] (a: Array[T]) (b: Array[T]) (i: Int): Array[T] =
    if i >= b.len() { a } else { array_concat_acc(a.push(b[i]), b, i + 1) }

let Array.concat[T] (a: Array[T]) (b: Array[T]): Array[T] = array_concat_acc(a, b, 0)

let array_take_acc[T] (arr: Array[T]) (n: Int) (i: Int) (acc: Array[T]): Array[T] =
    if i >= n { acc } else if i >= arr.len() { acc } else { array_take_acc(arr, n, i + 1, acc.push(arr[i])) }

let Array.take[T] (arr: Array[T]) (n: Int): Array[T] = array_take_acc(arr, n, 0, [])

let array_drop_acc[T] (arr: Array[T]) (n: Int) (i: Int) (acc: Array[T]): Array[T] =
    if i >= arr.len() { acc } else { array_drop_acc(arr, n, i + 1, if i < n { acc } else { acc.push(arr[i]) }) }

let Array.drop[T] (arr: Array[T]) (n: Int): Array[T] = array_drop_acc(arr, n, 0, [])

let Array.slice[T] (arr: Array[T]) (start: Int) (end: Int): Array[T] = Array.take(Array.drop(arr, start), end - start)

let array_find_acc[T] (arr: Array[T]) (f: (T) -> Bool) (i: Int): Option[T] =
    if i >= arr.len() { None } else if f(arr[i]) { Some(arr[i]) } else { array_find_acc(arr, f, i + 1) }

let Array.find[T] (arr: Array[T]) (f: (T) -> Bool): Option[T] = array_find_acc(arr, f, 0)

let Array.any[T] (arr: Array[T]) (f: (T) -> Bool): Bool = arr.fold(false, |acc, x| acc || f(x))

let Array.all[T] (arr: Array[T]) (f: (T) -> Bool): Bool = arr.fold(true, |acc, x| acc && f(x))

let array_index_of_acc[T: Eq] (arr: Array[T]) (x: T) (i: Int): Option[Int] =
    if i >= arr.len() { None } else if arr[i] == x { Some(i) } else { array_index_of_acc(arr, x, i + 1) }

let Array.index_of[T: Eq] (arr: Array[T]) (x: T): Option[Int] = array_index_of_acc(arr, x, 0)

let Array.contains[T: Eq] (arr: Array[T]) (x: T): Bool = Option.is_some(Array.index_of(arr, x))

let Array.sum_int (arr: Array[Int]): Int = arr.fold(0, |acc, x| acc + x)

let Array.sum_float (arr: Array[Float]): Float = arr.fold(0.0, |acc, x| acc + x)

let array_sort_insert_acc[T] (sorted: Array[T]) (x: T) (le: (T, T) -> Bool) (i: Int): Array[T] =
    if i >= sorted.len() { sorted.push(x) }
    else if le(x, sorted[i]) { Array.concat(Array.take(sorted, i).push(x), Array.drop(sorted, i)) }
    else { array_sort_insert_acc(sorted, x, le, i + 1) }

let array_sort_insert[T] (sorted: Array[T]) (x: T) (le: (T, T) -> Bool): Array[T] = array_sort_insert_acc(sorted, x, le, 0)

let array_sort_by_acc[T] (arr: Array[T]) (le: (T, T) -> Bool) (i: Int) (acc: Array[T]): Array[T] =
    if i >= arr.len() { acc } else { array_sort_by_acc(arr, le, i + 1, array_sort_insert(acc, arr[i], le)) }

let Array.sort_by[T] (arr: Array[T]) (le: (T, T) -> Bool): Array[T] = array_sort_by_acc(arr, le, 0, [])

struct Zipped[A, B] { first: A, second: B }

let array_zip_acc[T, U] (a: Array[T]) (b: Array[U]) (i: Int) (acc: Array[Zipped[T, U]]): Array[Zipped[T, U]] =
    if i >= a.len() || i >= b.len() { acc } else { array_zip_acc(a, b, i + 1, acc.push(Zipped { first: a[i], second: b[i] })) }

let Array.zip[T, U] (a: Array[T]) (b: Array[U]): Array[Zipped[T, U]] = array_zip_acc(a, b, 0, [])
";

/// `String` utilities — pure Plum, declared via `let String.func (...)`
/// (real associated functions, `plumc::assoc_fns`), no new IR/codegen
/// work. Built on the SAME codepoint-safe decomposition the JSON
/// parser already established and this file exports as a plain, non-
/// associated helper (`chars_of(s): Array[String] = s.split(\"\").
/// filter(|c| c != \"\")`, one-codepoint `String` elements) — reused
/// directly, not redefined, so `String.slice`/`String.index_of` never
/// risk splitting a multi-byte codepoint in half the way raw BYTE
/// indexing (`s[i]`, `.len()`) would. `Array.slice`/`Array.take`/
/// `Array.drop`/`Array.reverse` (chunk 12) do the actual positional
/// work on the resulting `Array[String]`; `chars_join` (new here, a
/// plain non-associated helper — internal plumbing, not public API,
/// same reasoning as `array_reverse_acc` and its siblings) folds a
/// character array back into one `String` via the existing `.concat()`
/// builtin.
///
/// `String.trim_start`/`String.trim_end` strip only ASCII whitespace
/// (space/tab/`\\n`/`\\r`) — narrower than the existing, real-Unicode-
/// aware `.trim()` builtin (which strips both ends at once, no one-
/// sided variant) — a real, honest v1 scope boundary, not a silent
/// gap, matching the same restraint chunk 9's JSON escape-set scope
/// already established for this codebase.
///
/// `String.parse_float` reuses `STDLIB_JSON_SRC`'s own already-tested
/// `parse_number(chars, pos): Result[ParseResult[Float], String]`
/// directly (handles sign/fraction/exponent already) rather than
/// writing a second float parser — just checks `next_pos` consumed the
/// WHOLE string, rejecting trailing garbage a partial JSON-internal
/// parse would otherwise silently accept.
///
/// **`String.repeat` is written the natural way, `s.concat(String.
/// repeat(s, n - 1))`.** An earlier version of this function used the
/// awkward `String.repeat(s, n - 1).concat(s)` ordering instead, to
/// dodge a real, previously-latent FBIP reuse-in-place bug found while
/// writing this chunk's own tests: a function PARAMETER used as
/// `.concat()`'s RECEIVER while ALSO passed again into a recursive call
/// that is itself `.concat()`'s own ARGUMENT silently corrupted the
/// result in both backends (confirmed: `rep(\"ab\", 3)` came back length
/// 8, not the correct 6). Root-caused and FIXED in `plum-ir/src/
/// fbip.rs`'s `mark_reuse`: it rewrote any bare-variable base into a
/// reuse candidate with no check that `insert_refcount_ops` had
/// actually protected that name with Inc/Dec — which it never does for
/// function parameters (no type checker in this IR to prove one is
/// heap-shaped). Fixed by gating every reuse rewrite on membership in
/// the same `known_heap` set `insert_refcount_ops` itself tracks. See
/// DESIGN.md's \"Standard library\" chunk 15 and its (now RESOLVED)
/// \"Open questions\" entry; `codegen_cli.rs` and this file both carry
/// dedicated regression tests pinning the ONCE-unsafe ordering directly.
const STDLIB_STRING_SRC: &str = "\
let chars_join (chars: Array[String]): String = chars.fold(\"\", |acc, c| acc.concat(c))

let string_reverse (s: String): String = chars_join(Array.reverse(chars_of(s)))

let String.is_empty (s: String): Bool = s.len() == 0

let String.slice (s: String) (start: Int) (end: Int): String = chars_join(Array.slice(chars_of(s), start, end))

let String.repeat (s: String) (n: Int): String = if n <= 0 { \"\" } else { s.concat(String.repeat(s, n - 1)) }

let string_is_ascii_ws (c: String): Bool = c == \" \" || c == \"\\t\" || c == \"\\n\" || c == \"\\r\"

let string_ws_prefix_len_acc (chars: Array[String]) (i: Int): Int =
    if i >= chars.len() { i } else if string_is_ascii_ws(chars[i]) { string_ws_prefix_len_acc(chars, i + 1) } else { i }

let String.trim_start (s: String): String = {
    let chars = chars_of(s);
    chars_join(Array.drop(chars, string_ws_prefix_len_acc(chars, 0)))
}

let String.trim_end (s: String): String = string_reverse(String.trim_start(string_reverse(s)))

let string_index_of_acc (chars: Array[String]) (needle: Array[String]) (i: Int): Option[Int] =
    if i + needle.len() > chars.len() { None }
    else if Array.slice(chars, i, i + needle.len()) == needle { Some(i) }
    else { string_index_of_acc(chars, needle, i + 1) }

let String.index_of (s: String) (needle: String): Option[Int] = string_index_of_acc(chars_of(s), chars_of(needle), 0)

let String.lines (s: String): Array[String] = s.split(\"\\n\")

let string_digit_value (c: String): Result[Int, String] = match c {
    \"0\" => Ok(0), \"1\" => Ok(1), \"2\" => Ok(2), \"3\" => Ok(3), \"4\" => Ok(4),
    \"5\" => Ok(5), \"6\" => Ok(6), \"7\" => Ok(7), \"8\" => Ok(8), \"9\" => Ok(9),
    _ => Err(\"expected a digit, found \".concat(c)),
}

let string_parse_int_digits_acc (chars: Array[String]) (i: Int) (acc: Int) (any: Bool): Result[Int, String] =
    if i >= chars.len() { if any { Ok(acc) } else { Err(\"expected at least one digit\") } }
    else { match string_digit_value(chars[i]) {
        Ok(d) => string_parse_int_digits_acc(chars, i + 1, acc * 10 + d, true),
        Err(e) => Err(e),
    } }

let String.parse_int (s: String): Result[Int, String] = {
    let chars = chars_of(s);
    if Array.is_empty(chars) { Err(\"empty string\") }
    else if chars[0] == \"-\" { Result.map(string_parse_int_digits_acc(Array.drop(chars, 1), 0, 0, false), |n| 0 - n) }
    else { string_parse_int_digits_acc(chars, 0, 0, false) }
}

let String.parse_float (s: String): Result[Float, String] = {
    let chars = chars_of(s);
    match parse_number(chars, 0) {
        Ok(r) => if r.next_pos == chars.len() { Ok(r.value) } else { Err(\"trailing characters after number\") },
        Err(e) => Err(e),
    }
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
    for src in [
        PRELUDE_SRC,
        STDLIB_IO_SRC,
        STDLIB_FILE_SRC,
        STDLIB_JSON_SRC,
        STDLIB_COLLECTIONS_SRC,
        STDLIB_ASSERT_SRC,
        STDLIB_OPTION_RESULT_SRC,
        STDLIB_NUMBER_SRC,
        STDLIB_ARRAY_SRC,
        STDLIB_STRING_SRC,
    ] {
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
const PRELUDE_TOTAL_LEN: usize = PRELUDE_SRC.len()
    + STDLIB_IO_SRC.len()
    + STDLIB_FILE_SRC.len()
    + STDLIB_JSON_SRC.len()
    + STDLIB_COLLECTIONS_SRC.len()
    + STDLIB_ASSERT_SRC.len()
    + STDLIB_OPTION_RESULT_SRC.len()
    + STDLIB_NUMBER_SRC.len()
    + STDLIB_ARRAY_SRC.len()
    + STDLIB_STRING_SRC.len();

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
    run_resolved_program_diag(program, fn_name, args).map_err(|e| e.to_string())
}

/// The `CompileError`-preserving sibling of `run_resolved_program` —
/// used by the CLI-facing `_diag` call chain (`modules::typecheck_and_
/// run_modules_diag` → `project::typecheck_and_run_project_diag`),
/// which needs the real `Span` to render a `file:line:col` + snippet.
/// `run_resolved_program` itself flattens this via `Display` at its own
/// boundary, so its own (many, pre-existing) callers/tests need no
/// changes at all. `Interpreter::load_program`/`call`'s own errors stay
/// plain `String` (interpreter runtime errors are genuinely spanless —
/// `ir::Expr` carries no `Span` at all, by design) — `?` auto-converts
/// them via `CompileError`'s blanket `From<String>`.
pub(crate) fn run_resolved_program_diag(
    program: ast::Program,
    fn_name: &str,
    args: Vec<Value>,
) -> Result<Value, plum_syntax::error::CompileError> {
    let mut program = program;
    assoc_fns::resolve_associated_calls(&mut program);
    let type_ctx = TypeContext::from_items(&program.items).map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;
    let mut infer = Infer::with_context(type_ctx);
    infer.infer_program(&program).map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;

    // A second, independent static gate — DESIGN.md's "channel send is
    // a move": reusing a value after `tx.send(v)` is a compile error.
    // Runs on the AST (see `movecheck`'s own doc comment for why), so
    // it doesn't need to wait for lowering; placed after type-checking
    // simply to keep the "cheapest/most-fundamental check first" order,
    // not because either gate depends on the other.
    plum_ir::movecheck::check_moves(&program).map_err(|e: plum_syntax::error::CompileError| e.context("move error"))?;

    // `p.x` needs to know WHICH struct `p` is to lower correctly —
    // lowering has no type information of its own, so this carries
    // inference's own answer across as a span-keyed side-channel. See
    // `Infer::field_owners`/`LoweringContext::field_owners`'s doc
    // comments for the full reasoning.
    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone());
    let ir_program = lower_program(&program, &lowering_ctx).map_err(|e: plum_syntax::error::CompileError| e.context("lowering error"))?;
    let ir_program = optimize_program(ir_program);

    let mut interp = Interpreter::new();
    interp.set_struct_field_names(lowering_ctx.struct_fields().clone());
    interp.load_program(&ir_program).map_err(|e| plum_syntax::error::CompileError::spanless(format!("load error: {e}")))?;
    interp.call(fn_name, args).map_err(Into::into)
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
                let m = Map.insert(Map.insert(Map.new(()), 1, 100), 2, 200);\n\
                let got = match Map.get(m, 2) { Some(v) => v, None => -1 };\n\
                let has1 = Map.contains(m, 1);\n\
                let m2 = Map.remove(m, 2);\n\
                let has2_after = Map.contains(m2, 2);\n\
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
                let s = Set.insert(Set.insert(Set.insert(Set.new(()), \"x\"), \"x\"), \"y\");\n\
                let n = Set.len(s);\n\
                let has_x = Set.contains(s, \"x\");\n\
                let s2 = Set.remove(s, \"x\");\n\
                let has_x_after = Set.contains(s2, \"x\");\n\
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
                let a = Set.from_array([1, 2, 2, 3]);\n\
                let b = Set.from_array([2, 3, 4]);\n\
                let m = Map.from_arrays([1, 2], [10, 20]);\n\
                let got = match Map.get(m, 2) { Some(v) => v, None => -1 };\n\
                Set.len(Set.union(a, b)) * 100 + Set.len(Set.intersection(a, b)) * 10 + Set.len(Set.difference(a, b)) + got\n\
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
                let m = Map.insert(Map.insert(Map.new(()), 1, 10), 2, 20);\n\
                let ks = Map.keys(m);\n\
                let vs = Map.values(m);\n\
                let s = Set.to_array(Set.from_array([1, 2, 3]));\n\
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
        // `cbrt` (cube root), not `sqrt` — `sqrt` is now a name the
        // prelude's own `STDLIB_NUMBER_SRC` declares as an extern
        // itself (backing `float_sqrt`), so a user program redeclaring
        // it is a real "already declared" error, same as any other
        // name collision with the prelude.
        let src = r#"
            extern "C" {
                fn cbrt(x: Float) -> Float;
            }
            let main x = unsafe { cbrt(x) }
        "#;
        let result = typecheck_and_run(src, "main", vec![Value::Float(8.0)]);
        assert_eq!(result, Ok(Value::Float(2.0)));
    }

    #[test]
    fn extern_call_outside_unsafe_is_rejected_before_it_ever_reaches_the_interpreter() {
        let src = r#"
            extern "C" {
                fn cbrt(x: Float) -> Float;
            }
            let main x = cbrt(x)
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

    // --- testing framework: `panic_raw` (see `ir::Expr::PanicRaw`) ---

    #[test]
    fn panic_raw_surfaces_as_a_runtime_error_through_the_interpreter() {
        let src = "let use_it dummy = panic_raw(\"boom\")";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Err("boom".to_string()));
    }

    #[test]
    fn panic_raw_inside_an_if_else_is_not_reached_when_the_condition_holds() {
        // Mirrors exactly how `assert`/`assert_eq` use `panic_raw` —
        // both branches of the `if` are `Unit`, so `panic_raw`'s own
        // fixed `Unit` result type unifies cleanly with the `then`
        // branch's `()`.
        let src = "let use_it dummy = if true { () } else { panic_raw(\"should not run\") }";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Unit));
    }

    // --- testing framework: `assert`/`assert_eq`/`assert_ne` (see `plumc::STDLIB_ASSERT_SRC`) ---

    #[test]
    fn assert_passes_silently_on_a_true_condition_through_the_interpreter() {
        let src = "let use_it dummy = assert(true)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Unit));
    }

    #[test]
    fn assert_fails_with_a_clear_message_on_a_false_condition_through_the_interpreter() {
        let src = "let use_it dummy = assert(false)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Err("assertion failed".to_string()));
    }

    #[test]
    fn assert_eq_passes_on_equal_primitives_through_the_interpreter() {
        let src = "let use_it dummy = assert_eq(1, 1)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Unit));
    }

    #[test]
    fn assert_eq_fails_with_left_and_right_values_through_the_interpreter() {
        let src = "let use_it dummy = assert_eq(1, 2)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit]).expect_err("expected assert_eq(1, 2) to fail");
        assert!(err.contains("left != right"), "unexpected error: {err}");
        assert!(err.contains('1') && err.contains('2'), "expected both values in the message, got: {err}");
    }

    #[test]
    fn assert_eq_on_structs_renders_their_to_string_in_the_failure_message() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = assert_eq(Point { x: 1, y: 2 }, Point { x: 1, y: 3 })";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit]).expect_err("expected the Points to differ");
        assert!(err.contains("Point"), "expected the struct's own .to_string() rendering in the message, got: {err}");
    }

    #[test]
    fn assert_ne_passes_on_different_values_through_the_interpreter() {
        let src = "let use_it dummy = assert_ne(1, 2)";
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Unit));
    }

    #[test]
    fn assert_ne_fails_with_a_clear_message_on_equal_values_through_the_interpreter() {
        let src = "let use_it dummy = assert_ne(1, 1)";
        let err = typecheck_and_run(src, "use_it", vec![Value::Unit]).expect_err("expected assert_ne(1, 1) to fail");
        assert!(err.contains("left == right"), "unexpected error: {err}");
    }

    // --- standard library: Option/Result combinators (see `plumc::STDLIB_OPTION_RESULT_SRC`) ---

    #[test]
    fn option_map_transforms_a_some_and_leaves_none_alone() {
        let src = "let use_it dummy = match Option.map(Some(1), |x| x + 1) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
        let src = "let use_it dummy = match Option.map(None, |x: Int| x + 1) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(-1)));
    }

    #[test]
    fn option_and_then_chains_a_some_and_short_circuits_a_none() {
        let src = "let half x = if x % 2 == 0 { Some(x / 2) } else { None }\n\
                    let use_it dummy = match Option.and_then(Some(4), half) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
        let src = "let half x = if x % 2 == 0 { Some(x / 2) } else { None }\n\
                    let use_it dummy = match Option.and_then(Some(3), half) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(-1)));
    }

    #[test]
    fn option_unwrap_or_falls_back_only_on_none() {
        let src = "let use_it dummy = Option.unwrap_or(Some(1), 9)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
        let src = "let use_it dummy = Option.unwrap_or(None, 9)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(9)));
    }

    #[test]
    fn option_unwrap_or_else_only_calls_its_closure_on_none() {
        let src = "let use_it dummy = Option.unwrap_or_else(Some(1), || panic_raw(\"should not run\").concat(0))";
        // deliberately invalid closure body to prove it's never called when the value is `Some`
        let result = typecheck_and_run(src, "use_it", vec![Value::Unit]);
        assert!(result.is_err(), "expected a type error from the deliberately-invalid closure body, not evaluation");

        let src = "let use_it dummy = Option.unwrap_or_else(None, || 9)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(9)));
    }

    #[test]
    fn option_is_some_and_is_none_agree_with_the_variant() {
        let src = "let use_it dummy = Option.is_some(Some(1))";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(true)));
        let src = "let use_it dummy = Option.is_some(None)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(false)));
        let src = "let use_it dummy = Option.is_none(None)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(true)));
    }

    #[test]
    fn option_ok_or_converts_some_and_none_to_result() {
        let src = "let use_it dummy = match Option.ok_or(Some(1), \"missing\") { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
        let src = "let use_it dummy = match Option.ok_or(None, \"missing\") { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(-1)));
    }

    #[test]
    fn result_map_transforms_an_ok_and_leaves_err_alone() {
        let src = "let use_it dummy = match Result.map(Ok(1), |x| x + 1) { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
        let src = "let use_it dummy = match Result.map(Err(\"boom\"), |x: Int| x + 1) { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(-1)));
    }

    #[test]
    fn result_map_err_transforms_an_err_and_leaves_ok_alone() {
        let src = "let use_it dummy = match Result.map_err(Err(\"boom\"), |e| e.concat(\"!\")) { Ok(x) => x, Err(e) => e.len() }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(5)));
        let src = "let use_it dummy = match Result.map_err(Ok(1), |e: String| e.concat(\"!\")) { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
    }

    #[test]
    fn result_and_then_chains_an_ok_and_short_circuits_an_err() {
        let src = "let half x = if x % 2 == 0 { Ok(x / 2) } else { Err(\"odd\") }\n\
                    let use_it dummy = match Result.and_then(Ok(4), half) { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
        let src = "let half x = if x % 2 == 0 { Ok(x / 2) } else { Err(\"odd\") }\n\
                    let use_it dummy = match Result.and_then(Err(\"boom\"), half) { Ok(x) => x, Err(e) => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(-1)));
    }

    #[test]
    fn result_unwrap_or_falls_back_only_on_err() {
        let src = "let use_it dummy = Result.unwrap_or(Ok(1), 9)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
        let src = "let use_it dummy = Result.unwrap_or(Err(\"boom\"), 9)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(9)));
    }

    #[test]
    fn result_unwrap_or_else_receives_the_err_payload() {
        let src = "let use_it dummy = Result.unwrap_or_else(Ok(1), |e: String| -1)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
        let src = "let use_it dummy = Result.unwrap_or_else(Err(\"boom\"), |e: String| e.concat(\"!\").len())";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(5)));
    }

    #[test]
    fn result_is_ok_and_is_err_agree_with_the_variant() {
        let src = "let use_it dummy = Result.is_ok(Ok(1))";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(true)));
        let src = "let use_it dummy = Result.is_ok(Err(\"boom\"))";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(false)));
        let src = "let use_it dummy = Result.is_err(Err(\"boom\"))";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(true)));
    }

    // --- standard library: string utilities (see `plumc::STDLIB_STRING_SRC`) ---
    //
    // `run_string_test` runs on a DEDICATED, generously-sized (16 MiB)
    // stack, not the default `#[test]` thread — the SAME "test-harness
    // artifact, not a real bug" reasoning `run_json_test` (below) is
    // already documented for: `String.index_of`/`.slice`/`.trim_*` all
    // go through `Array.slice`'s own `Array.take(Array.drop(...))`
    // chain, and the interpreter's `eval` has no tail-call optimization
    // (unlike native codegen's real `musttail` guarantee), so even a
    // modest, correctly-terminating amount of Plum-level recursion
    // (confirmed: `String.index_of("hello world", "world")` — 7 outer
    // steps, each running its own `Array.slice` sub-recursion — is
    // NOT a runaway loop, verified by re-running the exact same
    // program on a bigger stack and getting the correct, immediate
    // answer) fans out into far more actual Rust-level `eval` stack
    // frames, enough to overflow `cargo test`'s own narrow default.
    // `Value` isn't `Send` (it can transitively hold `Rc`), so it can't
    // cross the thread boundary directly — reduced to its `Debug`
    // representation instead, same "the actual value only ever needs
    // comparing, never touching from the parent thread" reasoning
    // `run_json_test`'s own `Bool`-only reduction uses, just general
    // enough to cover the Int/Float/Bool results these tests check.
    fn run_string_test(src: &str) -> Result<String, String> {
        let src = src.to_string();
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || typecheck_and_run(&src, "use_it", vec![Value::Unit]).map(|v| format!("{v:?}")))
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn string_is_empty_and_slice_are_codepoint_safe() {
        assert_eq!(run_string_test("let use_it dummy = String.is_empty(\"\")"), Ok("Bool(true)".to_string()));
        assert_eq!(run_string_test("let use_it dummy = String.is_empty(\"x\")"), Ok("Bool(false)".to_string()));
        // "café" — 'é' is a 2-byte UTF-8 codepoint; slicing by BYTE
        // index would either panic or split it in half. `String.slice`
        // goes through `chars_of`'s codepoint-safe decomposition
        // instead, so `String.slice("café", 0, 3)` must yield exactly
        // "caf" — 3 CODEPOINTS, not a mangled 3-BYTE prefix of the
        // 5-byte encoding (which would cut 'é' in half).
        assert_eq!(run_string_test("let use_it dummy = String.slice(\"café\", 0, 3) == \"caf\""), Ok("Bool(true)".to_string()));
    }

    #[test]
    fn string_slice_extracts_the_expected_substring() {
        assert_eq!(run_string_test("let use_it dummy = String.slice(\"hello world\", 0, 5) == \"hello\""), Ok("Bool(true)".to_string()));
        assert_eq!(run_string_test("let use_it dummy = String.slice(\"hello world\", 6, 11) == \"world\""), Ok("Bool(true)".to_string()));
    }

    #[test]
    fn string_repeat_concatenates_n_copies() {
        assert_eq!(run_string_test("let use_it dummy = String.repeat(\"ab\", 3) == \"ababab\""), Ok("Bool(true)".to_string()));
        assert_eq!(run_string_test("let use_it dummy = String.repeat(\"x\", 0) == \"\""), Ok("Bool(true)".to_string()));
    }

    #[test]
    fn the_previously_unsafe_recursive_concat_ordering_now_gives_the_correct_result_in_the_interpreter() {
        // Interpreter-side counterpart to `codegen_cli.rs`'s regression
        // test of the same shape — see this file's `STDLIB_STRING_SRC`
        // doc comment and DESIGN.md's "Open questions" entry (RESOLVED)
        // for the full FBIP reuse-in-place bug this pins.
        let src = "let rep (s: String) (n: Int): String = if n <= 0 { \"\" } else { s.concat(rep(s, n - 1)) }\n\
                    let use_it dummy = rep(\"ab\", 3).len() == 6";
        assert_eq!(run_string_test(src), Ok("Bool(true)".to_string()));
    }

    #[test]
    fn string_trim_start_and_trim_end_strip_only_their_own_side() {
        assert_eq!(run_string_test("let use_it dummy = String.trim_start(\"  hi  \") == \"hi  \""), Ok("Bool(true)".to_string()));
        assert_eq!(run_string_test("let use_it dummy = String.trim_end(\"  hi  \") == \"  hi\""), Ok("Bool(true)".to_string()));
    }

    #[test]
    fn string_index_of_finds_the_first_occurrence_or_none() {
        let src = "let use_it dummy = match String.index_of(\"hello world\", \"world\") { Some(i) => i, None => -1 }";
        assert_eq!(run_string_test(src), Ok("Int(6)".to_string()));
        let src = "let use_it dummy = match String.index_of(\"hello\", \"xyz\") { Some(i) => i, None => -1 }";
        assert_eq!(run_string_test(src), Ok("Int(-1)".to_string()));
    }

    #[test]
    fn string_lines_splits_on_newlines() {
        assert_eq!(run_string_test("let use_it dummy = String.lines(\"a\\nb\\nc\").len()"), Ok("Int(3)".to_string()));
    }

    #[test]
    fn string_parse_int_handles_positive_negative_and_invalid_input() {
        let src = "let use_it dummy = match String.parse_int(\"42\") { Ok(n) => n, Err(e) => -1 }";
        assert_eq!(run_string_test(src), Ok("Int(42)".to_string()));
        let src = "let use_it dummy = match String.parse_int(\"-42\") { Ok(n) => n, Err(e) => 999 }";
        assert_eq!(run_string_test(src), Ok("Int(-42)".to_string()));
        let src = "let use_it dummy = match String.parse_int(\"not a number\") { Ok(n) => n, Err(e) => -1 }";
        assert_eq!(run_string_test(src), Ok("Int(-1)".to_string()));
    }

    #[test]
    fn string_parse_float_handles_decimals_and_rejects_trailing_garbage() {
        let src = "let use_it dummy = match String.parse_float(\"3.5\") { Ok(f) => f, Err(e) => -1.0 }";
        assert_eq!(run_string_test(src), Ok("Float(3.5)".to_string()));
        let src = "let use_it dummy = match String.parse_float(\"3.5xyz\") { Ok(f) => f, Err(e) => -1.0 }";
        assert_eq!(run_string_test(src), Ok("Float(-1.0)".to_string()));
    }

    // --- associated functions: `Type.func(...)` (see `plumc::assoc_fns`) ---

    #[test]
    fn a_user_defined_struct_gets_a_real_associated_function_through_the_interpreter() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let Point.add (a: Point) (b: Point): Point = Point { x: a.x + b.x, y: a.y + b.y }\n\
                    let use_it dummy = { let p = Point.add(Point { x: 1, y: 2 }, Point { x: 10, y: 20 }); p.x * 100 + p.y }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1122)));
    }

    #[test]
    fn a_qualified_variant_construction_still_works_unaffected_by_associated_functions() {
        let src = "enum Shape { Circle(Float), Square(Float) }\n\
                    let use_it dummy = match Shape.Circle(2.0) { Circle(r) => r, Square(s) => s }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Float(2.0)));
    }

    #[test]
    fn a_local_shadowing_a_declared_type_name_is_ordinary_field_access_not_an_associated_call() {
        // `Point` here is a CLOSURE PARAMETER, not the struct type —
        // `Point.x` must resolve as ordinary field access, exactly as
        // if no `Point.x` associated function existed at all. (An
        // ordinary function parameter can't be spelled this way — a
        // capitalized name in `Param`/`Pattern` position is always a
        // struct/variant pattern, never a plain binding — but a closure
        // parameter is a bare `String`, no such restriction, so this is
        // the realistic way to construct the shadowing case at all.)
        let src = "struct Point { x: Int }\n\
                    let Point.x (a: Int): Int = a * 1000\n\
                    let use_it dummy = { \
                        let f = |Point: Point| Point.x; \
                        f(Point { x: 7 }) \
                    }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(7)));
    }

    #[test]
    fn a_bare_associated_function_reference_works_as_a_higher_order_argument() {
        let src = "struct Point { x: Int }\n\
                    let Point.double_x (p: Point): Int = p.x * 2\n\
                    let apply (p: Point) (f: (Point) -> Int): Int = f(p)\n\
                    let use_it dummy = apply(Point { x: 5 }, Point.double_x)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(10)));
    }

    // --- standard library: array utilities (see `plumc::STDLIB_ARRAY_SRC`) ---

    #[test]
    fn array_is_empty_and_reverse_behave_as_expected() {
        assert_eq!(typecheck_and_run("let use_it dummy = Array.is_empty([1, 2])", "use_it", vec![Value::Unit]), Ok(Value::Bool(false)));
        let src = "let use_it dummy = { let arr: Array[Int] = []; Array.is_empty(arr) }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(true)));
        let src = "let use_it dummy = match Array.reverse([1, 2, 3]) { arr => arr[0] * 100 + arr[1] * 10 + arr[2] }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(321)));
    }

    #[test]
    fn array_first_and_last_return_none_on_empty() {
        let src = "let use_it dummy = match Array.first([1, 2, 3]) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
        let src = "let use_it dummy = match Array.last([1, 2, 3]) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(3)));
        let src = "let use_it dummy = { let arr: Array[Int] = []; match Array.first(arr) { Some(x) => x, None => -1 } }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(-1)));
    }

    #[test]
    fn array_concat_take_and_drop_slice_correctly() {
        let src = "let use_it dummy = { let arr = Array.concat([1, 2], [3, 4]); arr[0] * 1000 + arr[1] * 100 + arr[2] * 10 + arr[3] }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1234)));
        let src = "let use_it dummy = { let arr = Array.take([1, 2, 3, 4], 2); arr[0] * 10 + arr[1] }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(12)));
        let src = "let use_it dummy = { let arr = Array.drop([1, 2, 3, 4], 2); arr[0] * 10 + arr[1] }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(34)));
        let src = "let use_it dummy = { let arr = Array.slice([1, 2, 3, 4, 5], 1, 4); arr[0] * 100 + arr[1] * 10 + arr[2] }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(234)));
    }

    #[test]
    fn array_find_any_all_locate_and_test_elements() {
        let src = "let use_it dummy = match Array.find([1, 2, 3, 4], |x| x % 2 == 0) { Some(x) => x, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
        let src = "let use_it dummy = Array.any([1, 3, 5], |x| x % 2 == 0)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(false)));
        let src = "let use_it dummy = Array.all([2, 4, 6], |x| x % 2 == 0)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(true)));
    }

    #[test]
    fn array_index_of_and_contains_use_the_eq_bound() {
        let src = "let use_it dummy = match Array.index_of([10, 20, 30], 20) { Some(i) => i, None => -1 }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(1)));
        let src = "let use_it dummy = Array.contains([10, 20, 30], 99)";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Bool(false)));
    }

    #[test]
    fn array_sum_int_and_array_sum_float_both_work_in_the_same_program() {
        // The regression test for the real `default_numeric`-fires-too-
        // early-inside-an-unannotated-closure bug (see `STDLIB_ARRAY_
        // SRC`'s own doc comment): `array_sum_int`'s `arr.fold(0, |acc,
        // x| acc + x)` and `array_sum_float`'s `arr.fold(0.0, |acc, x|
        // acc + x)` sit right next to each other in the SAME prelude —
        // before the fix, merely having BOTH declared (regardless of
        // which was actually called) made this fail with a bogus
        // "expected Int, found Float".
        let src = "let use_it dummy = Array.sum_int([1, 2, 3])";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(6)));
        let src = "let use_it dummy = Array.sum_float([1.5, 2.5, 3.0])";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Float(7.0)));
    }

    #[test]
    fn array_sort_by_sorts_using_the_given_comparator() {
        // The regression test for the real `Subst::compose` cyclic-
        // binding bug (see `STDLIB_ARRAY_SRC`'s own doc comment):
        // `array_sort_by`/`array_sort_insert`'s combination of THREE
        // other generic recursive helpers (`array_concat`/`array_take`/
        // `array_drop`) in one branch, alongside its own single self-
        // recursive call in the other, used to send `Subst::apply` into
        // genuine unbounded recursion before this was fixed.
        let src = "let use_it dummy = { \
                        let sorted = Array.sort_by([3, 1, 2], |a, b| a <= b); \
                        sorted[0] * 100 + sorted[1] * 10 + sorted[2] \
                    }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(123)));
        let src = "let use_it dummy = { \
                        let sorted = Array.sort_by([3, 1, 2], |a, b| a >= b); \
                        sorted[0] * 100 + sorted[1] * 10 + sorted[2] \
                    }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(321)));
    }

    #[test]
    fn array_zip_pairs_elements_positionally_and_stops_at_the_shorter_array() {
        let src = "let use_it dummy = { \
                        let zipped = Array.zip([1, 2, 3], [\"a\", \"b\"]); \
                        zipped.len() \
                    }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
        let src = "let use_it dummy = { \
                        let zipped = Array.zip([1, 2], [\"a\", \"b\"]); \
                        match zipped[1] { Zipped { first, second } => first } \
                    }";
        assert_eq!(typecheck_and_run(src, "use_it", vec![Value::Unit]), Ok(Value::Int(2)));
    }

    // --- standard library: number utilities (see `plumc::STDLIB_NUMBER_SRC`) ---

    #[test]
    fn int_min_max_abs_pick_the_expected_side() {
        assert_eq!(typecheck_and_run("let use_it dummy = Int.min(3, 7)", "use_it", vec![Value::Unit]), Ok(Value::Int(3)));
        assert_eq!(typecheck_and_run("let use_it dummy = Int.min(7, 3)", "use_it", vec![Value::Unit]), Ok(Value::Int(3)));
        assert_eq!(typecheck_and_run("let use_it dummy = Int.max(3, 7)", "use_it", vec![Value::Unit]), Ok(Value::Int(7)));
        assert_eq!(typecheck_and_run("let use_it dummy = Int.abs(-5)", "use_it", vec![Value::Unit]), Ok(Value::Int(5)));
        assert_eq!(typecheck_and_run("let use_it dummy = Int.abs(5)", "use_it", vec![Value::Unit]), Ok(Value::Int(5)));
    }

    #[test]
    fn int_clamp_bounds_a_value_into_range() {
        assert_eq!(typecheck_and_run("let use_it dummy = Int.clamp(-5, 0, 10)", "use_it", vec![Value::Unit]), Ok(Value::Int(0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Int.clamp(15, 0, 10)", "use_it", vec![Value::Unit]), Ok(Value::Int(10)));
        assert_eq!(typecheck_and_run("let use_it dummy = Int.clamp(5, 0, 10)", "use_it", vec![Value::Unit]), Ok(Value::Int(5)));
    }

    #[test]
    fn float_min_max_abs_clamp_pick_the_expected_side() {
        assert_eq!(typecheck_and_run("let use_it dummy = Float.min(3.0, 7.0)", "use_it", vec![Value::Unit]), Ok(Value::Float(3.0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Float.max(3.0, 7.0)", "use_it", vec![Value::Unit]), Ok(Value::Float(7.0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Float.abs(-2.5)", "use_it", vec![Value::Unit]), Ok(Value::Float(2.5)));
        assert_eq!(
            typecheck_and_run("let use_it dummy = Float.clamp(-1.0, 0.0, 10.0)", "use_it", vec![Value::Unit]),
            Ok(Value::Float(0.0))
        );
    }

    #[test]
    fn float_floor_ceil_round_and_pow_run_through_libm_through_the_interpreter() {
        assert_eq!(typecheck_and_run("let use_it dummy = Float.floor(3.7)", "use_it", vec![Value::Unit]), Ok(Value::Float(3.0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Float.ceil(3.2)", "use_it", vec![Value::Unit]), Ok(Value::Float(4.0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Float.round(3.5)", "use_it", vec![Value::Unit]), Ok(Value::Float(4.0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Float.pow(2.0, 10.0)", "use_it", vec![Value::Unit]), Ok(Value::Float(1024.0)));
        assert_eq!(typecheck_and_run("let use_it dummy = Float.sqrt(81.0)", "use_it", vec![Value::Unit]), Ok(Value::Float(9.0)));
    }

    // --- standard library: JSON (see `plumc::STDLIB_JSON_SRC`) ---
    //
    // `run_json_test` runs on a DEDICATED, generously-sized (16 MiB)
    // stack, not the default `#[test]` thread. `Interpreter::eval`
    // recurses as plain, un-tail-call-optimized Rust function calls
    // (unlike native codegen's real `musttail`-backed guarantee — see
    // DESIGN.md's "Guaranteed tail calls" section), and each ONE of
    // this recursive-descent JSON parser's own Plum-level recursive
    // calls fans out into many nested `eval` calls for its own
    // sub-expressions (`match`/`if`/field access/`.concat()`/...) — a
    // single, perfectly ordinary two-element array like `[1, 2]` was
    // enough to overflow `cargo test`'s own default 2 MiB worker-thread
    // stack. Confirmed via the REAL `plumc` CLI (main-thread stack,
    // ~8 MiB on Linux) that this is a TEST-HARNESS artifact, not a
    // real user-facing limitation — a real throwaway project parsing
    // both a small array and a realistic multi-field/nested JSON
    // document ran correctly with no special handling at all. This
    // helper exists purely so the test suite reflects that real
    // behavior instead of `cargo test`'s own narrower default.
    fn run_json_test(src: &str) -> Result<Value, String> {
        // `Value` isn't `Send` (it can transitively hold `Rc`), so it
        // can't cross the thread boundary directly — every one of
        // this helper's callers only ever checks for `Value::Bool
        // (true)`, so the closure reduces to that bool itself before
        // returning.
        let src = src.to_string();
        let result = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || typecheck_and_run(&src, "use_it", vec![Value::Unit]).map(|v| v == Value::Bool(true)))
            .unwrap()
            .join()
            .unwrap();
        result.map(|_| Value::Bool(true))
    }

    #[test]
    fn json_parse_handles_every_value_kind_through_the_interpreter() {
        let src = "let use_it dummy = match json_parse(\"{\\\"a\\\": 1, \\\"b\\\": [true, null, \\\"s\\\"]}\") { \
                    Ok(JsonObject(entries)) => entries.len() == 2, Err(_) => false }";
        assert_eq!(run_json_test(src), Ok(Value::Bool(true)));
    }

    #[test]
    fn json_parse_numbers_through_the_interpreter() {
        let src = "let use_it dummy = { \
            let a = match json_parse(\"42\") { Ok(JsonNumber(n)) => n == 42.0, Err(_) => false }; \
            let b = match json_parse(\"-3.5\") { Ok(JsonNumber(n)) => n == -3.5, Err(_) => false }; \
            let c = match json_parse(\"2e-2\") { Ok(JsonNumber(n)) => n == 0.02, Err(_) => false }; \
            a && b && c \
        }";
        assert_eq!(run_json_test(src), Ok(Value::Bool(true)));
    }

    #[test]
    fn json_parse_error_cases_through_the_interpreter() {
        let src = "let use_it dummy = { \
            let a = match json_parse(\"\") { Err(_) => true, Ok(_) => false }; \
            let b = match json_parse(\"[1, 2,]\") { Err(_) => true, Ok(_) => false }; \
            let c = match json_parse(\"{\\\"a\\\" 1}\") { Err(_) => true, Ok(_) => false }; \
            let d = match json_parse(\"42 extra\") { Err(_) => true, Ok(_) => false }; \
            let e = match json_parse(\"\\\"unterminated\") { Err(_) => true, Ok(_) => false }; \
            a && b && c && d && e \
        }";
        assert_eq!(run_json_test(src), Ok(Value::Bool(true)));
    }

    #[test]
    fn json_stringify_and_round_trip_through_the_interpreter() {
        let src = "let use_it dummy = { \
            let n = json_stringify(JsonNull) == \"null\"; \
            let arr = json_stringify(JsonArray([JsonNumber(1.0), JsonNumber(2.0)])) == \"[1,2]\"; \
            let a = json_parse(\"{\\\"x\\\": 1, \\\"y\\\": [true, null, \\\"s\\\"]}\"); \
            let roundtrip = match a { \
                Ok(v) => match json_parse(json_stringify(v)) { Ok(v2) => v == v2, Err(_) => false }, \
                Err(_) => false \
            }; \
            n && arr && roundtrip \
        }";
        assert_eq!(run_json_test(src), Ok(Value::Bool(true)));
    }

    // --- integration: chunk 8's file I/O composed with chunk 9's JSON ---
    //
    // Not new stdlib surface — a real end-to-end check that `write_
    // file`/`read_file` and `json_parse`/`json_stringify` compose
    // cleanly through the SAME `Result[T, String]` chaining a real
    // caller would write by hand (each one's own `Err` propagates
    // through the next step via ordinary `match`, no glue code needed
    // on either side). Runs on `run_json_test`'s big-stack thread since
    // it exercises the same recursive-descent parser.

    #[test]
    fn write_json_to_a_file_then_read_and_parse_it_back_through_the_interpreter() {
        let path = std::env::temp_dir().join(format!("plum-json-file-io-{}.json", std::process::id()));
        let path_str = path.to_str().unwrap();
        let src = format!(
            "let use_it dummy = {{ \
                let doc = JsonObject([JsonEntry {{ key: \"name\", value: JsonString(\"plum\") }}, \
                                       JsonEntry {{ key: \"tags\", value: JsonArray([JsonString(\"lang\"), JsonString(\"llvm\")]) }}]); \
                let w = write_file(\"{path_str}\", json_stringify(doc)); \
                match w {{ \
                    Ok(_) => match read_file(\"{path_str}\") {{ \
                        Ok(contents) => match json_parse(contents) {{ \
                            Ok(parsed) => parsed == doc, \
                            Err(_) => false \
                        }}, \
                        Err(_) => false \
                    }}, \
                    Err(_) => false \
                }} \
            }}"
        );
        assert_eq!(run_json_test(&src), Ok(Value::Bool(true)));
        let _ = std::fs::remove_file(&path);
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
