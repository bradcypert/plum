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
`plum run` (the INTERPRETER) always prints `{value:?}` of `main`'s own
return after the program's own output; since `main` always returns
`Unit`, that's always a literal `Unit` line, not real program output.
The NATIVE build no longer needs this — see below.)

**Validating the self-hosted interpreter** (`bootstrap/self_host`,
native build — see `lexer/lexer.plum`'s own top comment for why):

```
plum build bootstrap/self_host -o sh
./sh run bootstrap/exec_corpus/<name>/main.plum
```

`./sh run` type-checks BEFORE interpreting (Stage 4 wired into the same
pipeline, matching the real `plum run`'s own order). **All 29 fixtures
pass end to end**, under both the self-hosted interpreter and the
self-hosted backend, ASan-clean in the latter. `tuples/` used to be a documented exception — Plum
had no tuple type-ANNOTATION syntax, so it could not satisfy Stage 4's
requirement that every top-level signature be annotated. Tuple types
were added in 2026-08 and the fixture is annotated now.

`refs/` used to be the next exception — the self-hosted compiler didn't
know `ref` at all (`unbound function: ref` from Stage 4) — until
`Ref[T]` was added to Stages 4/5 in 2026-08. Two fixtures were added
later still, each pinning down a bug rather than a feature:

- `pipe/` — `|>`, desugared in the self-hosted checker and interpreter
  rather than in its parser (the parse tree has to keep printing
  `(|> x f)` for `corpus/expressions/pipe_*.expected`).
- `collections/` — `Map`/`Set`, which pin `String.hash` down to the
  sign-bit mask: a hash map enumerates in hash order, so the two
  compilers agree on a printed map only if their hashes agree exactly.
- `for_array/` — `for x in xs` over an array, including heap elements.
- `currying/` — partial application, including the chained
  `f(a)(b)(c)` shape that a hand-written smoke test missed.
- `json/` — the JSON stdlib, whose round trip must match the real
  compiler byte for byte. It also caught the closure-parameter leak
  described in DESIGN.md, which `Array[Int]` fixtures structurally
  could not.
- `match_guards/` — arm guards, which the self-hosted backend ignored
  outright until `bootstrap/example-sweep` caught it printing a wrong
  answer.
- `contracts/` — `require`/`ensure`, plus `println` of a non-String
  (the latter was what actually blocked `examples/contracts`).
- `nested_patterns/` — a REFUTABLE pattern in a nested position
  (`ENode(OMul, a)`), which BOTH compilers miscompiled, differently.
  See its own header comment, and DESIGN.md's "Refutable nested
  patterns".

The BACKENDS are universal too now. Both the self-hosted backend and
`plum build` compile tuple VALUES (type-specialized tags — see
DESIGN.md), and the last two fixtures `plum build` rejected were fixed
on 2026-08-16:

- `arrays/` — `let empty = []` used to be a type error ("never used
  anywhere that would pin its element type"). An empty array whose
  element type survives inference unconstrained provably never has an
  element enter or leave it, so every choice is observationally
  identical; it defaults to `Unit` now. See
  `Infer::resolve_empty_array_elem_types`.
- `closures_in_structs/` — a struct with a closure-typed field was
  rejected in EVERY program, because the prelude's own `http_serve_loop`
  contains a `spawn` and every non-generic prelude function was emitted
  whether or not anything reached it. That held codegen's whole-program
  closure/task-field gate open universally. Dead-function elimination
  (`plum_ir::prune`) fixed it at the root: the gate now means what its
  doc comment always claimed.

So all 29 fixtures build and run identically under both backends —
including `refs/`, added on 2026-08-16 when `Ref[T]` (`ref(v)`/`.get()`/
`.set(v)`, the shared mutable cell) stopped being interpreter-only. See
DESIGN.md's "`Ref[T]` in native codegen".

The native side used to have a trailing-artifact quirk too — "a
pre-existing native-`main()` CLI behavior noticed several times across
this project, never chased down." It was chased down on 2026-08-15 while
starting Stage 5: `emit_main` echoed the entry function's return value,
and `Unit` shared `Bool`'s `%d\n` print path, so every compiled program
ended with a stray `0`. A `Unit`-returning entry now prints nothing, and
a compiled binary's output is exactly what the program printed. No
`head -n -1` on the native side.

## Self-interpretation

Since 2026-08-15 these fixtures are also run through TWO interpreter
levels — the self-hosted interpreter interpreting the self-hosted
interpreter interpreting the fixture:

```
./sh run bootstrap/self_host run bootstrap/exec_corpus/<name>/main.plum
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
concurrency/FFI still aren't; `Ref[T]` isn't either — see `refs/`
above). Fixtures here are chosen to exercise
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
literal. `refs/` exercises `Ref[T]` — `ref(v)`/`.get()`/`.set(v)`, two
names aliasing one cell, identity (not structural) equality, a `Ref`
holding a heap value whose old contents get released on `.set()`, and a
closure capturing a `Ref` as a running total.
