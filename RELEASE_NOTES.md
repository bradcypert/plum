Plum is a small, statically typed, compiled language.

`plum highlight` was quadratic. It is not any more.

## 32x on a large file

| input | 0.0.30 | 0.0.31 |
|---|---|---|
| 51 KB | 130 ms | 61 ms |
| 107 KB | 437 ms | 95 ms |
| 225 KB | 2,175 ms | 148 ms |
| 415 KB | 7,693 ms | 239 ms |

Doubling the input used to roughly quadruple the time. The output is
byte-identical over all 296 files it is checked against, so this is
purely cost.

It matters beyond the command itself: every code block on
[plumlang.org](https://plumlang.org) goes through `plum highlight`, and
so does the API reference. A large file was minutes of a site build
rather than seconds.

## What it was

```plum fragment
acc = hl_gap_pieces(lexer.trivia_before_at(chars, lexed, i), acc);
```

Plum's backend can grow an array in place when it can prove the old
value is dead, and a **self-rebinding assignment** is the shape where
that is provable: the slot is overwritten with the result, so nothing
can observe the mutation. `highlight.plum` carries a twenty-line comment
about this, ending "the fourth accidental O(n^2) in this project and the
second caused by this exact distinction".

The line above looks like that shape and is not one. The `push` happens
inside the callee, on a **parameter**, which is exactly where the
compiler cannot prove the old value is dead. So it copied the whole
accumulator once per token while the comment above it explained why it
did not.

The rule is narrower than "rebind the accumulator": the push itself has
to be the self-rebinding assignment, in the scope that owns the slot.
Moving it into a helper defeats it however the call site is written.

## The measurement that changed the fix

Three shapes, at 415 KB:

| shape | time |
|---|---|
| `acc = hl_gap_pieces(gap, acc)` | 7.7s |
| `acc = acc.concat(hl_gap_pieces(gap, []))` | 11.0s |
| `for .. { acc = acc.push(ps[i]) }` | 0.24s |

The middle one was the first fix, and it was worse. `acc =
acc.concat(x)` is written down as one of the two self-rebinding shapes
the backend can reuse, and here it did not: it allocated a fresh array
of the combined length every token and paid for the small array
besides. The reuse that actually fires is `acc = acc.push(x)`.

Worth knowing before reaching for `concat` to fix a copy. It is in
DESIGN.md so the next person does not have to measure it again.

## How it was found, and what that says

Not by a harness. `bootstrap/highlight-check` timed out in CI on the
compiler's own 415 KB `codegen.plum` once a slower runner crossed a
25-second limit.

That harness asserts that highlighting does not **corrupt** code: strip
the tags, undo the escaping, and the source comes back byte for byte. A
quadratic corrupts nothing. Every file passed the whole time, because
passing only required finishing, and on a developer's machine it always
finished.

The harness was not weak. It answered exactly the question it claimed to
answer, and nothing in the suite was asking about cost. That is worth
saying plainly in release notes, because the previous release fixed
seven harnesses that could not fail, and this is the same lesson
approached from the other side: a test suite proves what it asserts, and
not one thing more.
