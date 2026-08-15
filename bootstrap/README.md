# `bootstrap/`

Support material for self-hosting Plum (a Plum compiler written in
Plum) — see DESIGN.md's "Self-hosting bootstrap corpus" section and
the project roadmap for the full "why now" story.

## `corpus/`

A comparison test suite, built and validated **before** writing a
single line of the self-hosted implementation — the whole point is to
turn "we forgot to handle some syntax" from a discovery made *while*
building Stage 1 (expensive: means going back and reworking whatever
was already written) into a *known, enumerated, already-passing* test
today, against the real Rust parser.

Each topic subdirectory (`let_defs/`, `structs/`, `contracts/`, ...)
holds small, focused `<name>.plum` fixtures, one isolated grammar
construct each (not big narrative demos — that's what `examples/` is
for), paired with TWO golden files:

- `<name>.expected` — the fixture's canonical AST dump: a compact,
  span-free s-expression (`(let double ((n:Int)) ->Int (* n 2))`),
  generated via `plum dump-ast <file>`.
- `<name>.tokens` — the fixture's canonical TOKEN dump: a flat, space-
  separated, span-free token list (`Let Ident("double") LParen ...`),
  generated via `plum dump-tokens <file>`. Exists so the self-hosted
  Stage 1 LEXER gets its own real pass/fail signal, independent of
  whatever state the self-hosted parser is in — without it, a lexer
  bug could only ever be discovered indirectly, once the parser was
  also far enough along to expose it.

**Adding a fixture**: write `<topic>/<name>.plum`, then generate both
goldens:

```
plum dump-ast <file>    > <topic>/<name>.expected
plum dump-tokens <file> > <topic>/<name>.tokens
```

`crates/plum-syntax/tests/golden.rs` discovers every fixture
automatically (no separate registration) and asserts the real
lexer/parser's output still matches both goldens — run it via `cargo
test -p plum-syntax --test golden`.

**Why this format, not `Debug`-derived output**: the AST's own
`#[derive(Debug)]` includes exact byte-offset `Span`s, which would make
every golden brittle to irrelevant whitespace/comment changes in a
fixture. The s-expression renderer (`plum_syntax::render`) drops spans
entirely — two parses that are semantically identical always render
identically.

**Why this lives at the repo root, not inside `crates/plum-syntax/`**:
this corpus is a contract between implementations, not one crate's own
test fixtures. Today only the real Rust parser is checked against it;
once Stage 1's self-hosted lexer/parser exists, it gets checked against
these exact same `.expected` files too — same acceptance bar, two
independent implementations.

## `exec_corpus/`

The execution counterpart to `corpus/` — validates Stage 3 (the
self-hosted INTERPRETER: does a real program produce the right
*output*?), which token/AST dumps can't do at all. Also reused
(unmodified) for most of Stage 4's own validation. See its own
`exec_corpus/README.md` for the full format and scope.

## `typecheck_corpus/`

The REJECTION counterpart to `exec_corpus/` — validates Stage 4 (the
self-hosted TYPE CHECKER) actually rejects ill-typed programs, not just
that it accepts well-typed ones. See its own `typecheck_corpus/
README.md`.

## `self_host/`

The self-hosted implementation itself — ONE project (Go-style
directory modules, DESIGN.md's "Module system" section), not one
project per stage: `lexer/`, `parser/`, `interp/`, and `typecheck/` are
library modules (`use lexer;`/`use parser;`/`use interp;`/`use
typecheck;`), `main.plum` at the root is the one real entry point.

```
plum build bootstrap/self_host -o sh
./sh tokens bootstrap/corpus/<topic>/<name>.plum          # lexer only
./sh ast    bootstrap/corpus/<topic>/<name>.plum          # lexer + parser
./sh run    bootstrap/exec_corpus/<name>/main.plum        # lexer + parser + interpreter
./sh check  bootstrap/typecheck_corpus/<name>/main.plum   # lexer + parser + type checker
```

**Validate with the NATIVE build, not `plum run`** — `plum run`
genuinely stack-overflows on some fixtures, a real, documented
interpreter limit (`lexer/lexer.plum`'s own top comment has the full
story), not a bug in this code. `plum build` doesn't hit it.

- **`lexer/`** — DONE, 98/98 corpus fixtures. Two real, concrete bugs
  found by actually running it against the corpus (a golden-generator
  bug, and the interpreter-stack-cost limit above) — see DESIGN.md's
  "Stage 1: self-hosted lexer" section.
- **`parser/`** — DONE, 98/98 corpus fixtures, both `tokens` and `ast`
  modes. Found and FIXED a genuine, previously-undiscovered bug in
  `plum-types::infer` itself along the way (non-generic function-to-
  function forward references couldn't do field access on the callee's
  struct-typed return value — a real gap in the type checker, not just
  this bootstrap effort, now closed for every Plum program). See
  DESIGN.md's "Stage 2: self-hosted parser" section for the full story,
  including one narrower, related gap found but NOT fixed (worked
  around in Plum source instead, documented at its exact call site).
- **`interp/`** — DONE, 12/12 execution-corpus fixtures, PLUS closures-
  as-values (`VClosure`, lexically captured at the closure literal's
  own creation site) AND arrays (`VArray` wrapping a real HOST `Array
  [Value]`, `EArray`/`EIndex`, `.len()`/`.push()`/`.set()`, `Array.map`/
  `filter`/`fold` as namespace calls) added afterward — 3 more
  fixtures, 15/15 total. Dynamically typed, walks the parser's AST
  directly (no lowering/IR) — no type checker needed to run a program.
  Found a real gap in its own env model along the way (`for`-loop
  accumulation into an outer `let mut` didn't persist past the loop)
  and fixed it with a scoped env-threading path, not a full rewrite.
  See DESIGN.md's "Stage 3: self-hosted interpreter", "Closures-as-
  values," and "Arrays" sections for the full story, including two more
  native-codegen pattern-lowering gaps found and worked around.
- **`typecheck/`** — DONE, 11/12 `exec_corpus` fixtures accepted (1
  real, documented exclusion — Plum has no tuple type-ANNOTATION
  syntax) + 5/5 `typecheck_corpus` fixtures correctly rejected, PLUS
  closures (fresh-var-typed `EClosure`, `ITFunction` unification at
  call sites, including the record-of-closures pattern) AND arrays
  (`Array[T]` represented as `ITStruct("Array", [T])` — the exact same
  representation the real compiler uses, reusing existing `ITStruct`
  machinery with zero new `ITy` cases) added afterward. Real Hindley-
  Milner: `types.plum` (`ITy`/`Subst`/`unify`, faithfully ported
  including a real self-loop-avoidance bug fix the real compiler found
  the hard way once already), `context.plum` (struct/enum templates),
  `infer.plum` (fresh vars, `TyEnv`, `infer_expr`/pattern-typing). Two
  deliberate simplifications made "full HM in one session" tractable:
  top-level signatures must be fully annotated (sidesteps the real
  compiler's hardest, most bug-prone piece — the Phase 1/Phase 2
  signature-bootstrapping split — entirely) and no `Scheme`/
  `generalize` machinery (Plum's generics are always explicitly
  declared, never discovered by generalization, a real difference from
  textbook ML, not a cut corner). See DESIGN.md's "Stage 4: self-hosted
  type checker," "Closures-as-values," and "Arrays" sections for the
  full story, including a real bug found by the very first test
  (`Option.unwrap_or`'s eager-evaluation trap).

## What's next

Four self-hosted Plum stages now exist (lexer, parser, interpreter,
type checker), each validated against its own real corpus, and the
interpreter and type checker now support both closures and arrays as
first-class values/types — the two biggest blockers identified when
this push toward true self-hosting began. Still not self-hosting:
neither stage supports generics beyond what's needed for `Array`
itself, and a real codegen backend (genuinely a different kind of
problem than tree-walking interpretation) is the single biggest
remaining piece. Neither has been scoped yet, matching how every prior
stage was scoped only once the previous stage's own real pain points
were known.
