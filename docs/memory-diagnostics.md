# Sanitizer builds and memory measurements

CStr shares an existing String/Bytes cell and owns a reference to it. Creating
or transferring a CStr allocates no wrapper or byte copy. Its backing cell
remains live until the last owner is dropped, and an outstanding CStr can
prevent an alias from being updated in place. That retention and any resulting
copy-on-write allocation are real costs, visible in live/peak cells and bytes.
Dropping the last owner releases the cell through the normal allocator path.
The compiler's existing conservative loop analysis can retain a local until
overwrite or function cleanup; repeated conversions must remain bounded, and
completed function calls must restore the caller's live-byte baseline.
NativeBuffer.snapshot provides a separate optional ledger for NativeBuffer
payload requests, live/peak resource counts and requested capacity. Its handle
and lease cells remain in Memory.snapshot; payload, native metadata and allocator
rounding do not. NativeBuffer counts only storage owned by that helper, not
unrelated libraries/GPU allocations. Both snapshots are coherent individually,
but two calls are not an atomic combined measurement. See the
[native buffer contract and reuse example](native-buffers.md).

RawCStr carries no owner and its native storage is excluded from these counts.
An outstanding raw pointer cannot prevent release or reuse: copy while its
native owner is valid. See [the FFI contract](../LANGUAGE.md#ffi).

Use a source-built compiler containing the sanitizer build mode. Native Linux
and macOS are supported with a Clang toolchain and its sanitizer runtimes;
Windows and cross-target sanitizer builds are explicitly rejected. On macOS,
use an upstream LLVM toolchain when the system Clang runtime lacks leak detection.
Put `llvm-symbolizer` on PATH, or set `ASAN_SYMBOLIZER_PATH` to its full path.
The runner verifies leak detection before running your project.

```sh
./bootstrap/sanitize-run --plum /path/to/plum /path/to/project -- application-arguments
./bootstrap/sanitize-run --plum /path/to/plum --release /path/to/project
```

`sanitize-run` builds with `--sanitize address`, then executes with leak detection,
symbolization and nonzero failure status enabled. It first compiles and runs a
known leak fixture with the same toolchain/settings. If that fixture does not
produce a failing LeakSanitizer report, the runner fails. Build failures,
sanitizer setup failures and permission restrictions are failures; it never
retries with leak detection disabled. Ambient `detect_leaks=0` or `exitcode=0`
settings cannot silently turn a finding into a passing run. Project arguments
follow the project; an optional first `--` is consumed by the runner.

For direct control, the same build policy is available to all compilation paths:

```sh
plum build project --sanitize address -o /tmp/project-asan
ASAN_OPTIONS=detect_leaks=1 /tmp/project-asan
plum run project --sanitize=address -- application-arguments
ASAN_OPTIONS=detect_leaks=1 plum test project --sanitize address
plum emit-llvm project --sanitize address > /tmp/project.ll
```

The direct commands obey sanitizer runtime settings. Use `sanitize-run` for the
verified ASan/LSan workflow. `--release` retains `-O2` while adding symbols in
sanitizer mode. Optimization and tail-call elimination remain enabled, so stacks
can omit inlined/tail-called frames. `--trace` remains available separately.
For `run`, compiler options precede `--`; tokens after it belong to the program.

## Instrumentation coverage

Generated source functions, runtime helpers, synthesized destructors and entry
functions carry LLVM `sanitize_address` and frame-pointer attributes. Merely
passing `-fsanitize=address` when linking LLVM IR does not enable address checks
on arbitrary generated function loads. Native compilation and linking receive
the sanitizer flags too, including embedded C/TLS sources and project/dependency
C sources. Prebuilt external libraries must be rebuilt separately for equivalent
instrumentation; this is not a whole-process instrumentation guarantee.

IR cache keys include the complete build policy; native object keys include
the sanitizer compiler flags and preprocessed C inputs. Ordinary and sanitized
artifacts remain distinct. Cached and uncached builds, test dispatchers, and
single-file/project IR emission use the same policy. macOS symbol extraction
runs while its object files still exist.

`bootstrap/sanitizer-check.py /path/to/plum` exercises generated Plum and native
C invalid accesses, intentional leaks, clean shared ownership, debug/release
source symbols, instrumentation cache transitions and runner preflight failures.
It is a diagnostic suite: its intentionally invalid programs must fail with the
specified sanitizer diagnostics. They are outside the ordinary execution corpus.

## The larger memory story

ASan detects invalid accesses and LSan detects unreachable allocations at exit;
a clean workload is evidence about that workload, not proof of all ownership
behavior. Live reachable memory may be intentional or excessive without being
an LSan leak. Sanitizers also change allocator behavior, timing and RSS, so do
not compare their footprint directly with ordinary-build performance baselines.

`PLUM_RT_STATS` reports cumulative allocation traffic, including freed cells.
Use `--memory-stats` for live/peak requested Plum-cell bytes and coherent snapshots:

```sh
plum build project --memory-stats -o /tmp/project-memory
plum run project --memory-stats
plum build project --memory-stats --sanitize address -o /tmp/project-both
```

When linking `emit-llvm --memory-stats` output manually, compile the embedded
native memory shim with `-DPLUM_HEAP_STATS`; build/run/test supply that flag
automatically. A mismatched native shim fails to link rather than silently
disabling requested tracking. Combined sanitizer output also needs the matching
Clang sanitizer compile/link flags.

```plum
use Memory;
let main (): Unit = match Memory.snapshot() {
    None => println("build with --memory-stats to enable snapshots"),
    Some(s) => println("live requested bytes: ${s.live_bytes}; peak: ${s.peak_bytes}"),
}
```

`Memory.snapshot()` returns `Option[Memory.Snapshot]`. None means tracking was
not compiled in; zero is a valid measured value and is not the disabled signal.
Fields are cumulative `allocations`/`allocated_bytes`, current
`live_allocations`/`live_bytes`, and process-lifetime
`peak_allocations`/`peak_bytes`. `Memory.delta(before, after)` subtracts traffic
and live values from two snapshots in the same process. Live deltas can be
negative; peak counters are independent maxima and are never reset by reads.

Snapshots freeze all fields under the registry mutex, then release the mutex
before constructing public Plum records. The snapshot record and Some wrapper
are ordinary owned cells allocated after capture. Retaining a previous snapshot
changes later live totals. To check a bounded workload, keep observer state equal;
for example, a helper that returns just `s.live_bytes` drops its snapshot before
the caller starts the workload. Snapshot traffic itself increases cumulative totals.

## What the heap numbers include

- Dynamically allocated Plum cells: strings, arrays, structs/enums, closures,
  refs and resource-handle cells. Include requested headers, string terminators
  and explicit retained array capacity, even when logical length is smaller.
- Track one cell until its final storage release. Aliases add references, not
  allocations. Perceus in-place reuse keeps the original requested size even
  when allocator rounding allows a larger logical payload. Growing by allocating
  a replacement counts its actual overlap with the old cell toward peak.
- Exclude allocator rounding/metadata, immortal constants, native scratch buffers,
  task/channel infrastructure, file/network resources, external libraries and GPU
  memory. A handle's small Plum cell is included; its native resource is not.
- Native producers of managed cells must use the cell boundary declared in
  `memory_shim.h`. Release owned children before final `plum_free`; direct libc
  free of a tracked cell bypasses accounting and is unsupported. Untracked native
  malloc pointers can pass through `plum_free` without being charged as Plum cells.

Allocation and free events linearize at registry insertion/removal. Snapshots
therefore describe tracked cells at one instrumentation point, rather than an
instantaneous allocator/RSS measurement. Legacy cumulative atomics have their own
individual observation points and do not promise a coherent multi-field snapshot.

## Diagnostic overhead and limits

Ordinary builds allocate directly through malloc and inline the release boundary;
they contain no tracking table, mutex or thread-local snapshot metadata. Diagnostic
builds acquire one shared mutex per allocation, free and snapshot. The address
registry uses roughly 16 bytes per slot on supported 64-bit targets, grows with
maximum concurrent live cells, and retains that capacity after a burst. Tombstone
compaction avoids unbounded growth from bounded allocation/free churn. Frozen
snapshots add a small fixed thread-local copy. Registry storage is excluded from
reported payload bytes and can still contribute to RSS after live payload drops.
Metadata/counter failures abort with a diagnostic rather than silently undercount.

Registry keys encode addresses so the diagnostic index does not become an LSan
root that hides otherwise unreachable tracked cells. Combined tracking/ASan tests
include an intentionally leaked Plum cell to verify this boundary. This encoding
is diagnostic bookkeeping, not ownership or a replacement allocator. No cell
layout, pointer offset or allocator-usable-size contract changes.

Useful categories/source attribution are deferred until construction and cross-type
reuse have a complete retagging contract; aggregate totals do not guess a cell's
current type from its original allocation. Cumulative traffic, live requested bytes,
peak requested bytes and OS RSS answer different questions. A nonzero live snapshot
alone is not a leak verdict. See the [delivery plan](memory-diagnostics-plan.md).
Perceus ownership and in-place reuse remain the production memory model.

References: [Clang ASan](https://clang.llvm.org/docs/AddressSanitizer.html),
[Clang LSan](https://clang.llvm.org/docs/LeakSanitizer.html), and
[LLVM function attributes](https://llvm.org/docs/LangRef.html#function-attributes).
