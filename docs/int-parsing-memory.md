# Integer parsing without character materialization

Follow-up to #482's allocation work. String.parse_int previously built a
String for every character plus an array before parsing, and copied the
character array again for negative input. The compiler's lexer and LSP use
this public function, as do compiled programs.

## Alternatives

All implementations were compared under the same merged #482 runtime in
debug/release. The character-based version, a byte scanner returning a
Result for each digit, and a scalar byte scanner passed an independent
before/after value-and-error oracle.

| Parse workload | Character array | Bytes + per-digit Result | Bytes + scalar digits |
| --- | ---: | ---: | ---: |
| 1,000 nine-digit parses: traffic | 274,000 B | 240,000 B | 24,000 B |
| Same: allocations | 11,000 | 10,000 | 1,000 |
| One 65,536-digit zero: traffic | 1,703,976 B | 1,572,888 B | 24 B |
| Same: allocations | 65,538 | 65,537 | 1 |

Choose scalar byte scanning in ordinary Plum. Removing the array alone is
insufficient when every byte creates a temporary Result. No new runtime
primitive, builder, native parser or ownership representation is required.
Restricting inputs to 19 digits would reject valid long zero-padded numbers.
Using libc parsing would require additional checks to preserve the current
strict sign/space/NUL behavior and error messages, with no allocation benefit
over this byte scan. Float/JSON number parsing is separate and unchanged.

## Compatibility and ownership

The public grammar and Result type stay unchanged: optional ASCII minus,
ASCII decimal digits, no plus/whitespace/radix prefixes, signed Int bounds,
and the existing first-error messages and precedence. Range is checked before
checked multiplication/addition. Negative accumulation still represents
Int::MIN without first constructing its positive magnitude.

The scan holds an immutable Bytes view of the input and unboxed integer/Bool
state. It creates only the final dynamic result on successful parses. Literal
errors can use the existing shared constant Results. Early overflow no longer
requires materializing the remainder of a long input first.

Raw Strings from unsafe interfaces require care: chars_of groups following
continuation bytes with the preceding byte, even if that byte is ASCII. The
scanner therefore accepts a sign/digit only as a single-byte unit, reusing
the existing continuation predicate. Every accepted prefix byte is consequently
also one character. On an invalid unit, String.slice can use that index to
preserve the same complete diagnostic unit, including NUL and malformed UTF-8.
There is no second Unicode decoder.

Perceus and ARC still own input views and final Results normally. Inputs and
String/Bytes/CStr aliases remain immutable; no new mutable state or borrowing
contract is introduced. Diagnostic payload construction can still allocate
in proportion to a long offending unit. The constant-allocation claim covers
numeric scanning and fixed-size outcomes, not arbitrary error-message text.

## Actual program measurements

These use unmodified compiler builds, including the compiler rebuilt from
the refreshed seed. Input is kept alive across allocation-free snapshots;
traffic/live figures are deltas and peaks are whole-process requested bytes,
including input construction. Requests include headers, excluding allocator
rounding, native buffers and registry storage. Before snapshots precede the
first measured parse. All measured Result cases and values/messages are
checked on every iteration.

| Workload | Traffic before → after | Peak before → after |
| --- | ---: | ---: |
| 1,000 nine-digit values | 274,000 → 24,000 B | 274 → 97 B |
| One 65,536-digit zero | 1,703,976 → 24 B | 1,769,529 → 65,577 B |
| Ten negative, zero-padded 4,098-byte values | 2,046,680 → 240 B | 184,374 → 8,227 B |
| Ten 4,096-digit prefixes followed by é | 1,066,060 → 860 B | 110,719 → 4,180 B |
| Overflowing 65,536-digit input | 1,703,952 → 0 B | 1,769,505 → 65,553 B |

Both modes agree. Input construction can dominate the new peak, especially
under ASan's exact allocation sizes and the generic append reserve policy.
The guard compares peak against pre-parse peak/live storage plus bounded
parser workspace. Its independent traffic/count budgets reject per-digit
allocations regardless of the input's existing capacity. No measured workload
retained additional live storage after parsing.

## Compiler impact

The seed refresh makes the compiler's own lexer use the byte parser. On the
same current compiler sources, observed cumulative emission traffic was:

| Emission | Before | After |
| --- | ---: | ---: |
| Debug | 1,078,668,826 B | 1,077,521,721 B |
| Release | 911,513,775 B | 911,042,320 B |

The observed savings are about 1.15 MB / 0.47 MB, with 39,016 / 16,338 fewer
allocation events. These are small whole-compiler changes compared with the
large benefit to callers parsing long inputs. RSS was 127.6 → 128.4 MiB debug
and 113.9 → 114.3 MiB release; no peak-RSS or timing improvement is claimed.
Allocation counts vary slightly with allocator placement/rounding. Requested
traffic is separate from live memory and RSS.

## Verification and reproduction

The public fixture uses an independent character-based oracle that accumulates
negatively for both signs. It checks signed bounds, explicit grammar/error
cases, all 256 actual byte values over 8 prefixes and 4 suffixes (8,192 raw
cases), long leading zeros, error precedence and retained String/Bytes/CStr
aliases. The raw constructor is checked against Bytes.to_array so the test
cannot silently substitute UTF-8 encodings for individual bytes.

Generated public properties test zero-padded spellings, exact invalid-character
messages and overflow precedence. Focused debug/release guards run in ordinary
and diagnostic modes, with ASan/LSan; they are integrated into compiler memory
and sanitizer gates. The allocation fixture holds its 65,536-byte input live
and permits just that one input cell plus one 24-byte Result. Source-built docs
are regenerated. The refreshed seed is rebuilt and reproduction-checked;
native sources and their fingerprint are unchanged.

```sh
python3 bootstrap/parse-int-check.py /path/to/compiler
python3 bootstrap/parse-int-check.py /path/to/compiler --sanitize
python3 bootstrap/parse-int-check.py BEFORE_COMPILER --measure-only --out /tmp/int-before.json
python3 bootstrap/parse-int-check.py AFTER_COMPILER --measure-only --out /tmp/int-after.json
```

[Raw records](int-parsing-measurements.json) distinguish the exploratory
three-way comparison (which warmed validation before snapshots), actual
before/after programs, and compiler-only profiles. Public numeric results and
messages are the compatibility contract; this change does not promise
byte-identical generated LLVM, because the compiled standard-library body
changes.
