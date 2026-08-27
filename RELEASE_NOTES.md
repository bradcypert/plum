Plum is a small, statically typed, compiled language.

This release is about **shipping a program you wrote in Plum**. Every
item in it came from porting a real CLI — an ADR management tool — and
running into the wall where the port stopped looking like the original.

## Cross-compiling: `--target`

```sh
PLUM_CC="zig cc" plum build myapp --target aarch64-linux-musl
```

Plum does not implement cross-compilation so much as stop preventing
it. Two things were already true and neither was on purpose: the IR
carries no target triple and no datalayout, so `clang --target=`
retargets it directly; and `Os.platform()` is compiled in by the C
compiler's own `#ifdef`, so it reports the *target's* platform without
being asked to. Exactly one line was host-specific — the list of
libraries to link read the compiler's own platform instead of the
target's.

What Plum still does **not** ship is a sysroot. `clang` can emit code
for any target but cannot link one without that target's libc, so
`--target` needs a C driver that has one, and `PLUM_CC` is where you
name it. That is the same arrangement Rust's ecosystem settled on with
`cargo-zigbuild`, and it is why a native build still needs nothing but
`clang`: nobody pays for a feature they do not use.

Two details worth knowing. A Windows target gets a `.exe` suffix when
you do not pass `-o`, because a PE not named `.exe` will not run and
nothing downstream would say why. And a non-64-bit target is refused
outright rather than built — cell layout assumes 8-byte slots, so a
32-bit target would not fail to link, it would silently miscompile.
The architecture list is an allow-list for that reason: an unfamiliar
name gets a clear refusal instead of a wrong binary.

`plum run` and `plum test` are always native. They execute what they
build.

Verified from an x86_64 Linux box: `aarch64-linux-musl`,
`x86_64-windows-gnu` and `aarch64-macos` all built and identified by
object format, with the aarch64 binary **run under qemu**.

## Compile-time builtins: `@embed_file`

```plum
let template (): String = @embed_file("templates/adr.md")
```

The `@` is the point as much as the function is. A builtin runs while
the **compiler reads your source**, not while your program runs, and
nothing about a plain call conveys that — `embed_file("x")` looks like
every other call on the page.

Replaced by the file's contents while the source is parsed. Nothing
downstream sees a call — the type checker and the backend see the same
string literal they would have seen had you typed the text out.

The path resolves against **the source file's own directory**, not the
working directory, so a build does not depend on where you launched it.
A module in a subdirectory embeds relative to itself.

The argument must be a literal string, since the file is read at
compile time — and an interpolated string is a value, not a literal, so
`embed_file("templates/${name}.md")` is an error that says so. All four
failure modes point at the call and quote the line.

Embedded text is data: it is never re-lexed, so `${...}` inside an
embedded file stays exactly as written. Text only — the result is a
`String`, which covers templates, schemas, SQL and help text, not
images.

The sigil also keeps builtins out of the identifier namespace, so
nothing is reserved: `embed_file` remains an ordinary name you are free
to define, and both can appear in the same expression.

The set of builtin names is closed, and an unknown one lists what
exists — `unknown builtin @nope. Plum has one: @embed_file("path")`.

## `use Time;` — and the first standard-library module

```plum
use Time;

Time.rfc7231(Time.now())   // Thu, 27 Aug 2026 01:40:28 GMT
Time.iso8601(Time.now())   // 2026-08-27T01:40:28Z
Time.utc(epoch)            // DateTime { year, month, day, hour, ... }
```

`Time` is a **module**, not a prelude namespace, and the distinction is
now a rule rather than a preference. `Array`, `String`, `Option` and
the rest have to be in the prelude, because `T.f(x)` is the method-call
mechanism — `xs.map(f)` only works because `Array.map` is in scope. A
namespace that names no type and dispatches to nothing is a module
wearing a namespace's clothes, and is now spelled like one.

A side effect worth knowing: this makes `use` **load-bearing** for the
first time. Until now it was parsed and then ignored — module
membership came from the directory an item was found in, so `use
shapes;` documented an intention nothing checked. A stdlib module has
no directory, so the `use` is the only thing that can bring it in.

Without it the error names the fix:

```
unbound variant/function: Time -- `Time` is a standard library module;
add `use Time;` to this file
```

`Os` is in exactly the same position and has **not** moved. It is
reachable from every program written so far, so that is a deliberate
break to schedule rather than a tidy-up to slip into a point release.

The 0.0.7 notes argued that turning epoch seconds into a date "is a
library on top of this rather than a runtime concern". That was right,
and this is that library — ordinary Plum in the prelude, no new
primitive. `Time.now()` is still the only part that needed the runtime.
What changed the mind was seeing what the argument costs in practice:
40 lines of calendar arithmetic in an ADR tool that wanted one
timestamp in a README.

UTC only; a timezone database is a data-shipping problem and Plum ships
no data. Dates before 1970 work — the arithmetic uses floor division
rather than `/`, which truncates toward zero and would otherwise put
the second before the epoch in 1970.

Checked against Python's `datetime` on 48 timestamps spanning 1900 to
2100, including both century-leap-year cases. All 48 agreed exactly.

## Padding, and a distinction that was documented backwards

```plum
String.pad_left("7", 5, "0")     // "00007"
String.pad_right("ab", 5, ".")   // "ab..."
String.char_len("café")          // 4  ("café".len() is 5 — bytes)
```

`String.len` counts bytes; every other string function counts
codepoints. Both halves were true and the combination was a trap, so
`String.char_len` now gives the count that matches the rest of the
library, and padding uses it — text lined up by byte count puts an
accented name in the wrong place.

The README claimed "there is currently no substring/slice primitive",
which stopped being true when `String.slice` was added. Fixed.

Padding refuses rather than surprises: a string already wide enough
comes back unchanged rather than truncated, and a fill that is not one
character comes back unchanged rather than panicking.

## What this did to the program that asked for it

The ADR tool, counting non-blank non-comment lines with template files
excluded on both sides:

| | lines |
|---|---|
| Zig original | 109 |
| Plum, before this release | 135 |
| Plum, after | **67** |

The comparison only became fair in this release. Before `embed_file`,
the Zig side got to keep 64 lines of template text in separate files
while Plum had to carry them as string literals in the source.

The rewrite is output-identical to the version before it, checked by
diffing a full exercise of every command.
