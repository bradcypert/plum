# Checked replacement construction and unchanged-result sharing

`String.replace` already counted matches and allocated an exact-sized result.
It still copied the input when there were no matches or when pattern and target
were identical, and computed the new length with unchecked multiply/add. Empty
results also received a heap cell. This change retains exact construction while
sharing unchanged nonempty results, using static empty storage, and checking
all result-size arithmetic.

## Alternatives compared

[Program measurements](string-replace-measurements.json) compare the original
runtime, a source-level identity wrapper, and the final integrated constructor.
The wrapper shares obvious unchanged cases but retains the old changed-result
constructor; it does not contain the production size/failure checks. All three
use ordinary compiler builds without LLVM substitution. An allocation-free
native observer verifies every byte, output length and trailing NUL before
recording. Debug and release agree.

For unchanged 327,680-byte Unicode/NUL input:

| Approach | Call allocations | Requested traffic | Additional live requested bytes | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Original replacement | 1 | 327,697 | 327,697 | 655,394 |
| Source identity wrapper | 0 | 0 | 0 | 327,697 |
| Integrated constructor | 0 | 0 | 0 | 327,697 |

The source wrapper demonstrates the benefit, but adds a separate `contains`
scan before calling the existing changed-result path and cannot ensure safe
size arithmetic there. The integrated implementation uses the match count it
already needs to decide both sharing and exact construction.

Three no-match HTML escaping stages on a 229,376-byte input fall from
**three allocations / 688,179 requested bytes** to zero. With input kept alive,
additional live result storage falls from 229,393 bytes to zero and whole-process
peak from 688,179 to 229,393. Empty-pattern identity was already shared and stays
allocation-free. An empty result now uses static storage instead of a 17-byte
heap cell.

Changed nonempty output retains the existing one-allocation cost. For example,
UTF-8 expansion to 327,680 bytes still requests one 327,697-byte cell; shrinking
to 163,840 bytes still requests one 163,857-byte cell. No intermediate output
array or match-position table is introduced. The 300 recorded rows include
no-match, identical operands, empty patterns/input/results, UTF-8 growth/shrink,
NUL matching/insertion, nonoverlapping matches, path normalization and escaping
chains across five sizes in both modes.

## Checked plan, shared matcher, owned result

```text
validate all three stored lengths
  empty input → static empty
  empty/too-long pattern or identical pattern/target → owned input reference
count nonoverlapping matches using the existing byte matcher
  no matches → owned input reference
compute checked result size
  empty output → static empty
  changed nonempty → one exact cell, copy literal spans and replacements
```

The scalar plan validates input length `n`, pattern length `m`, and target length
`t` against the existing signed/header limit. For `m > 0` it requires
`0 <= count <= n / m`; for `m == 0`, count must be zero. The removed byte count
`count * m` therefore cannot overflow or exceed `n`. It then checks the inserted
size `count * t` and final addition `(n - count*m) + count*t` with the existing
checked helpers. This avoids relying on an unchecked signed delta/product.
All three headers are checked before comparisons, scans, reference updates or
allocation. After counting has read valid input, invalid counts/result sizes fail
in the scalar planner before output allocation. Allocation failure follows the
established fatal path.

Counting and filling now use the same existing `plum_str_find` matcher as
`contains` and nonempty-separator `split`. The writer copies whole literal spans
between matches rather than repeating its own character-matching loop. Matches
advance by the full pattern length, preserving left-to-right nonoverlap and
never revisiting inserted text. Empty patterns keep the behavior established
in [#247](https://github.com/bradcypert/plum/issues/247). UTF-8 and embedded NULs
retain their byte-level behavior.

This shared native matcher remains **O(n*m)** in comparisons with no auxiliary
heap storage. The change makes no linear-time search or throughput claim.
[#275](https://github.com/bradcypert/plum/issues/275) fixed the separate public
`String.index_of` codepoint-index API with source KMP; that implementation and
its guarantee are unchanged. Replacing the shared native search algorithm would
have wider scope across contains/split/replace. KMP tables would add storage;
a constant-space linear matcher would need its own design and validation.
Neither is necessary for the allocation and size-safety improvement here.

## Perceus ownership and capacity retention

The constructor borrows all three arguments and returns a +1 String owner. For
unchanged nonempty output it increments the input's existing reference count.
Changed output always owns independent exact storage, even if input has a unique
reference and surplus capacity, or if pattern/target alias input. Sources stay
alive through copying and are never mutated. Empty results use the static cell.
Normal Perceus ARC/last-use and copy-on-write behavior handles the returned owner;
no compiler consuming ABI or cell layout changes are introduced.

Sharing a whole unchanged cell retains its original capacity and can prevent an
alias from being updated in place. This is a real tradeoff, not zero retained
memory. A native check creates a 3-byte String in a 4,113-byte requested cell,
shares its unchanged result, drops the caller's input reference, and verifies
that the result remains valid while retaining all 4,113 requested bytes until
its final release. Previously a no-match replacement could compact that result
into a fresh 20-byte cell. The common case avoids a duplicate allocation/copy;
it does not promise that every unchanged result retains the smallest possible
buffer. Changed output never replicates reserve capacity.

Consuming in-place replacement would need a compiler-proven last-use boundary,
plus handling of overlapping input/pattern/target aliases and growth/shrink.
Reference count one alone does not permit mutation of a borrowed argument. The
existing borrowed contract provides these memory savings without that larger
ownership design.

## Larger memory story

Savings apply to **compiled programs using replacement**, especially no-match
normalization and multi-stage escaping. They reduce cumulative allocation
traffic and, when input and result both remain live, eliminate duplicate result
storage. Additional-live zero means sharing input already present in the before
snapshot. Whole-process peak includes setup, and requested bytes are distinct
from RSS and allocator rounding.

The compiler itself uses replacement in LLVM string escaping, HTML escaping,
path normalization and native-cache command normalization. Its separate refreshed
seed-to-source measurements below exercise real callers; they include combined
runtime/module and caller effects. Ordinary program changes still use one exact
allocation, and retained-capacity tradeoffs remain relevant to both compiler and
programs. No RSS or timing improvement is inferred from requested-byte totals.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-string-replace.py /path/to/before /path/to/after \
  --out /tmp/string-replace-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

A public byte-array reference checks method and namespaced results for 660
input/pattern/target combinations, including Unicode/NUL, repeated/overlapping
prefixes, empty operands and deletions. Additional tests cover large input,
String/Bytes/CStr aliases after rebinding/appending, and pattern/target aliases
of the receiver. Properties preserve identity rules and nonoverlapping behavior.

The emitted-runtime guard extracts actual compiler functions, testing O0/O2,
tracked/untracked and ASan/LSan. A separate native reference checks 2,048 binary
cases, exact requests and ByteStorage accounting, input immutability even at
unique ownership, aliased arguments, shared-result last-owner lifetime and
retained capacity, scalar near-limit arithmetic, header-only invalid input
lengths, buffer-free scalar size/count faults and injected allocation failure.
Header checks precede payload reads; OOM injection uses valid data because
counting necessarily reads input first.

Budgets for 1,000 public calls require unchanged and empty outputs to allocate
nothing; changed output requires exactly 1,000 allocations / 27,000 requested
bytes in the selected workload. The existing Path helper fixture also drops
eight allocations, and its count budget is tightened from 72,371 to 72,363.
The seed is refreshed and actually rebuilt, bootstrap/native filename checks
pass, and source docs are regenerated with that source-built compiler.

## Compiler measurements

[Compiler measurements](string-replace-compiler-measurements.json) compare the
predecessor and refreshed seed-to-source compilers on identical final compiler
and stdlib sources, with one warmup and two recorded samples per mode:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,077,155,386 | 1,046,848,275 |
| Release | 909,823,535–909,823,536 | 879,991,171 |

That is approximately **30.31 MB less debug traffic** and **29.83 MB less release
traffic** per emission in these samples. The values are cumulative requested
bytes, not peak RAM reductions. RSS is recorded separately, with no RSS or timing
improvement claimed. Other validation ran concurrently, so elapsed times are
omitted. Existing compiler memory guards continue to enforce their budgets.

To reproduce the compiler comparison on Linux:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --case emit-compiler --case emit-compiler-debug --samples 2 --out /tmp/replace-compiler-profile
```
