# Find qualified-name separators without repeated scans

After #475 and #476 removed character materialization from shared String
counts and slices, the typechecker's last-dot search still called both APIs
for every character. That repeatedly scanned the entire name for its length
and each growing prefix for a one-character slice: quadratic byte visits and
one temporary String per character.

The shared implementation in `typecheck/qualified_names.plum` now scans a
Bytes view once, retaining character-index semantics for both callers.
`module_of_qualified` still extracts the namespace with `String.slice`, and
`rewrite_type_path` still resolves file-local aliases using that namespace.
Their string work takes a bounded number of further linear passes;
they no longer rescan every prefix while looking for a separator.

## Design and alternatives

| Approach | Assessment |
| --- | --- |
| Shared byte scan returning a character index | Chosen: no scanner allocations, constant state, preserves the callers' slicing and alias interfaces |
| Decode once to a character array | Removes quadratic rescans, but retains an array and a String per character |
| Use Bytes.at at each offset | Bounds-safe, but materializes an Option per read; use the existing private scalar primitive with explicit bounds instead |
| Return byte offsets and retag byte slices in callers | Avoids some additional linear passes, but moves byte/character and malformed-input reasoning into both consumers |
| Store structured or interned qualified names throughout the compiler | Could avoid repeated work across phases, but changes AST/type representations and lookup interfaces; a separate design |
| Add a public reverse-search API | Broadens library behavior for one compiler separator case; unnecessary for this contained fix |

The scanner retains the same owned Bytes view through its tail call. It
counts the first byte and every non-continuation byte as character starts,
matching `chars_of` even for malformed native text. A dot followed by a
continuation byte belongs to a larger character group and is not the single
character "."; a bounded lookahead preserves that distinction. Byte reads
are guarded by the stored length, including empty inputs and trailing dots.
The recursive call is guaranteed `musttail`.

The low-level byte read is an existing runtime primitive, confined to this
module. There is no new native API, string representation, persistent cache,
global scan state, or change to alias lookup/visibility rules.

## Memory and Perceus

This is a compiler optimization. Generated user-program ARC/FBIP decisions,
runtime functions and prelude implementations are unchanged. The compiler's
scanner uses ordinary Perceus ownership to share and then release its input
view. Namespace extraction allocates only its result String; alias rewriting
retains its existing result and temporary namespace lifetimes.

Removing temporary per-character Strings reduces allocation traffic and the
number of RC-managed objects created. It does not establish a lower whole-
compiler live-heap peak. There is no backing-buffer retention cache. Scanner
state and call-stack use are constant; extracted results are proportional to
the requested namespace length. Source representations elsewhere in the
compiler retain their existing costs.

This change is in compiler source, rather than in the compiling executable's
embedded prelude/runtime. The existing seed can compile it directly into the
distributed source-built compiler. Seed equivalence is checked; a seed refresh
is not needed merely to move these functions into their own source module.

## Coverage and the malformed-byte correction

The independent compiler-unit oracle materializes characters and checks both
the last-dot position and extracted namespace across 588 combinations of
Unicode, dots, NULs and malformed fragments. Long Unicode prefixes and lone
continuation runs exercise bounded stack use. A second unit checks matching,
missing, nested and file-local alias bindings without changing resolution.

Plum's `\x80` escape encodes a Unicode character; it does not construct a lone
raw byte. The unit and existing character-count/slice executable fixtures now
construct malformed input through explicit Bytes and unsafe CStr conversion,
and assert the resulting byte arrays. This corrects the earlier fixtures'
input construction and exercises actual invalid sequences. No library change
was required: the previous count/slice implementations preserve those groups.

`bootstrap/qualified-name-check.py` copies the actual self-contained compiler
module into an isolated executable, rather than duplicating its algorithm.
It extracts the namespace 100 times from names with 128, 256 and 512 repeated
Unicode pairs, in debug and release modes. The byte ceiling is linear and the
128-allocation ceiling rejects per-character temporary objects. Geometric
input construction makes the setup independent of allocator size classes.
The guard runs from `mem-check`; its sanitizer variant also executes the
boundary unit in both modes and runs from `sanitizer-check.py`.

At width 128, the unchanged baseline helper requests 523,987 bytes in 26,208
allocations and fails the guard. The new helper requests 41,387 bytes in 108
allocations. At widths 256/512 it requests 80,956/160,077 bytes in 109/110
allocations. The remaining allocations construct inputs and copy namespace
results; separator scanning adds none.

## Related issues

The search found #118's same rescan pattern in path helpers, already fixed
with byte scans. #440 remains a separate alias-qualified type-resolution
report; this PR preserves existing lookup behavior and does not close it.
#63 remains the broader compiler-cost work. Closed #212 and #320 established
name-resolution and symbol-identity requirements that remain intact.

## Whole-compiler measurements

The baseline is the final #476 compiler, whose source tree is identical to
merged main `f4904a0`. The candidate is built directly from that same seed
and current compiler source. Both process identical current inputs. One
warmup and three serial interleaved measured rounds run on Linux x86_64 with
Clang 23.1.4. [Raw samples and hashes](compiler-qualified-names-measurements.json)
include warmups. Check output and complete release/debug LLVM are byte for
byte identical across implementations and all rounds.

| Whole-compiler workload | Requested MB before → after | Allocations before → after | Median seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1072.72 → 1071.44 | 16,146,134 → 16,068,842 | 4.560 → 4.553 | 115.1 → 115.2 |
| Debug emit | 1239.86 → 1238.24 | 19,392,467 → 19,316,022 | 4.756 → 4.731 | 129.5 → 129.3 |
| Check | 285.76 → 285.09 | 8,641,418 → 8,604,127 | 1.666 → 1.675 | 47.4 → 47.3 |

Requested MB are decimal cumulative allocation traffic. Savings are about
1.28 MB per release emission, 1.61 MB per debug emission and 0.67 MB for
checking. These are small whole-workload changes (about 0.1–0.2%); ordinary
compiler names are short and many other allocations remain. Timing ranges
overlap and peak RSS is effectively flat. The main result is bounded linear
separator scanning for long names, not a demonstrated general build speedup
or lower peak heap.

```sh
python3 bootstrap/compiler-profile.py --before /tmp/plum-slice-ci \
  --after /tmp/plum-qualified-final --samples 3 --case check-compiler \
  --case emit-compiler --case emit-compiler-debug --out /tmp/plum-qualified-bench
```
