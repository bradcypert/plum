# `bootstrap/exec_corpus/`

The execution counterpart to `bootstrap/corpus/` — where `corpus/`
validates the PARSER (does a snippet produce the right AST?), this
validates the whole pipeline: does a real *program* produce the right
*output*? Token/AST dumps can't answer that, hence a separate corpus.

Each `<name>/` is a small, runnable project (`main.plum`, matching
`examples/<name>/main.plum`'s own convention — a real `let main ():
Unit = ...`), paired with `expected.txt`: the exact stdout the program
prints.

## Running it

```
bootstrap/corpus-check
```

That builds every fixture with the SELF-HOSTED compiler, runs it under
ASan with `detect_leaks=1`, and compares stdout to `expected.txt` — and
also runs `./sh check` over each one, which is a separate claim from
"it runs" and turned out not to be true (below). No Rust compiler is
involved: the answers are checked in, not derived by running another
implementation, which is why this kept working when the Rust code was
retired on 2026-08-25.

**These fixtures were run by nothing at all until 2026-08-20.** Every
sentence below about what passes was, until then, the record of
somebody having typed the commands by hand once. Two things had rotted
in the meantime, and neither was visible in any output:

- `check` rejected `collections/` with `unbound variant/function: Map`.
  The fixture compiled and ran correctly the whole time — the BACKEND
  sees the prelude, and only the checker was blind to it. `Map`/`Set`
  had simply been missed when the builtin signatures were written.
- `concurrency/` leaks, by design (see its header). Worth knowing;
  `corpus-check` annotates it rather than dropping ASan for everyone.

This paragraph will rot too. `bootstrap/corpus-check` will not.

## History

`./sh run` used to INTERPRET, and these fixtures used to validate Stage
3, the self-hosted interpreter, which was deleted on 2026-08-20 once
`run` and `test` both compiled instead. Expected output used to be
generated with `plum run <dir> | head -n -1`, the `head` dropping the
interpreter's trailing echo of `main`'s own return value. A compiled
binary prints exactly what the program printed, so neither the echo nor
the `head` exists any more.

`tuples/` used to be a documented exception — Plum
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

So all 31 fixtures build and run identically under both backends —
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

## Self-interpretation (gone)

These fixtures were once run through TWO interpreter levels — the
self-hosted interpreter interpreting the self-hosted interpreter
interpreting the fixture — and the same trick ran the whole compiler
under itself. It found four real interpreter bugs; see DESIGN.md. None
of it is possible now that the self-hosted interpreter is gone. The
fixed-point check (`bootstrap/bootstrap-check`) is what covers
"the compiler is correct enough to process itself" today.

## Scope

Deliberately narrower than `corpus/`'s 98-fixture grammar breadth. This
section used to describe the bounded subset the self-hosted INTERPRETER
supported, and listed concurrency, FFI and explicit generic
instantiation as things it could not do — while `concurrency/`, `ffi/`
and `generics/` sat in this same directory, passing. The interpreter is
gone and the bound with it: a fixture belongs here if it pins down
behavior worth defending, and `bootstrap/corpus-check` decides whether
it passes.

Two fixtures originally planned (`match` with a guard, `match` with an
or-pattern) were dropped in 2026-08 because the REAL Rust interpreter
could not run those shapes, so no golden could be generated at all.
`match_guards/` exists now.

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
