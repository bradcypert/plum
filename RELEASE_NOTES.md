Plum is a small, statically typed, compiled language.

**Upgrade if you are on 0.0.3.** It shipped two comparison bugs that
give wrong answers silently, described under Fixes.

The compiler is written in Plum. It compiles itself to a fixed point —
the compiler it produces emits byte-for-byte identical output to the
compiler that produced it — and building it needs no toolchain beyond
`clang`.

## Install

Download the archive for your platform, unpack it, and put `plum` on
your PATH. You need **`clang`** available; the compiler shells out to it
to assemble and link what it emits. Nothing else is required — the C
shims Plum programs use are embedded in the compiler itself.

| Platform | Archive |
|---|---|
| Linux x86_64 | `plum-0.0.4-x86_64-linux.tar.gz` |
| macOS Apple Silicon | `plum-0.0.4-arm64-macos.tar.gz` |
| macOS Intel | `plum-0.0.4-x86_64-macos.tar.gz` |
| Windows x86_64 | `plum-0.0.4-x86_64-windows.tar.gz` |

```sh
tar -xzf plum-0.0.4-arm64-macos.tar.gz
./plum-0.0.4-arm64-macos/plum version
```

Windows binaries are built for MSYS2/MinGW and the archive contains
`plum.exe`.

## Fixes

**Ordered comparison of Strings compared ADDRESSES, not contents.**
`<`, `<=`, `>` and `>=` on `String` fell through to a pointer
comparison. It never crashed; it returned whatever the allocator
happened to arrange. `"p" > "a"` was `false`, while the same
comparison on two computed strings was `true` — the split being that a
literal is a global constant and a computed string is a heap cell.

Sorting was unaffected: `Array.sort_string` uses an internal helper
rather than the operators, which is exactly why the bug survived. The
one place the language orders strings in anger never used the operators
that were broken.

**Ordered comparison of anything else was incoherent.** The checker
accepted `<` on any type at all, so the same pointer comparison applied
to arrays, structs, enums and `Bool`:

```
[1] < [2]   was true
[2] < [1]   was ALSO true
true < false was true
```

Both directions true at once is not a wrong ordering, it is no ordering
at all. These are now **compile errors** naming the type:

```
'<' needs an ordered type -- Int, Float or String -- but got Array[Int]
```

Equality is unaffected and still structural, all the way down: arrays,
structs, enums and their payloads.

`Int`, `Float` and `String` are ordered; nothing else is. Arrays and
structs could be given a lexicographic order and deliberately have not
been — enums have no obvious answer, nothing needs it, and rejecting
can be relaxed later while the reverse cannot.

**Hover could answer for the wrong node.** The prelude was parsed
without a source path of its own, so its items inherited the path of
whichever file was read last. Positions then collided: hovering a
`p: Point` could report `o: Option[T]`, an identifier from inside the
prelude.

## Editor support

**Completion after `.`** offers the members of whatever precedes the
dot: a struct's fields with their declared types, and every function
namespaced under that type. `p.` on a `Point` offers `x`, `y` and your
own `Point.shift`; `s.` on a `String` offers the nineteen `String.`
functions. The type comes from the checker, so it works on a local
whose type was never written down.

**Hover resolves fields and methods**, not just identifiers. `x` in
`p.x` reports `Int`; `trim_end` in `s.trim_end()` reports its whole
signature. Previously hovering a field answered for the value it
belonged to.

Both have the same limit: the base must be a plain identifier.
`p.x.to_string()` answers nothing for `to_string` rather than guessing,
and completion falls back to the whole-project name list.

## What is checked

Linux runs the full suite: 64 corpus fixtures — 44 that must run and
print exactly the right bytes, 7 that must abort with the right
message, 13 that the type checker must reject — all under
AddressSanitizer with `detect_leaks=1`, plus 11 property tests.

macOS and Windows run the 44 execution fixtures and a language-server
session.

The property tests are what found both comparison bugs' neighbours and
pin them now. `test_string_ordering` checks trichotomy, mirroring and
reflexivity, and checks the literal cases explicitly — a property
written only with computed values would have passed on half the bug.

## Known limits

- **Completion and hover need a plain identifier before the `.`.**
- **No completion of keywords or locals in scope.**
- **Linux arm64 is not published.** It is expected to work; nothing
  tests it.
- This is a 0.0.x release. There is no compatibility promise.
