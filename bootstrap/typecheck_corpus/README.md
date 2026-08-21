# `bootstrap/typecheck_corpus/`

The REJECTION counterpart to `bootstrap/exec_corpus/` — Stage 4 (the
self-hosted type checker) is validated two ways: all 32 `exec_corpus/`
fixtures must type-check successfully (`ok`), and these twelve fixtures
must all be REJECTED. Without this half,
"the checker prints `ok` for everything" and "the checker actually
discriminates well-typed from ill-typed programs" would be
indistinguishable.

Each `<name>/main.plum` is a small, deliberately ILL-TYPED program.
Every one was confirmed to be genuinely rejected by the REAL Plum
compiler first (`plum run <dir>`) before being added here — a fixture
that accidentally happened to be valid Plum would prove nothing.

```
bootstrap/corpus-check
```

That runs both halves — every `exec_corpus/` fixture must type-check
clean, every fixture here must exit 1 — with no Rust compiler involved.
Until 2026-08-20 nothing ran either half, and the acceptance half was
in fact failing: `check` rejected `exec_corpus/collections` outright,
because `Map`/`Set` had been missed when the builtin signatures were
written. A fixture that runs but that the checker rejects is a real
disagreement inside one compiler, and it is exactly what a rejection
corpus on its own cannot see.

To run one by hand:

```
./sh check bootstrap/typecheck_corpus/<name>   # must exit 1
```

The twelve fixtures, and what each pins down:

- `wrong_return_type/` — a function's body doesn't match its declared
  return type.
- `wrong_arg_type/` — a function call with an argument of the wrong
  type.
- `mismatched_if_branches/` — an `if`/`else` whose two branches don't
  agree on a type.
- `unbound_variable/` — a genuinely undeclared name.
- `wrong_field_type/` — a struct literal with a field value of the
  wrong type.
- `closure_equality/`, `closure_field_equality/` — `==` on a function,
  directly and through a struct field. Structural equality would have
  to compare code.
- `extern_without_unsafe/` — calling an `extern` function outside an
  `unsafe` block.
- `stale_binding_after_constraint/` — a binding whose type was pinned
  by a later constraint, used at the older type.
- `zero_arg_is_not_partial/` — `scale()` on a two-parameter function.
  Currying makes a missing argument look like a closure rather than an
  error, which is how three separate call-site bugs hid in this
  compiler; see DESIGN.md.
- `let_annotation_mismatch/` — a local `let` whose annotation
  contradicts its value. The self-hosted compiler ACCEPTED this in
  every shape until 2026-08-20, and compiled and ran it: the parser
  read the annotation only to find where it ended, then discarded it.
  Its accepting counterpart is `exec_corpus/let_annotations`, which is
  the half that catches an over-strict fix. See DESIGN.md's "The
  annotation the parser threw away".
- `non_exhaustive_match/` — below.

## The one exclusion, now closed

`exec_corpus/tuples/` used to be the exception: it took an UNANNOTATED
tuple-shaped parameter (`let swap (t) = match t { (a, b) => (b, a) }`)
and Plum had no tuple type-ANNOTATION syntax, so it could not satisfy
this checker's requirement that every top-level signature be annotated.
Tuple types were added in 2026-08. The corpus is 32 of 32 now.

## `non_exhaustive_match/`

An enum `match` with a variant that has no arm and no catch-all. Both
checkers reject it with the *same* message — that agreement is the
point of the fixture, not the rejection on its own. The self-hosted
checker deliberately implements the real compiler's rule rather than a
stricter or cleverer one: two checkers that disagree about which
programs are valid is a worse outcome than either rule alone.

The rule, in both: only ENUM scrutinees are checked; a trailing
wildcard or bare binding exempts the match; only TOP-LEVEL variant tags
count (`Some(Ok(x))` covers `Some`, not "`Some` whose payload is
`Ok`"); an or-pattern covers every tag it names; and a GUARDED arm still
counts as covering its tag. The last two make the check incomplete
rather than unsound — it never rejects a match that does cover
everything.
