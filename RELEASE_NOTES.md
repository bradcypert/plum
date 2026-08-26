Plum is a small, statically typed, compiled language.

**Upgrade if you are on 0.0.2.** It shipped three defects, one of them
silent and platform-specific — details under Fixes below.

The compiler is written in Plum. It compiles itself to a fixed point —
the compiler it produces emits byte-for-byte identical output to the
compiler that produced it — and building it needs no toolchain beyond
`clang`. As of this release there is no Rust in the repository at all.

## Install

Download the archive for your platform, unpack it, and put `plum` on
your PATH. You need **`clang`** available; the compiler shells out to it
to assemble and link what it emits. Nothing else is required — the C
shims Plum programs use are embedded in the compiler itself.

| Platform | Archive |
|---|---|
| Linux x86_64 | `plum-0.0.3-x86_64-linux.tar.gz` |
| macOS Apple Silicon | `plum-0.0.3-arm64-macos.tar.gz` |
| macOS Intel | `plum-0.0.3-x86_64-macos.tar.gz` |
| Windows x86_64 | `plum-0.0.3-x86_64-windows.tar.gz` |

```sh
tar -xzf plum-0.0.3-arm64-macos.tar.gz
./plum-0.0.3-arm64-macos/plum version
```

Windows binaries are built for MSYS2/MinGW and the archive contains
`plum.exe`.

## Breaking

**`String.join` is now `Array.join`, and `String.concat_all` is now
`Array.concat_all`.** Both take an `Array[String]`, so both were filed
under the wrong namespace. That mattered once a namespace started
meaning something — see below.

## Method calls are namespaced functions

`x.f(a)` now means `T.f(x, a)`, where `T` is `x`'s type.

```
"  padded  ".trim_end()      // String.trim_end
"abcdef".slice(1, 3)         // String.slice
xs.map(|n| n * 2)            // Array.map
xs.contains(2)               // Array.contains
Some(7).unwrap_or(0)         // Option.unwrap_or
["a", "b"].join("-")         // Array.join
```

Before this, each method was hand-wired into the type checker
individually, so `s.trim()` worked and `s.trim_end()` did not — eight
did and six did not, with no rule to predict which.

It applies to your own types too. Declaring `let Box.bump (b: Box) (n:
Int): Box` makes `myBox.bump(1)` work; there is nothing special about
the standard library here.

A namespace names the type of the first parameter. `Os.` has no
receiver and therefore has no methods. A struct field holding a closure
wins over a namespaced function of the same name.

## Fixes

- **`parse_int` rejected `Int`'s own minimum.**
  `String.parse_int("-9223372036854775808")` returned "integer out of
  range" in 0.0.2. The magnitude was accumulated as a positive number
  and negated at the end, and that magnitude is one larger than `Int`'s
  maximum, so it overflowed while still positive.
- **`parse_float` was not correctly rounded.** `9.21258e-07` parsed to a
  double one ulp away, so `parse_float(x.to_string()) != x`. The value
  was computed as `mantissa * 10^exp` in floating point; it now comes
  from `strtod`, which is the same thing `to_string` checks its own
  output against.
- **The Windows language server could not open a file.** `file:///C:/…`
  became `/C:/…`, which opens nothing, so every hover, diagnostic and
  go-to-definition failed against a real Windows editor. macOS and
  Linux were unaffected. If you used the 0.0.2 Windows binary with an
  editor, this is why.

## Editor support

**Completion**, new in this release: every top-level name in your
project, the standard library's public functions, and every enum
variant, each with its signature as the detail. It does not resolve `.`
— that needs the type of whatever precedes the dot — so there is no
field completion yet.

The language server is now exercised by a real LSP session in CI on
Linux, macOS and Windows. In 0.0.2 it was tested on Linux only.

## What is checked

Linux runs the full suite: 63 corpus fixtures — 44 that must run and
print exactly the right bytes, 7 that must abort with the right
message, 12 that the type checker must reject — all under
AddressSanitizer with `detect_leaks=1`, plus 10 property tests.

macOS and Windows run the 44 execution fixtures and a language-server
session. They do not run leak checking: Plum is reference counted, so a
leak is a miscompile rather than untidiness, and LeakSanitizer does not
exist on macOS at all.

**The Rust implementation was retired in this release** — 44,698 lines.
It had been kept as a test oracle, comparing an independent
implementation of the semantics against the compiler. Two of the three
bugs above were ones it had *identically*, and an oracle can only find
disagreements. Property tests replaced it: they are written in Plum, so
they track the language instead of lagging it.

## Known limits

- **No field completion after `.`**, and hover does not resolve fields
  or variants — hovering `.x` in `p.x` answers for `p`.
- **Linux arm64 is not published.** It is expected to work; nothing
  tests it.
- This is a 0.0.x release. There is no compatibility promise.
