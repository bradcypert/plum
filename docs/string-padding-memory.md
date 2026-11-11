# Perceus-aware String padding

All three String padding operations now use one consuming storage operation.
They retain codepoint widths, single-character fills, no truncation and the
center operation's odd remainder on the right. Invalid fills and widths that
need no padding still return the original String.

## Design and alternatives

The prelude keeps responsibility for counting characters and choosing the left
and right repetition counts. The runtime validates the final byte size, then
either reuses uniquely owned capacity or creates one exact-sized result. It
moves the original text with `memmove` and fills both sides directly. The shared
`plum_str_repeat_into` helper doubles initialized prefixes without overlapping
copies; `String.repeat` uses this same helper. There are no temporary repeated
fill strings, character arrays, or input-sized stack buffers.

| Approach | Evidence and decision |
| --- | --- |
| Repeat and concatenate in Plum | Existing behavior. It already uses Perceus, but can allocate separate fill and intermediate output cells. Large centered padding used three allocations. |
| Always create one compact result | Prototyped. Removes temporary buffers, but changes an existing zero-allocation right-padding case to one allocation. |
| Consume the input, reuse fitting unique capacity, otherwise copy exactly | Selected and implemented. Preserves existing reuse, enables it for left/center padding, and removes fill temporaries. |
| Incrementally append fill units | Could retain consuming append, but repeats growth checks and cannot remove geometric capacity overhead when the final size is already known. |
| Expose a new public builder or String layout | Unnecessary for this operation; the existing ByteStorage layout and Perceus ownership rules suffice. |

The optional comparison in `bootstrap/measure-string-pad.py` builds isolated
C/LLVM models of the two proposed alternatives with the before compiler. Those
models replace only the public padding entries and verify their bytes through
an independent observer. They are measurement experiments; production padding
is implemented in the prelude, compiler lowering and emitted runtime.

## Ownership and bounded construction

`plum_str_pad` consumes its String input and borrows the fill. The compiler
already prepares owned arguments and moves their final uses. For this private
primitive it transfers the first argument to the kernel, then releases the
remaining prepared arguments. Other runtime calls retain their existing cleanup
policy. Public functions, partial applications and first-class calls all pass
through this boundary.

An unchanged result returns that owned input directly. Padding mutates storage
only when its reference count equals one, its capacity already fits the entire
result, and its pointer differs from the fill's pointer. Shared and immortal
cells take the copy path. The explicit alias check also protects native callers
borrowing fill from the consumed input itself. Copies finish before the input
reference is released; fill remains borrowed throughout. Managed String, Bytes
and CStr aliases therefore preserve their contents and lifetimes.

Both byte lengths, both counts, their sum, the repeated-byte product and final
String header/NUL bounds are checked before allocation or payload access. An
empty fill is rejected by the private kernel for positive counts; the public
functions return unchanged for that invalid fill. The native tests include
header-only overflow and allocation-failure inputs, making premature reads or
writes visible under ASan.

The fill's byte length is not assumed to be at most four. Existing character
counting groups an initial byte with following continuation bytes even for text
created through unsafe code. Such a one-unit fill may be a long continuation
run. Padding preserves the old byte composition for these inputs, with checked
sizes and the same bounded-space repetition helper.

## Produced-program measurements

```sh
python3 bootstrap/measure-string-pad.py BEFORE AFTER --compare-designs \
  --out docs/string-padding-measurements.json
python3 bootstrap/string-pad-check.py AFTER --sanitize
```

The artifact contains 480 observations: 432 ordinary before/after cases and
48 explicitly labeled prototype cases. Production measurements use ordinary
public API builds with memory diagnostics enabled, without substituting LLVM.
Inputs include literals, exact
repetition and ordinary growing concatenation, with unique/shared ownership,
ASCII, multibyte fills and embedded NULs. An allocation-free native observer
checks every output byte, stored length and terminating NUL. Arguments are kept
alive across snapshots so their cleanup cannot masquerade as a memory saving.

Debug and release agree on these ordinary Linux requested-byte examples:

| Operation | Allocations before → after | Cumulative requested bytes | Result requested bytes |
| --- | ---: | ---: | ---: |
| Pad literal `data` right to 262144 | 2 → 1 | 524318 → 262161 | 262161 → 262161 |
| Pad literal `data` center to 262144 | 3 → 1 | 524335 → 262161 | 262161 → 262161 |
| Pad literal `data` left to 262144 | 1 → 1 | 262157 → 262161 | 262157 → 262161 |
| Exact 4096-byte input, left to 4097, unique | 1 → 0 | 4114 → 0 | 4114 → 4113 |
| Exact 4096-byte input, right to 4097, unique | 0 → 0 | 0 → 0 | 4113 → 4113 |
| Exact 4096-byte input, center to 4097, unique | 1 → 0 | 4113 → 0 | 4113 → 4113 |
| Grown 4096-byte input, center to 5120, unique | 4 → 1 | 12622 → 5137 | 6939 → 5137 |

Large centered padding's requested Plum heap peak falls from **524465 to 262291
bytes**, with the same final 262161-byte result. Large right padding's peak
falls from 524448 to 262291. These peaks include registered argument/setup storage;
registry bookkeeping and C-library internals are outside the counters. Small
cases can have unchanged peaks because input construction already set a higher
peak. Cumulative traffic, requested live/peak heap and RSS are distinct; no RSS
or speed improvement is claimed.

The old large left-padding case already allocated once. Its repeated fill cell
used allocator rounding to accommodate the four source bytes, so diagnostics
retained its original 262157-byte request. The new exact allocation requests
262161 bytes. This four-byte increase is deliberate; the old identity/width
controls remain unchanged, and right/center temporary buffers are eliminated.

## Capacity retention and portability

Reuse trades allocations for retained capacity, as existing Perceus append
does. A grown input in the measurements owns a 5043-byte request for 4096 bytes
of text. Padding it to 4097 now reuses that request for every alignment. Old
left/center padding instead returned requests of 4114/4113 bytes. The new paths
allocate nothing but retain about 930 more requested bytes while the result is
live. The existing right-padding path already made that tradeoff. Shared input
storage is preserved and the result receives one compact request; oversized
unique inputs that do not fit also receive one exact result, without adding
geometric reserve to an already known final size.

The 4097-byte examples depend on the host's actual usable capacity. Reuse is
guaranteed only when capacity fits, not by a particular rounding assumption.
The native guard derives its expected branch from the allocator; the public
guard grows until it observes spare capacity, then requires zero allocations
for unique input and one exact allocation for shared input. Both run with
ASan's exact capacities as well as ordinary allocation. The `pad_exact` fixture
uses static input and requires exactly 100 allocations / at most 411300
requested bytes for 100 mixed-alignment pads, independent of allocator rounding.

## Compiler impact and validation

`string-padding-compiler-measurements.json` compares the merged #497 compiler
with a compiler rebuilt through the regenerated seed on identical final inputs.
One warmup and two alternating measured samples show release emission traffic
increasing about **48 KB**, and debug emission traffic decreasing about **16 KB**
out of roughly 882 MB / 1.05 GB of cumulative requests. This is essentially
flat compiler allocation traffic, not a compiler RAM reduction. Instrumented
program padding costs above describe the binaries it produces.

The public property checks byte placement through existing public repetition
and concatenation APIs across widths, Unicode and NUL fills. The independent
native guard checks unique/shared/static inputs, self-aliasing, original
allocation provenance on reuse, exact copies, final release, long continuation
fills, representable scalar limits, overflow and allocation failure. Public
fixtures also cover String/Bytes/CStr aliases, higher-order calls and later
argument reads. All four ordinary/tracked debug/release modes run, with address
and leak checks on supported native platforms.

This preserves completed #23's formatting API and #189's date formatting.
Completed #6 and open #295 concern terminal display cells and cursor-moving
controls, rather than String's codepoint padding. Open #33's interpolation
specifier syntax remains deferred. The change introduces no public API,
ByteStorage representation or native shim change; the refreshed seed ships
the implementation in distributed compilers too.
