# Issue 63: investigation and implementation plan

Investigated on 2026-10-05 against source revision
`ae5303f34a6fd2c889cf0a39970b8810d165e714`.
Issue: https://github.com/bradcypert/plum/issues/63

## Recommendation and scope

Deliver a validated native artifact cache for `plum run`, then optimize
cold compilation using phase measurements. Treat per-module compilation
as a separate compiler architecture project, with its own design and
acceptance gate. Caching fixes unchanged runs; it does not fix the
edit/run loop by itself. Both outcomes need explicit measurements.

Correctness means a cache hit produces the behavior of a fresh build
from the same effective compilation inputs. If an input cannot be
validated, reuse only the earlier stages whose inputs are known, or
compile normally. Never guess that an opaque toolchain is unchanged.

This records the original investigation and plan. The implementation,
validation evidence, measured tradeoffs and remaining acceptance gates
are documented in [issue-63-results.md](issue-63-results.md). The separate
[incremental compilation proposal](incremental-compilation.md) completes
the design gate; per-module compilation remains future implementation.

## What the current code actually does

- `bootstrap/self_host/main.plum:3105`: `run_project` allocates a temporary
  directory, calls `compile_project`, runs the executable with inherited
  streams, and removes the directory. Every invocation recompiles.
- `main.plum:3003`: `compile_project` allocates another scratch directory.
  `compile_with_scratch` loads sources, emits LLVM, writes embedded C
  shims, invokes the selected C driver, and removes successful-build IR.
- `main.plum:3337`: project collection includes project sources and the
  transitive dependency closure. Hidden entries and root `vendor` are
  skipped according to specific traversal rules. A separate cache walker
  must not invent different rules.
- `main.plum:2880`: native sources and ordered link libraries come from
  CLI flags, the project, and transitive dependencies, selected by target.
- `main.plum:3548`: dependency resolution prefers vendored Git packages
  and verifies declared package hashes on every build. A hit must retain
  this verification and missing-dependency errors; it must not fetch.
- `parser/parser.plum:1420`: `@embed_file` reads text during parsing,
  relative to the containing source file. The resulting AST holds the
  string, so the original dependency is otherwise lost downstream.
- `codegen/codegen.plum:6998`: emission includes parsing the embedded
  prelude/required standard modules and whole-program type checking.
  `emit-llvm - check` is not a clean measurement of pure emission.
- `codegen.plum:7078`: generic instantiations already follow a reachable
  worklist rooted at the entry point and global initializers. The issue's
  unused-prelude hypothesis is not established by the current code.
- `codegen.plum:7016`: the runtime IR is emitted unconditionally. Helper
  functions, globals, strings, and debug metadata also contribute bytes.
- `codegen.plum:7390`: function emission already collects fragments and
  joins once. Replacing this accumulator again is not a useful plan.
- `pkg/fetch.plum:68`: cache root honors `PLUM_CACHE`, `XDG_CACHE_HOME`,
  then the home directory. Existing `cache list/clean` manage packages.
- `native_stdlib/os_shim.c:201`: tree copying does not preserve file
  modes. It cannot be assumed to safely materialize cached executables.

Some comments describe retired implementations or older behavior.
Implementation and executable harnesses take precedence over those claims.

## Preliminary measurements

Host: Linux x86_64, clang 22.1.8, existing local compiler reporting
`plum 0.0.37`, through `./sh` with a 60-second timeout and cgroup bypassed.
Three invocations each; values below are medians in seconds.

| Fixture | check | emit-llvm | build | run | LLVM bytes | definitions |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| One-line hello | 0.323 | 0.313 | 1.200 | 1.151 | 83,877 | 112 |
| examples/json_and_files | 0.352 | 0.412 | 1.646 | 1.593 | 360,755 | 206 |
| bootstrap/self_host | 6.270 | 34.963 | not measured | not measured | 25,936,183 | 2,978 |

These establish repeated work, not an isolated performance baseline.
Build/run samples overlapped a compiler-source benchmark, and the local
binary has not been rebuilt and verified against the investigation HEAD.
`emit-llvm` measurements include output writing. The original gh-dash
source/revision is not supplied by the issue and was not found locally.
Do not extrapolate these Linux numbers to Apple clang 17/macOS arm64.
The compiler fixture includes its embedded source/shim bundle, so its
large IR does not independently establish unnecessary runtime emission.

The environment's `/tmp` quota prevented ordinary sandbox startup and
temporary-file creation. Measurements used workspace scratch directories;
build/run also used an isolated `TMPDIR`. No compiler code was changed.

## 1. Establish a reproducible baseline and input contract

Build the current compiler through the documented guarded workflow; record
source revision, compiler digest, platform/architecture, C driver, SDK,
build modes, and commands. Pin the original gh-dash revision if available.
Otherwise use a checked-in representative multi-module TUI fixture without
network calls, plus hello, JSON, generics/closures, TLS/native integration,
and the compiler itself. Label substitute workloads explicitly.

Add optional phase timing/statistics on stderr, leaving normal output and
LLVM stdout unchanged. Measure dependency verification, source discovery,
lex/parse, prelude/stdlib loading, type checking, reachability, helper
discovery, function emission, debug metadata, final joining/writing,
shim extraction, C compilation, linking, and macOS `dsymutil` separately.
Count IR bytes by section and concrete function/helper instantiations.
Measure instrumentation-disabled overhead before retaining instrumentation.

Run sequentially on an idle host: fresh cache, unchanged warm run, one-line
edit, dependency edit, native edit, debug/release/trace. Separate process
startup from compiler work and time to application readiness. Record
median/spread and RSS; distinguish warm filesystem state from artifact hits.

Before enabling cache hits, write a supported-input contract for every
platform. Toolchain/dependency discovery in section 3 is a gating prototype,
not something to leave until after cache integration.

## 2. Share discovery and record actual source dependencies

Introduce a build-input description rather than adding another whole-tree
hash in `run_project`. Resolve the package closure once and share its
results between source collection, native selection, and link arguments.
Preserve discovery/order semantics initially; do not independently sort
compiler inputs while claiming equivalent initialization behavior.

Record logical module identity, source path/spelling, ordered source list,
raw file content digests, manifests (including absence), resolved package
identities/paths, dependency graph, and selected native inputs. Rediscover
the source set on each lookup to catch additions, deletions, renames,
newly invalid dependency-root sources, and native-directory errors.

Have the parser record each actual `@embed_file` read in the build context.
Save that list with successful artifacts and rehash it on subsequent
lookups. Changed source always forces rediscovery before publishing.
Do not regex-scan Plum text for embeds: strings, comments and escapes
must follow parser semantics. Include embeds outside the project tree.

Use a versioned, unambiguous, length-delimited encoding and SHA-256 over
raw bytes and identities. Reuse the existing hashing approach, not its
whole-tree selection: hashing outputs/logs/runtime data causes needless
misses and can feed cached outputs back into their own key. Do not accept
mtime/size as evidence of identical contents.

Include the effective source paths and working directory where they
affect debug metadata or relative link inputs. Resolve symlink identities
without silently changing the compiler's existing module semantics.
Unreadable required inputs are errors, not empty bytes or a hit.

## 3. Define validated cache layers

Use a schema-versioned sibling of the package cache, such as
`<cache_root>/build/v1/`. A candidate index can locate prior manifests;
it is not itself proof that an executable is reusable.

The Plum-stage key includes source/dependency/embed inputs, the digest of
the actual running Plum compiler, debug/release/trace mode, entry kind,
and all effective lowering options. Compiler version alone is insufficient
for development builds. The executable digest also covers its embedded
runtime, prelude, stdlib and shims; hashing the checkout's shim files
would identify different inputs from those the installed compiler uses.

The native-stage manifest additionally includes host OS/architecture,
effective target, ordered C-driver arguments and ordered link inputs,
resolved compiler/linker identities, relevant build environment, native
source/include closure, and resolved startup objects/libraries/linker
scripts/SDK resources. Normalized flags must match the actual invocation;
library order matters. Runtime-only application arguments/environment
must not create misses.

**Native dependency discovery is the difficult part.** Prototype compiler
dependency/preprocessor output and linker input tracing on Linux, macOS,
and Windows before selecting the implementation. `clang --version`, the
driver path, or a `-MMD` file alone do not prove validity: system headers,
an earlier newly added include, library search changes, SDK updates,
compiler wrappers, and time-dependent macros can change results.

Choose a documented supported toolchain adapter that can revalidate the
effective preprocessing inputs and link resolution, including missing
search candidates that can later shadow a previous file. Hash toolchain
components/resources and compile-affecting environment or supply a
controlled build environment with explicitly preserved semantics.
Depfile parsing must handle escaping and Windows drive paths.

Opaque/custom `PLUM_CC` commands need an explicit fingerprint/dependency
protocol or must bypass native artifact reuse. In that case reuse valid
Plum IR and rebuild native stages. Apply the same rule to unresolved
native/include/library inputs; report the reason in optional diagnostics.
Do not exclude all native projects and call the TUI problem solved.
Validate the actual gh-dash toolchain/input path before final acceptance.

The desired unchanged path does no parsing, checking, code generation,
or compilation. Validation work may invoke cheap discovery probes;
measure their cost. If full validation is too costly, make the resulting
performance/correctness tradeoff explicit before selecting the design.

## 4. Publish and execute artifacts safely

Build in a uniquely owned staging directory on the cache filesystem.
Record the complete dependency manifest, binary digest, schema and all
required sidecars. Publish a complete directory atomically only after
successful compilation and validation. Do not expose a binary before its
manifest exists. On input changes during compilation, discard/retry or
return an uncached result; never publish under an unrelated input key.
Use a consistent captured source snapshot where practical. Native inputs
also need consistency checks; pre/post hashes alone cannot detect every
change-and-revert race. State the ordinary build concurrency assumptions.

Handle two writers for one key, interruption, partial entries, malformed
manifests, corrupted binaries and interrupted cleaning. Duplicate work
is acceptable; partially valid hits are not. Use private directories and
immutable published artifacts. Do not assume identical builds produce
identical binary bytes when paths/timestamps differ.

Choose execution/materialization semantics explicitly. A per-run copy
preserves the existing temporary-executable behavior and avoids executing
an inode being replaced or removed by cache maintenance. It requires
mode-preserving copy support and costs disk work. Direct cache execution
needs reader lifetime protection and Windows deletion handling; it also
exposes a different executable location to the application. Benchmark
both, then select and document the contract.

Keep macOS debug `.dSYM` bundles together with executables and materialize
them consistently. Preserve inherited stdin/stdout/stderr, terminal access,
working directory, arguments and current child-exit handling. The current
driver maps a nonzero child exit through `panic_raw`; changing that policy
is a separate decision, not incidental cache work.

Recover from cache I/O failure with the ordinary uncached build. Do not
mask actual dependency/compiler/link errors or run an older successful
artifact after a failed edit. Cache lookup before single-file dispatch's
existing eager lexing is necessary if file-mode hits are to skip parsing.

## 5. CLI, lifecycle and documentation

Implement `run` reuse first through the existing compilation path. Keep
`check` fresh and make `emit-llvm` emit actual IR. Extending reuse to
`build`/`test` can follow with output/dispatcher-specific acceptance tests.

Provide a documented bypass, preferably `PLUM_NO_BUILD_CACHE=1`, without
stealing application arguments. Everything after the run project is
currently forwarded, and release/trace/link flags are also inspected
there. Preserve that behavior in this change; do not quietly add a new
option parser or change `--target` handling. Add optional cache diagnostics
on stderr via an environment setting.

Extend cache inspection/cleaning with explicit build/package scopes;
preserve existing package behavior and counts. Make build entries,
sizes, locations and removal visible without assuming global project
reachability. Implement a bounded build-cache retention policy with
reader-aware pruning, or explicitly require manual cleaning and document
that choice before release. Respect `PLUM_CACHE` everywhere and isolate
all tests from the user's real cache. Missing/unusable cache configuration
must not prevent dependency-free uncached execution.

Update RUNNING.md, TOOLING.md, MAINTENANCE.md, help checks and release
notes. Regenerate/check embedded shims, stdlib docs and seed artifacts
when applicable, using the repository's established workflows.

## 6. Optimize cold compilation based on evidence

Profile after phase timing identifies costs. Candidates to investigate:
reachable-worklist slicing/concatenation and linear membership checks,
repeated linear function lookup, repeated type closure walks, metadata
generation/source rereads, and unconditional runtime/helper emission.
These are hypotheses, not measured bottlenecks.

Any runtime pruning must follow a complete symbol closure covering calls,
function addresses, globals, closure/spawn paths, allocation/release,
channel copy helpers, FFI and panic/trace support. External declarations
and symbol-collision diagnostics must keep their semantics. Keep checking
all source: unreachable ill-typed code must not become silently accepted.

Preserve global initialization order and effects. Do not delete globals
because no function reads them. Do not remove RC/reuse/release helpers
based only on obvious direct calls. Prove omissions with execution,
allocation, sanitizer and IR-level coverage. Keep measured optimizations
in separate changes so their performance and semantic effects are reviewable.

## 7. Per-module compilation design gate

An object cache keyed by complete emitted IR can save clang work but
does not skip whole-program parsing/checking/emission. Call it that.
Separate reusable shim objects/TLS bundles may be worthwhile first,
provided they use the validated native-input contract above.

True incremental compilation needs stable module interfaces and serialized
typed artifacts; invalidation rules for signatures, layouts, exports,
generic bodies and compile-time embeds; SCC handling for dependency cycles;
and a defined owner for each cross-module generic specialization and helper.
It also needs stable symbols, closure/runtime ABI, global initialization
ordering, debug metadata, and linkage/coalescing policies across platforms.

Current instantiation register numbering is threaded through the whole
program and globals use shared emission machinery. Module output must
be independent of unrelated modules before objects can be reused safely.
Determine whether a shared definition's edit invalidates consumers even
when its exported signature remains identical.

Accept this stage only with clean/incremental equivalence tests and proof
that a local implementation edit rebuilds the intended minimal closure.
Do not promise that every edit rebuilds exactly one module.

## Validation and completion gates

Add an isolated `bootstrap/build-cache-check` with a counting C-driver
wrapper and phase observations. A fast wall-clock run alone is not proof
of a hit. Assert unchanged runs avoid the expensive stages while changing
every relevant input forces the appropriate stage to rebuild.

Cover source content changes with unchanged size/mtime; add/delete/rename;
hidden discovery versus explicit hidden files; single-file/project and
bare-run forms; transitive path/Git/vendor deps, cycles, manifest absence,
missing/tampered pinned packages; embeds inside/outside the tree; native
sources, included/system headers and include shadowing; library replacement
and search shadowing; compiler replacement at the same path/version;
toolchain/SDK changes; modes; runtime arguments/environment; corruption,
failed build, full/unwritable/noexec cache, concurrent writers/readers/clean,
interruption, spaces/Unicode paths, and symlink behavior.

Run TTY/stdin tests on misses and hits, and compare output/exit behavior
to uncached execution. Verify `.dSYM` on real macOS and executable copy/
publication/cleanup on real Windows. Run on the supported Linux x86_64
and arm64, macOS and Windows CI paths. Unsupported toolchain fallback
must itself be tested, not merely documented.

Run the complete MAINTENANCE.md pre-commit suite for implementation,
including fixed-point bootstrap and seed checks. Add cache coverage to
the appropriate CI jobs and the maintenance harness registry. Keep
performance regression checks separate from correctness and use measured
platform tolerances rather than noisy universal timing thresholds.

Provisional performance objectives to ratify after the controlled baseline:
unchanged representative TUI run at least 10x faster, ideally under 200 ms
before application work; bounded miss overhead, initially targeting <=10%;
and a recorded improvement in one-line edit-to-readiness from measured
codegen/native-stage work. Report actual cold/warm/edit results and RSS;
do not close the edit-loop portion on cache-hit timings alone.

Suggested reviewable delivery sequence: baseline/profiling; shared input
discovery/embed tracking; toolchain dependency prototype and contract;
artifact store plus run integration/lifecycle/tests; individually measured
cold-build optimizations; then the incremental-compilation design proposal.
Each phase has correctness gates; none relies on cached stale output to
make a benchmark pass.
