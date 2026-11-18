# Tooling

## Build-cache diagnostics

`bootstrap/build-cache-check --require-objects` requires certified Linux
object reuse rather than allowing adapter fallback. The pinned Clang 18 CI
job uses it and verifies linked Plum/C debug locations on cold and warm runs
before and after edits. Ordinary platform runs continue to test fallback.

`PLUM_CACHE_TRACE=1 plum run <project>` reports IR/object hits, misses and
conservative fallbacks on stderr. `PLUM_PROFILE=1` reports compiler phase
timings in microseconds and emission counts. Neither changes the program's
stdout or LLVM output. See [RUNNING.md](RUNNING.md) for cache locations,
bypass controls, supported native reuse, and manual cleaning.

`bootstrap/measure-build-cache --baseline <original-compiler> --out <report.json>`
measures uncached, empty-cache, warm and edited runs sequentially, along with
checking and LLVM emission. It compares both compilers' IR for identical
inputs and records compiler digests, platform, sample spread and profiles.
Use an idle host; this is a measurement tool, not a timing assertion in CI.
See [the issue 63 results](docs/issue-63-results.md) for the baseline and
validation evidence, and [the incremental compilation proposal](docs/incremental-compilation.md)
for the next architectural stage.

## Generated reference documentation

`plum doc <project> -o <folder> --html` writes a reference that works
from a local folder. Omit `--html` for Markdown. Module and namespace
pages keep ordinary filenames such as `prelude.html` and
`prelude.Array.html`.
Dots within module or namespace names, literal tildes, and other unsafe
UTF-8 bytes are percent-encoded in filenames. A module named `index` uses
`~index`, keeping the landing page separate. Thus module `foo.Bar` writes
`foo%2EBar.html`, while namespace `Bar` in module `foo` writes `foo.Bar.html`.
Links encode those filenames as URL components, so a literal `%` on disk
is `%25` in a link. Remaining collisions, including case-only differences,
are rejected before writing any output.

`--home 'Project=https://example.com/'` adds a link back to your site.
The destination must be an HTTP(S) URL or a relative reference such as
`../index.html`, `/`, or `#overview`. Other schemes, protocol-relative
URLs (`//host`), whitespace, ASCII controls and backslashes are rejected.
Percent-encode spaces in destinations as `%20`. Labels and URLs are escaped
for their HTML context.

Search displays at most 40 results as text nodes. Its temporary match lists
are also bounded by that display limit; the complete search index remains
loaded in memory and is scanned on each query.

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

`plum fmt` uses a roomier layout by default, including when formatting in an
editor. It puts statements on separate lines, expands records with more than
two fields and enums with more than three variants, separates match/select
arms, and leaves a blank line between definitions. Consecutive imports stay
together, and comments stay attached to their code.

The formatter uses a soft target of 100 columns. Long function signatures
place each curried parameter group on a continuation line; `=` starts the body
at the definition's indentation. Calls, arrays, tuples and type arguments that
do not fit expand inside their delimiters, one argument per line. Long method
and pipeline chains continue four spaces past their starting line.

```plum fragment
let total (): Int = {
    let a = 1;
    let b = 2;
    a + b
}
```

Short records and expression-only guards can stay compact. Comment-free short
argument/type groups collapse when they fit; wrappers stay attached to multiline
payloads. Other existing line breaks are retained. Indentation uses four spaces,
and runs of blank lines
collapse to one. Token spacing is normalized, including commas, colons and
binary operators; a unary minus stays attached to its operand. Contract colons
retain their leading space, as in `require b != 0 : "message"`.

The width is a target rather than a hard limit. Literal contents and comment
text are preserved, and an indivisible identifier, string or deeply nested
construct can exceed it. Formatting never splits a token or rewrites an
expression to make it fit.

Preserve intentional layout with standalone comment directives:

```plum fragment
// fmt: skip
let aligned (): Array[Int] = [1,   2,
                             3,   4]

// fmt: off
let compact (): Int = { let n = 1;    n }
// fmt: on
```

`skip` preserves the next definition, statement, or expression, using the widest
parsed construct that starts at the next token. It includes its leading trivia
and text through its final token, plus following comments/newlines; indentation
of the next code is formatted normally. Use `off/on` for fragments such as
individual field declarations or match patterns.

`off/on` regions can nest. Each `on` closes one `off`; an unmatched `off` extends
to EOF. A stray `on`, or a `skip` with no following parsed construct, has no
effect. Overlapping exclusions are combined. Directive lines and preserved text
keep their original bytes, including tabs, literal contents and CRLF endings.
The directives must occupy their own lines, with the exact comment body shown;
leading/trailing whitespace is allowed. Text in strings, documentation comments,
trailing comments, or comments with additional prose does not activate them.
Place `skip` above any attached `///` documentation comments. The CLI and editor
use the same rules.

`bootstrap/fmt-check` checks the repository's formatting, recorded input/output
fixtures, token preservation, and stability on the next format-on-save.
Deliberately unformatted inputs retain their original layout as test data.
Highlighting fixtures with caret assertions use `fmt: off/on` directives and
pass the same fixed-point check as other sources.

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
