# Formatter follow-ups

Continue the default roomier formatter from PR #554 in four separate changes.
Preserve token text, comments, literals, CLI/editor parity, and idempotence.

## 1. Allocation scaling — complete

Reproduce allocation growth with a source-built compiler using the current
seed-to-source path. Measure requested Plum-cell allocation traffic separately
from peak/live memory and wall time. Profile normalization, lexing/parsing,
structural planning/rendering, indentation, and width layout before choosing a
fix. Prefer changes local to the measured allocation sources.

Baseline source: merged PR #554, commit
`566e602be3afc1a46d60d4af385f81b4cf4265c5`. The dense-statement fixture allocates about 107 MB, 398 MB, and
1.53 GB cumulatively for 1,000, 2,000, and 4,000 statements. Input doubling
grew allocations by roughly 3.7–3.8 times before this change.

Acceptance:

- Add an end-to-end scaling check that fails on the baseline and runs in CI.
- Keep doubling-size allocation growth below 2.6 times, with a measured absolute
  budget for the largest fixture. Cover dense statements and ordinary source.
- Preserve exact formatting output, token streams, and second-pass stability.
- Compare before/after source-built compilers on the same inputs and report
  allocation traffic without presenting it as peak memory.
- Run formatter, editor, generated-source, property, documentation, and relevant
  bootstrap checks. Refresh the seed only if needed to distribute the fix.

The measured hotspots were retained aliases in two buffers: the width writer's
`Ref[String]` and the parser's recorded-span `Ref[Array[ExprSpan]]`. Each append
copied the previous buffer while its Ref still held it. Both paths now read the
owned value, clear the Ref, then append and store the result. A retained caller
snapshot still triggers ordinary copy-on-write.

Source-built before/after results are recorded in
[formatter-scaling-measurements.json](formatter-scaling-measurements.json).

| Fixture | Largest size | Before allocated bytes | After allocated bytes |
| --- | ---: | ---: | ---: |
| Dense statements | 4,000 | 1,525,487,629 | 67,561,928 |
| Already formatted statements | 4,000 | 1,518,118,839 | 60,194,506 |
| Small definitions | 1,000 | 830,570,476 | 58,293,278 |

Allocation growth on input doubling is approximately 2.06–2.07 times. These
figures are cumulative allocation traffic, not peak process memory. No formatting
rules change in this step.

Reproduce using a compiler built before these changes and one built from this
source, both through the checked-in seed:

```sh
python3 bootstrap/formatter-scaling-check.py AFTER --before BEFORE \
    --out docs/formatter-scaling-measurements.json
```

`bootstrap/fmt-check` now runs the scaling test with a 2.6-times growth limit and
96 MB volume budgets. Parser tests separately retain span snapshots while new
spans are appended and verify that older snapshots and duplicate suppression
remain unchanged.

Validation used the source-built compiler: all 824 formatter checks, the new
allocation budgets, 54 compiler-internal tests, 92 properties, 43 documentation
pages, generated shims, CLI/editor formatting parity, and the full LSP smoke
suite passed. The seed gate produced an identical compiler without a seed
refresh. A focused AddressSanitizer build of the actual formatter/parser modules
passed 12 fixtures and retained-snapshot assertions with leak detection enabled.

## 2. Nested-call grouping and canonical layout — complete

Collect representative Powerlaunch examples as golden fixtures. Keep small
wrapper calls around multiline payloads together when prefixes and suffixes
fit, reducing delimiter-only lines and unnecessary indentation. Define when
short expressions return to one line so incidental input line breaks do not
permanently determine their layout.

Acceptance: reviewable before/after examples for JSON wrappers, callbacks,
Result chains, records, and signatures; stable output and unchanged tokens;
comments and literal contents retain their text and ownership.

The width pass now measures token/gap prefixes once, independently of their
existing line breaks. Comment-free parenthesized and bracketed groups collapse
when they fit the 100-column target. Expanded brace bodies and multiline literal
contents remain hard boundaries. Existing line breaks outside those groups are
retained; this step does not collapse statement or definition boundaries.

A call can keep its short prefix and final delimited payload together, including
nested JSON wrappers, literal arrays, records, and closure bodies. Index/type
argument brackets and conditional branches use ordinary argument layout. A
short final call or list stays whole on a fresh argument line when the outer
call is too wide; attachment is reserved for payloads that need their own lines.
Both the opening prefix and the trailing delimiters/separators count toward the
line budget. Comments at either join prevent contraction. Long signatures and
chain links use the same flat measurements so moving an inner group does not
require another save. When a callback body is another multiline call, its body
keeps a continuation line after `|v|`, including nested Result chains. Earlier
callback arguments stay inline when they fit.

For example:

```plum
let boxed (): Result[P, String] = Ok(P {
    x: 1,
    y: 2,
    z: 3
})
```

The earlier output put `P {` and `})` inside another pair of wrapper-only lines.
The fixtures in `bootstrap/fmt_corpus/grouping/` capture JSON wrappers, records,
Result callbacks, short calls, nested types, and comment boundaries. Width tests
also cover boundary lengths, closing delimiter reservations, CRLF, Unicode,
token preservation, and second-pass stability; editor tests cover the same rules
on unsaved buffers.

Validation passed with a compiler built through the seed-to-source path: 825
formatter checks and allocation budgets, 54 compiler tests, 92 properties,
43 documentation pages, generated-shim consistency, 339 CLI checks, editor
formatting/lifecycle and full LSP smoke tests, formatter filesystem checks, and
the seed equivalence gate. A focused build of the formatter/parser passed 23
AddressSanitizer/LeakSanitizer fixtures. The new grouping regression rejects the
allocation-only formatter. The repository is reformatted with the new default;
original formatting inputs and column-sensitive highlighting fixtures remain
inputs to their respective tests.

The additional width metadata raises the 4,000-statement fixture's cumulative
allocation traffic from 67.6 MB after step 1 to 70.9 MB. Doubling-size growth
remains close to linear and within the existing budgets. No seed refresh is
needed.

## 3. Explicit formatting exclusions — planned

Design a small directive surface for skipping a construct or preserving a
region, such as `fmt: skip` and `fmt: off/on`. Define nesting, unmatched markers,
and boundaries before implementation. Read directives from comments, never
from matching text inside string literals.

Acceptance: CLI/editor agreement, byte-preserved excluded regions, clear
boundary behavior, and column-sensitive highlighting fixtures maintained with
source-local directives rather than hard-coded path exemptions. Deliberately
invalid lexer fixtures remain a separate concern.

## 4. Formatting-independent tooling — planned

Audit generators and tools that read Plum source. Use existing compiler query
outputs where possible; add stable structured metadata for module/version
information that is otherwise recovered with regular expressions. Keep bootstrap
generators usable without an already-built compiler.

Acceptance: generated Plum is checked as a formatter fixed point, metadata
consumers handle missing/invalid data without passing vacuously, and formatting
changes do not require edits to source-scraping regular expressions. The shim
generator regression from PR #554 is the starting example.
