# Issue 63 implementation and results

Implemented locally on 2026-10-05/06 against parent revision
`ae5303f34a6fd2c889cf0a39970b8810d165e714`.
Issue: https://github.com/bradcypert/plum/issues/63

## Delivered behavior

`plum run` reuses validated whole-program LLVM IR. Source/dependency
discovery is shared with ordinary compilation. Lookups verify source
contents, membership/order, manifests, package pins, paths, compiler bytes,
modes and the parser's actual compile-time embed reads. Valid hits skip
parsing, type checking and emission; failed edits cannot execute an older
successful program. File and project forms use the same path.

The Linux default-Clang adapter additionally reuses compiled objects.
It fingerprints the actual executable and loaded libraries, certifies the
effective in-process compile command, and preprocesses C afresh before
lookup. The captured preprocessor output is the compilation input, so
header edits, earlier search candidates and time-dependent macros are
reflected. On C object misses, Clang's parsed AST rules out assembly that
can read additional files; symbol labels must have safe emitted names.
Concatenated/escaped assembly strings cannot hide those dependencies.
Generated LLVM is scanned for actual assembly tokens while skipping quoted
data, identifiers and comments. Objects with assembly remain uncached.
The native-object key carries a validation version independently of the
outer cache-directory schema.

Opaque drivers, unsupported hosts/loaders and uncertified configurations
use the ordinary source compile/link path while retaining validated IR.
The adapter rejects loader overrides, Clang option overrides, external
compiler execution, plugins, profile inputs and precompiled-header options
it cannot certify. macOS and Windows currently reuse IR only.

Every invocation links a fresh temporary executable. Library replacement,
search shadowing, startup objects and linker behavior are therefore resolved
again rather than approximated with an executable-cache key. Arguments,
stdin/stdout/stderr, working directory, terminal access and the existing
nonzero-child-status handling are preserved. macOS retains its existing
compile/link/dSYM path. No executable runs from the cache.

IR/object entries and shared embedded shims are integrity-checked and
published as complete directories by same-filesystem rename. Concurrent
writers may duplicate work; readers capture verified IR or private object
copies before linking. A concurrent cleaner or failed cache extraction
recovers through private compilation, including IR-only reuse. Cache
retention uses a default 1 GiB budget with oldest-published eviction after
cached compilation, configurable through `PLUM_BUILD_CACHE_MAX_BYTES`.
Abandoned staging directories older than 24 hours are cleaned only when
the recorded owner process is gone. Explicit `plum cache clean build` and
existing package-cache commands retain their scope.

Cold compilation improves through two measured changes: parser source
characters are decoded once for documentation extraction, and equality/
release helper membership uses emission-scoped maps with the original
symbol identities and output ordering. Whole-program checking, global
initializers, reachability and emitted helpers keep their semantics.

Profiling, diagnostics, bypass controls, documentation, maintenance and
platform CI coverage are included. The checked-in compiler and LLVM seed
are regenerated; the seed refresh is necessary because native sources and
the compiler's extern dependencies changed. Bootstrap scripts now include
the time shim used by profiling. Cross-check also supplies the required TLS
headers and correctly checks intentional nonzero 32-bit target refusal.

## Measurement method

The definitive measurements are in
[issue-63-measurements.json](issue-63-measurements.json), including all
samples, compiler digests, host/toolchain identity and phase profiles.
Both compiler executables are release builds. Application `run` commands
use the ordinary default debug mode.

The baseline is built with the original checked-in compiler from a pristine
archive of the parent revision, including its original embedded native
sources. Both binaries then process the same modified current compiler
source and copied example fixtures. Three samples run sequentially without
other compilation jobs. Each empty-cache sample gets its own cache; warm
samples follow a successful population. Edit samples make distinct real
source changes while retaining reusable shim objects. Wall time includes
process startup, compilation/validation, linking and the small program's
execution. It is not a direct TUI time-to-first-frame measurement.

The environment's `/tmp` quota prevented normal sandbox setup and scratch
creation. All builds/tests used workspace scratch through `TMPDIR`.
This is an artifact-cache comparison on a warm filesystem, not a disk-cache
flush experiment. Preliminary comparisons were repeated after constructing
the pristine baseline; only the final data is reported below.

Median wall times in seconds:

| Workload | Baseline run | Final uncached | Empty cache | Warm cache | Source edit |
| --- | ---: | ---: | ---: | ---: | ---: |
| Hello | 1.085 | 0.883 | 1.944 | 0.540 | 0.694 |
| JSON/files | 1.446 | 1.172 | 2.239 | 0.549 | 1.023 |

| Workload/stage | Baseline | Final | Improvement |
| --- | ---: | ---: | ---: |
| Hello LLVM emission | 0.327 | 0.030 | 10.8x |
| JSON/files LLVM emission | 0.413 | 0.073 | 5.7x |
| Whole-compiler checking | 6.483 | 0.550 | 11.8x |
| Whole-compiler LLVM emission | 36.107 | 3.231 | 11.2x |

The final compiler profile puts helper discovery at 0.150 seconds and
function emission at 2.127 seconds. The earlier before/after phase probe
measured helper discovery dropping from 25.751 to 0.142 seconds; final
source has grown slightly since that probe. Prelude/stdlib work is about
0.025 seconds. Warm application profiles confirm IR/object hits, fresh
preprocessing/linking and no checking/function emission.

Whole-compiler peak RSS is 109 MB for emission and 41 MB for checking,
within the existing 128/64 MB ceilings. Profiling-enabled JSON emission
measured 0.063 seconds versus 0.073 with logging disabled in this small
sample; that variation does not establish zero instrumentation overhead.
No claim is based on subtracting noisy whole-command timings.

## Correctness evidence

The [local validation record](issue-63-validation.json) includes the broad
suite and targeted final reruns. Performance measurements are separate.

- `bootstrap/build-cache-check`: 310 assertions pass. They cover unchanged
  hits without checking/emission, same-size/same-mtime edits, file-set
  changes, hidden and explicitly named files, Unicode/space paths, symlinks,
  dependency edits/errors/conflicts, external embeds, modes, corrupt
  IR/metadata/objects/shims, compiler replacement, loader and driver bypass,
  default Clang configurations, native header/search changes, library
  replacement/search shadowing, assembler-file changes/removal, inherited
  arguments/stdin/cwd, real terminal streams, failed edits and child exits,
  concurrent publication/cleaning, forced native-input removal, cache I/O
  failure, inspection and cleanup.
- `bootstrap/pkg-check`: 64 checks pass, including transitive dependencies,
  cycles, Git revisions, vendoring/offline resolution and a warm run refusing
  a tampered pinned non-source asset.
- `bootstrap/self-test`: 19 internal tests pass, including SHA-256 vectors,
  unresolved loader output and assembly AST/LLVM classification.
- The complete maintenance pre-commit suite passed, including 190 sanitizer/
  rejection corpus fixtures, properties, allocations, docs/examples, CLI,
  packages, lexer round trips, formatting, stdin/TTY, networking and LSP.
  Compiler fixed-point and arbitrary-directory self-sufficiency proofs pass.
  `check-seed` proves a fresh clang-only seed produces the current compiler.
- Native shims and the seed cross-compile/link for macOS arm64/x86_64,
  Windows x86_64 and Linux arm64. A real `build --target` produces the
  expected object formats; Linux arm64 executes under QEMU and 32-bit
  targets are refused.
- Baseline/candidate IR is byte-identical for both measured applications
  and the complete current compiler. The earlier optimization comparison
  also matched all ten example IR outputs byte for byte.
- Native SHA-256 was compared with Python `hashlib` across block/padding/
  buffer boundaries, embedded NULs and large toolchain files. Optional Linux
  kernel acceleration matches the portable implementation and falls back
  when the provider or cross-sysroot header is unavailable.

Actual macOS/Windows runtime cache checks and native Linux arm64 cache
checks passed in the corrected PR CI run:
https://github.com/bradcypert/plum/actions/runs/37498631558.
Native Windows console attachment remains outside the Unix PTY checks.

### Windows CI follow-up

The first PR run passed Linux, Linux arm64 and macOS validation. Windows
built the compiler and passed the language-server and ordinary-program
checks, but the cache fixture with `ü` in its path missed repeatedly.
Windows native argv contains active-code-page bytes; decoding a saved path
as UTF-8 rejected that unchanged filename. Saved paths now round-trip as
bytes through the native file-hashing helper. Malformed hex and embedded
NULs are rejected before opening a file.

A Linux filename containing a non-UTF-8 byte reproduces the original
failure and now has regression coverage for both warm hits and edited
source misses. The harness also distinguishes MSYS/Cygwin Python from a
Unix compiler host: native Windows cannot use Unix PTYs, MSYS symlinks or
Unix executable paths as if it were a POSIX process. Argument, stdin, cwd,
exit-status, invalidation, corruption and concurrency checks still run on
Windows; Unix terminal and wrapper checks run on Linux/macOS. Windows
console attachment is not covered by these Unix PTY checks.

The measurement JSON describes the original implementation in commit
`6f97f6c`, before this correction. Its binary hashes and timings are retained
as recorded, rather than attributed to the rebuilt compiler.

## Tradeoffs and remaining gates

Initial object-cache population is slower than an uncached invocation on
these small fixtures. Toolchain hashing, fresh preprocessing, parsed assembly
validation on misses and fresh linking are intentional correctness costs.
Warm object hits need no AST validation or native compilation, but still
hash the toolchain, preprocess C and link. `PLUM_NO_OBJECT_CACHE=1` retains
IR reuse with the ordinary native compilation path.

The provisional under-200-ms unchanged-run and <=10% miss-overhead targets
are not met on these fixtures. The original gh-dash source/revision was not
available, so neither its reported slowdown nor a 10x TUI improvement is
claimed resolved. Measure that workload and real platform CI before closing
the issue's performance acceptance gates. Native object support for macOS/
Windows also remains a separate adapter project.

True per-module compilation is not implemented. The
[incremental compilation proposal](incremental-compilation.md) defines the
next architecture gate: initially retain full checking, then partition
validated typed emission into stable units with explicit generic/helper
ownership, ABI/layout dependencies, SCC handling, initialization order,
debug behavior and clean/incremental equivalence tests.

As with ordinary compilation, source inputs and the toolchain must remain
stable during a command. Observed source changes prevent publication under
the previous input identity; the cache does not promise an atomic snapshot
of a concurrently edited project/toolchain.

## Reproduce

Build a release baseline from the pristine parent revision in a scratch
directory, using that revision's compiler and native sources. Keep its
executable outside the source archive before removing the archive so the
repository-wide formatting check does not scan duplicate intentional-error
fixtures. Build today's compiler with `./sh build bootstrap/self_host -o
sh.real --release`, then run:

```sh
bootstrap/measure-build-cache --baseline /path/to/pristine-baseline --out measurements.json
bootstrap/build-cache-check
bootstrap/pkg-check
bootstrap/check-seed
bootstrap/cross-check
```

The measurement harness uses isolated temporary caches and checks output
and IR equivalence. No timing threshold is added to correctness CI.

### Cache follow-up acceptance

A dedicated Ubuntu 24.04 / Clang 18 CI job requires native object hits;
portable jobs still exercise conservative adapter fallback. Linux cache
checks resolve actual linked Plum and C source locations after both cold
and warm runs, then repeat after editing both sources. Retention checks
cover budget eviction, zero/invalid budgets, live/abandoned staging,
symlink boundaries and concurrent eviction.

Local follow-up validation on 2026-10-07 passed all 403 cache assertions
with `--require-objects`, the maintenance suite (including 190 sanitizer/
rejection fixtures), compiler unit/format checks, seed bootstrap,
self-compilation and self-sufficiency. Emission/check RSS remains 109/41 MB.
All four cross-compilation targets pass, with Linux arm64 running under
QEMU. The refreshed compiler emits LLVM byte-identical to the refreshed
seed. The new Clang 18 CI job and native Windows junction assertion still
require CI execution after these follow-up changes are pushed.

The performance acceptance gate remains a separate realistic edit–run
benchmark comparing full caching, IR-only caching and uncached execution,
including the gh-dash/TUI workload and compiler upgrades. The default
cache mode is unchanged; existing measurements do not cover that session.

### macOS retention correction (2026-10-09)

The macOS follow-up CI run exposed a dangling `CStr` in pruning: the
cache-root String temporary was released before the native helper read
its bytes. Eviction and abandoned-stage cleanup silently did nothing on
Darwin; Linux and Windows allocators happened to preserve the bytes.
The pruning wrapper now binds the owning String through the native call.
The linked-debug fixture also binds its executable path before borrowing
it for native copying.

The Linux retention regression disables glibc tcache and poisons freed
memory. It fails against the previous compiler and passes with the root
owner bound, exercising the same failure without requiring a Mac.
Eviction failures now report entry sizes, timestamps, the configured
budget and the compiler trace. The existing seed can bootstrap this
source change; a seed refresh is unnecessary if `check-seed` passes.
