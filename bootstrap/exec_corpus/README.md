# `bootstrap/exec_corpus/`

The execution counterpart to `bootstrap/corpus/` — where `corpus/`
validates the PARSER (does a snippet produce the right AST?), this
validates Stage 3, the self-hosted INTERPRETER (does a real *program*
produce the right *output*?). Token/AST dumps can't validate an
interpreter at all, hence a separate corpus.

Each `<name>/` is a small, runnable project (`main.plum`, matching
`examples/<name>/main.plum`'s own convention — a real `let main ():
Unit = ...`), paired with `expected.txt`: the exact stdout the REAL
`plum run` produces for it, generated via:

```
plum run bootstrap/exec_corpus/<name> | head -n -1
```

(`head -n -1` drops the CLI's own trailing return-value echo —
`plum run` always prints `{value:?}` of `main`'s own return after the
program's own output; since `main` always returns `Unit`, that's
always a literal `Unit` line, not real program output.)

**Validating the self-hosted interpreter** (`bootstrap/self_host`,
native build — see `lexer/lexer.plum`'s own top comment for why):

```
plum build bootstrap/self_host -o sh
./sh run bootstrap/exec_corpus/<name>/main.plum | head -n -1
```

`./sh run` type-checks BEFORE interpreting (Stage 4 wired into the same
pipeline, matching the real `plum run`'s own order) — 11 of these 12
fixtures pass end to end; `tuples/` fails at the type-check step
specifically (Plum has no tuple type-ANNOTATION syntax, so it can't
satisfy Stage 4's own annotation requirement — see `typecheck_corpus/
README.md`), a real, documented exception, not a regression. `./sh
check`/`./sh run` both surface the identical error for it.

The self-hosted CLI's own `run` mode has the SAME trailing-artifact
quirk for a different, unrelated reason (a pre-existing native-`main()`
CLI behavior noticed several times across this project, never chased
down — see DESIGN.md) — `head -n -1` strips it the same way on both
sides, so the comparison is apples to apples.

## Self-interpretation

Since 2026-08-15 these fixtures are also run through TWO interpreter
levels — the self-hosted interpreter interpreting the self-hosted
interpreter interpreting the fixture:

```
./sh run bootstrap/self_host run bootstrap/exec_corpus/<name>/main.plum | head -n -1
```

14/14 pass. The same mechanism runs the whole compiler under itself
(`./sh run bootstrap/self_host check bootstrap/self_host` -> `ok`); see
DESIGN.md's "The self-hosted interpreter now runs the whole self-hosted
compiler" section for the four real interpreter bugs that found.

## Scope

Deliberately narrower than `corpus/`'s 98-fixture grammar breadth —
Stage 3 is scoped to a real but bounded subset (see `interp/interp.plum`'s
own top comment for the exact list: closures-as-values and arrays ARE
supported now, but explicit generic instantiation (`EGenericInst`) and
concurrency/FFI still aren't). Fixtures here are chosen to exercise
exactly what that subset supports: arithmetic, comparisons/logic, `if`/
`else`, recursion, `struct`/`enum`/`match` (including tuple and nested-
struct patterns), `for` loops with `let mut` accumulation, string
building, closures (as first-class values, including the record-of-
closures pattern), and arrays (literals, indexing, `.len()`/`.push()`/
`.set()`, `Array.map`/`filter`/`fold`). Two fixtures originally planned
(`match` with a guard, `match` with an or-pattern) were dropped after
discovering the REAL Rust interpreter itself can't run those exact
shapes yet (pre-existing, unrelated compiler limits — confirmed via
direct testing, not assumed) — no golden could be generated for them at
all, so there was nothing to validate against.

`closures/` exercises a closure bound to a local and called by name, a
closure returned from another closure (`make_adder(5)`), a closure
passed as an ordinary higher-order argument (`apply_twice`), and a
closure capturing an outer local. `closures_in_structs/` exercises the
"interfaces via records of closures" pattern (DESIGN.md's own Reader/
Writer story) — a struct whose fields are closures, called via
`instance.field(args)`. `arrays/` exercises literal construction,
indexing, `.len()`/`.push()`/`.set()`, `Array.map`/`filter`/`fold` (the
namespace-call convention, not dot-call sugar), and an empty-array
literal.
