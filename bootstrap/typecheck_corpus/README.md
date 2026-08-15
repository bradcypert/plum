# `bootstrap/typecheck_corpus/`

The REJECTION counterpart to `bootstrap/exec_corpus/` — Stage 4 (the
self-hosted type checker) is validated two ways: `exec_corpus/`'s own
fixtures (11 of 12 — see below) must all type-check successfully
(`ok`), and these 5 fixtures must all be REJECTED. Without this half,
"the checker prints `ok` for everything" and "the checker actually
discriminates well-typed from ill-typed programs" would be
indistinguishable.

Each `<name>/main.plum` is a small, deliberately ILL-TYPED program.
Every one was confirmed to be genuinely rejected by the REAL Plum
compiler first (`plum run <dir>`) before being added here — a fixture
that accidentally happened to be valid Plum would prove nothing.

```
plum build bootstrap/self_host -o sh
./sh check bootstrap/typecheck_corpus/<name>/main.plum   # must exit 1
```

- `wrong_return_type/` — a function's body doesn't match its declared
  return type.
- `wrong_arg_type/` — a function call with an argument of the wrong
  type.
- `mismatched_if_branches/` — an `if`/`else` whose two branches don't
  agree on a type.
- `unbound_variable/` — a genuinely undeclared name.
- `wrong_field_type/` — a struct literal with a field value of the
  wrong type.

## Why `exec_corpus/` isn't 12/12 here

`exec_corpus/tuples/main.plum` uses an UNANNOTATED tuple-shaped
parameter (`let swap (t) = match t { (a, b) => (b, a) }`) — Plum has no
tuple type-ANNOTATION syntax at all (only tuple values), so there is
no way to satisfy this checker's own stated scope requirement (every
top-level function must be fully annotated) for this specific fixture.
A real, expected, documented exclusion — not a bug — see `typecheck/
infer.plum`'s own top comment for the full reasoning behind requiring
annotations in the first place.
