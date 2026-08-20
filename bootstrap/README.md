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

## `seed/`

The self-hosted compiler as LLVM IR, checked in so a fresh clone can
build a compiler with `clang` alone — no Rust toolchain:

```
./bootstrap/from-seed                          # clang only
./sh.seed build bootstrap/self_host -o sh.real
```

See `seed/README.md` for why it is IR rather than a binary, and for the
refresh rule (`check-seed` fails when the seed has fallen behind;
`gen-seed` refreshes it, deliberately, because each refresh is ~6MB of
generated text).

**What the Rust compiler is still for.** Since the seed landed it is no
longer required to build anything. It stays for two jobs it is uniquely
good at: it is the ORACLE the example sweep compares self-hosted output
against byte for byte (which is what caught the `Bool`-width FFI bug,
the dropped match guards and both nested-pattern miscompilations), and
it is a from-source path for anyone unwilling to trust a checked-in
artifact. It is no longer where new language work happens.

## Scripts

| script | what it proves |
|---|---|
| `bootstrap-check` | the compiler compiled by itself is the same compiler |
| `self-sufficiency` | it can build itself with no Rust compiler, from any directory |
| `check-seed` | the checked-in seed still bootstraps to today's compiler |
| `example-sweep` | every `examples/` project agrees with the Rust compiler |
| `corpus-check` | 42 corpus fixtures compile, run, print the right thing, and leak nothing |
| `test-smoke` | `plum test` really runs tests, and both compilers agree |
| `lsp-smoke` | the language server answers a real session |
| `check-shims` | the embedded C shims match `native_stdlib/` |

`corpus-check` and `example-sweep` divide differently than the names
suggest. `example-sweep` derives its reference output by RUNNING the
Rust compiler, so it is the differential test and it needs `crates/`.
`corpus-check` compares against answers checked into the tree, so it
needs no second compiler and will outlive the first one.

## `self_host/`

The self-hosted implementation itself — ONE project (Go-style
directory modules, DESIGN.md's "Module system" section), not one
project per stage: `lexer/`, `parser/`, `typecheck/`, `codegen/`,
`lsp/` and `shims/` are library modules, `main.plum` at the root is the
one real entry point.

```
./sh tokens <file>.plum            # lexer only
./sh ast    <file>.plum            # lexer + parser
./sh check  <file-or-project>      # lexer + parser + type checker
./sh emit-llvm <project>           # + codegen: LLVM IR text
./sh build  <project> -o <out>     # + clang: a native binary
./sh run    <project>              # build to a temp binary and execute
./sh test   <project>              # compile once, run each test in its own process
./sh lsp                           # language server over stdio
```

`run` and `test` COMPILE — there is no interpreter here. There was one,
Stage 3 of the bootstrap below, and it was removed on 2026-08-20 once
both commands compiled instead. Keeping a second implementation of the
semantics inside one compiler meant every feature had to be written
twice, and the second half kept not happening: `run` fell seven features
behind `build`, and `test` was broken for every test that called
`assert_eq` — for months, unnoticed, because nothing exercised it. See
DESIGN.md's "The test runner was running on the wrong engine".

Plum still HAS an interpreter: `crates/plum-interp`, reached through the
Rust implementation's `plum run`. That one is live and tested.

- **`lexer/`** — DONE, 98/98 corpus fixtures. Two real, concrete bugs
  found by actually running it against the corpus (a golden-generator
  bug, and a stack-cost limit in the since-removed interpreter) — see
  DESIGN.md's
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
- **`interp/`** — REMOVED 2026-08-20. It reached 15/15 execution-corpus
  fixtures and was a genuine stage of the bootstrap (see DESIGN.md's
  "Stage 3: self-hosted interpreter"), but once `run` and `test` both
  compiled it was reachable from nothing, exercised by no harness, and
  still described here as live — which is exactly the state in which the
  `sh test` bug survived for months. The history is in DESIGN.md and in
  git; the code is not worth carrying unexecuted.
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

**Self-hosting is done.** The compiler compiles itself to a
byte-identical fixed point, builds itself with no Rust compiler involved
from any directory (`self-sufficiency`), and a fresh clone bootstraps
from `seed/` with clang alone. Every project in `examples/` builds and
runs identically under both implementations.

The paragraph that stood here described four stages and said "still not
self-hosting" — accurate when written, wrong for a long time after, and
nobody noticed because nothing checks prose. That is the argument for
`example-sweep`: **run it, and believe it over anything written here**,
including this sentence.

Known remaining differences are all in editor support — the Rust
implementation still has completion, live-as-you-type diagnostics, and
resolution for field names and enum variants. See the README's "Editor
support" table.
