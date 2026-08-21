# `bootstrap/abort_corpus/`

Programs that must DIE, with a specific message, at a specific point.

`exec_corpus/` cannot hold these: its fixtures run to completion and
are compared on their output. A program that aborts halfway has no
"output" in that sense — what needs checking is the partial output, the
message, and the exit code together.

Each `<name>/` is a project whose `expected.txt` holds everything
printed before the abort PLUS the message itself. `bootstrap/corpus-
check` builds each with the self-hosted compiler, runs it, and requires
exit 1 and an exact match.

The `println("before")` at the top of each is deliberate: it proves the
program reached the failing operation rather than dying earlier for
some unrelated reason, which an exit code alone cannot distinguish. The
`println("unreachable")` at the bottom is the other half — it proves
the program actually STOPPED. Before 2026-08-20 the division fixtures
would have printed it.

## Why these four

Three are the fix of 2026-08-20. LLVM's `sdiv`/`srem` are undefined for
a zero divisor and for `i64::MIN / -1`, and both backends emitted them
unguarded. Undefined did not mean "crashes": optimized, `10 / d` for a
runtime zero `d` printed 22988924727 under the real backend and 135105
under the self-hosted one, and both programs then CARRIED ON. Built
unoptimized, the same program died of SIGFPE with no message. The
interpreter had been right the whole time (`eval_arith`'s
`checked_div`), and README.md already promised a division by zero was
an ordinary runtime error.

`array_index_out_of_bounds/` is not a fix — both backends have checked
that for a long time. It is here because it had no fixture either, and
because it is the behaviour the division guards were built to match:
same abort path, same shape of message, same exit code.

The divisor and the index come from FUNCTION CALLS in every fixture, so
no constant-folding can see them. A guard that only worked when the
compiler could already see the zero would pass a fixture written the
obvious way, and do nothing for a real program.
