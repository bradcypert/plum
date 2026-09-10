# Tooling

## Testing

Any top-level function whose name starts with `test_` is a test. No
attribute, no registration, no `pub` required:

```plum fragment
// myapp/main.plum
let main (): Unit = println("hello, plum")

let test_addition (): Unit = assert_eq(2 + 2, 4)
let test_something_is_true (): Unit = assert(1 < 2)
```

```sh
plum test myapp
```

```
running 2 tests
test test_addition ... ok
test test_something_is_true ... ok

test result: ok. 2 passed; 0 failed
```

`assert(cond)` fails with `"assertion failed"` if `cond` is `false`.
`assert_eq(a, b)`/`assert_ne(a, b)` require `a`/`b` to be the same
`Eq`-comparable, `Show`-able type, and print both values on failure
(via `.to_string()`, so a struct/enum failure is readable, not just
"not equal"):

```
test test_area_is_wrong ... FAILED

failures:

---- test_area_is_wrong ----
assertion failed: left != right
  left:  12.56636
  right: 12

test result: FAILED. 0 passed; 1 failed
```

A failing test is just an ordinary runtime error under the hood, and any
other error (an array-index-out-of-bounds, a division by zero, ...)
inside a test function fails it the same way, not only a failed
`assert`. Tests in a non-root module are reported under their
qualified name (`shapes.test_area`, ...), same as anywhere else a
qualified name is used.

`plum test` COMPILES the project once and runs each test in its own
process, since a runtime failure is a hard abort with no way to keep going
in the same process, so isolation is not optional.

`bootstrap/test-smoke` exercises it against a fixture that
deliberately uses the things a smoke test is tempted to skip: the
prelude's assertions, a `Ref`, a zero-argument call, a partial
application, and asserts that a failing test both fails and does not
stop the ones after it. `plum test` was silently broken for months
before that fixture existed.

## Formatting

```sh
plum fmt --check src/     # list what needs formatting, non-zero exit
plum fmt --write src/     # format in place
plum fmt one.plum         # to stdout
```

`plum fmt` has seven opinions:

* statements, items and match arms are indented four spaces per block,
  and a closing brace sits one level out from what it closes;
* a run of comment lines is indented with whatever it documents;
* a run of blank lines collapses to one;
* a comma has no space before it and one after, and so does a colon --
  though a colon's LEADING space is left alone, because `require b != 0
  : "message"` spells it that way;
* nothing sits just inside a bracket, so `f( a )` becomes `f(a)`;
* a binary operator gets one space on each side, while a unary minus
  does not: `a-b` becomes `a - b` and `-n` is left alone;
* a line that CONTINUES a construct begun on an earlier line is indented
  four past the line that construct started on.

That last one covers the shapes the formatter used to leave alone: a
`let` body written on the next line, a `|>` chain broken across lines, a
multi-line call's arguments:

```plum
let bounds (xs: Array[String]): String =
    xs
        |> Array.map(_, String.trim)
        |> Array.join(_, ", ")

let main (): Unit = println(bounds([" a ", " b "]))
```

```
a, b
```

Every link of the chain is inside the one expression that began at `xs`,
so every link is indented from that line.

Everything else is passed through untouched, and it does not reflow: no
line is ever joined or split.

The rules were measured against this repository rather than chosen, and
the test of that is `bootstrap/fmt-check`: every `.plum` file here is
already formatted, so `plum fmt` changes none of them.

It also declines to place lines it cannot name. A line inside a
multi-line string literal, a hand-aligned line sitting deeper than the
grid answer, an item's `require`/`ensure`/`=` header lines. Those are
places where this repository either says nothing or disagrees with
itself, and a formatter with no evidence should leave your code alone.

**It cannot corrupt a file.** Before writing, `--write` re-lexes its own
output and compares the token sequence to the original's; whitespace
and comments are exactly what lies between tokens, so files whose tokens
agree in order differ only in formatting. A rule that changed the tokens
would be refused rather than written. Writes go to a temporary file
beside the original and are renamed into place, so an interrupted run
cannot leave a half-written source file.

## Editor support

Formatting is served over LSP as well as on the command line, so an
editor can format on save. It formats the buffer being edited, not the
file on disk; see the Formatting section above for what the formatter
will and will not do.

Expand-selection is served too (`textDocument/selectionRange`): put the
cursor on a name and grow the selection one construct at a time --
`v`, then `v * 2`, then `a + v * 2`, then the closure, then the call,
then the whole declaration. The chain is the nesting of expressions
around the cursor, which is what the compiler's expression spans record.

`plum lsp` serves an LSP out of the `plum` binary itself (the same
shape `gopls` takes for Go), speaking LSP over stdio. It is in every
published binary, on Linux, macOS and Windows.

| | |
|---|---|
| diagnostics | live, as you edit, against the unsaved buffer |
| hover | the inferred type of any identifier, field or method |
| go-to-definition | locals, params, top-level names |
| completion | project names, the stdlib, enum variants; fields and methods after `.` |

It asks the TYPE CHECKER, so hovering a local or a parameter shows the
type it was actually inferred to have (`doubled: Point`), and
go-to-definition on a local jumps to its binding, not to whatever
top-level name it happens to share. Top-level names fall back to a
by-name index, which is what supplies a function's full signature on
hover.

Completion offers every top-level name in your project, the standard
library's 100-odd public functions, and every enum variant, each with
its signature as the detail. The whole list is returned and your editor
filters it, which is what LSP clients do anyway.

**After a `.`** it offers the members of whatever precedes the dot
instead: a struct's fields with their declared types, and every
function namespaced under that type, so `p.` on a `Point` offers `x`,
`y` and your own `Point.shift`, while `s.` on a `String` offers the
nineteen `String.` functions. The type comes from the checker, so this
works on a local whose type was never written down. The base must be a
plain identifier; `foo().bar.` returns nothing rather than guessing,
and the editor falls back to the whole-project list.

Known limits: hover and go-to-definition need the project to type-check
cleanly, and the server re-checks per request (26ms on a small project,
~0.9s on the compiler's own 14k lines). Hover resolves fields and
methods as well as identifiers. `x` in `p.x` reports `Int`, and
`trim_end` in `s.trim_end()` reports its whole signature, but only
when the base is a plain identifier, the same limit dot completion has.
Hovering `to_string` in `p.x.to_string()` answers nothing rather than
guessing. The language server is exercised by a real LSP
session in CI on Linux, macOS and Windows.

Two pieces, independent of each other:

- **`plum lsp`** is an LSP server served straight out of the `plum`
  binary itself, speaking LSP over stdio.

  **Diagnostics** are live, against the unsaved buffer rather than the
  file on disk, and are attributed to the file the error is IN, which
  is not always the file being edited. Fixing an error publishes an
  empty list for that file, so it clears rather than lingering. One
  error at a time: the checker stops at the first, because a later
  function's error can genuinely depend on an earlier one's resolved
  signature, and reporting a cascade would be worse than reporting one
  real thing.

  **Hover** gives the inferred type of an identifier, a field or a
  method. **Go-to-definition** covers locals, parameters and top-level
  names. A local jumps to its binding, not to whatever top-level name
  it shadows.

  **Completion** offers project names, the standard library, enum
  variants, keywords and the locals in scope; after a `.` it offers
  the members of whatever precedes the dot instead. Names and keywords
  survive a buffer that does not type-check, because they are read
  without checking it; locals do not, since finding them requires the
  check to succeed.

  Hover and go-to-definition need the project to type-check cleanly.
  Fix-and-recheck is fast in practice: 26ms on a small project.
- **[`tools/tree-sitter-plum`](tools/tree-sitter-plum)** is a
  [tree-sitter](https://tree-sitter.github.io/) grammar for syntax
  highlighting/indentation, transcribed from `GRAMMAR.md`. A genuinely
  separate implementation from the compiler's own parser (see that
  directory's own README for the scope note and its two documented,
  deliberate simplifications). It exists purely to drive editor
  highlighting, not a second source of truth for the language's actual
  syntax rules.

**Neovim** is the only editor this is packaged and verified for so
far; see [`editors/nvim`](editors/nvim) for a ready-to-use runtime
bundle (LSP config + tree-sitter highlighting) and setup instructions.
Other editors aren't packaged yet; both pieces above are general enough
(stdio LSP, a standard tree-sitter grammar) that another editor's own
LSP client / tree-sitter integration should be able to point at them
directly, but that hasn't been tried.

## Debug and release builds

```sh
plum build myapp            # debug: -O0 -g, keeps frames and debug info
plum build myapp --release  # optimised: -O2
plum test myapp --release   # the same choice for tests
```

**Debug is the default**, and the asymmetry is the reason: someone who
wanted the fast binary and got the debuggable one has a slow program and
a flag to learn, while someone who wanted to debug and got the optimised
one has inlined frames and nothing to tell them why.

A debug binary keeps its frames and carries DWARF **line tables for
your Plum source**, so a debugger stops on the line you wrote and a
profiler attributes samples to it:

```sh
addr2line -e ./myapp 0x5320      # myapp/main.plum:14
perf report ./myapp              # samples land on Plum lines
```

Line tables only: where code came from, not what its types are. `gdb`
can step, break on a line and profile; it cannot print a local, because
describing every Plum type in DWARF is a much larger project than this
and nobody has needed it yet. Functions the compiler generates rather
than compiles (equality, release, the runtime) carry none, which is why
they show as `??` rather than pointing somewhere wrong.

`--release` carries none of this. Plum function names are real symbols
in both modes.

### Stack traces

```sh
plum build myapp --trace
```

A program built with `--trace` prints a stack trace to **stderr** when
it dies: for a failed bounds check, a division by zero, an overflow, a
broken contract, or an explicit `panic_raw`:

```
array index out of bounds        <- stdout, the same in every mode
stack trace:                     <- stderr, only with --trace
  at deepest
  at middle
  at outer
  at main
```

Innermost first, with your own function names. Compiler-generated frames
are hidden, so a failed precondition starts at the function you wrote.
Deep recursion is capped at 256 frames with a count of the rest.

**Tail recursion still runs in constant stack space under `--trace`.**
The trace is a shadow call stack, a frame pushed on entry and popped on
the way out, and where that pop goes decides whether the optimiser can
still turn a tail-recursive call into a loop. Each path through a
function pops its own frame, and a call to the function *itself* in tail
position pops *before* calling, so nothing sits between that call and
the return. The investigation, including two designs that failed, is in
[DESIGN.md](DESIGN.md).

One consequence is worth knowing: a tail-recursive chain shows **one**
frame, not one per iteration.

```
division by zero
stack trace:
  at count      <- three million calls deep; one frame
  at main
```

That is an accurate description rather than a lost frame. A tail call
really does replace its caller's frame, so the shadow stack replaces
the entry too. A call to a *different* function in tail position keeps
both frames, as `middle` does above.

It remains a flag rather than something the debug build always does,
because a shadow stack still costs a push and a pop per call.
