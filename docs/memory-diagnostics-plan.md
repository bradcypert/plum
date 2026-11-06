# Memory diagnostics delivery plan (#437)

Registry design and staged delivery approved on 2026-11-06.

Goal: distinguish allocation churn, retained Plum memory, peak requested heap, and OS RSS without changing Perceus ownership or reuse.

## Chosen design and alternatives

Use a compile-time opt-in address registry for Plum heap diagnostics. Keep ordinary cell layout and native pointer ABI. Sanitizer support and the registry/public snapshots shipped together in #459; #460 fixed the temporary CStr ownership issue. All six acceptance criteria are complete, and #437 is closed. Allocation attribution is tracked separately in #461.

| Approach | Benefit | Cost / limitation |
| --- | --- | --- |
| Address registry (recommended) | Records original request size without changing pointers; unknown native pointers can pass through the release boundary | Diagnostic builds need a synchronized table and metadata; every tracked cell must use the release boundary |
| Prefix before each allocation | Constant-time metadata lookup | Returned pointer is no longer the allocator base; free and usable-size operations must translate it; mixed native pointers require provenance anyway |
| Size field in every cell | Direct metadata lookup | Changes cell layouts, native interop, and ordinary-build footprint |
| Thread-local accounting | Less shared-counter contention | Still needs size provenance, foreign-thread registration, thread-exit aggregation, and a coherent snapshot protocol |
| GNU linker malloc wrapping | Convenient existing compiler measurement tool | Current probe is single-threaded and Linux/linker-specific; also mixes Plum cells with native internals |

Perceus determines ownership and reuse. These instrumentation choices measure that behavior; they do not replace it.

## Runtime boundary and coverage

- A small cell-allocation/release boundary owns tracking. Generated structural destructors, strings, closures, handles, and runtime growth paths release managed cells through it.
- Keep unrelated native buffers and task/channel infrastructure outside the Plum-cell total. Current generated code also frees native task result boxes: these must remain native frees, or pass safely through a boundary that tolerates unregistered pointers.
- Existing native shims allocate their own task/channel objects, I/O buffers, directory/file handles and TLS state. Those are excluded. Foreign code returning a Plum-owned cell must follow the documented allocator/release contract; direct libc free of a tracked cell bypasses diagnostics and is unsupported.
- Include the bytes requested for the cell header, explicit array capacity, and string terminator. Exclude allocator rounding, registry metadata, native resources and GPU memory.
- In-place reuse keeps the original request and one live allocation. Even if the allocator's rounded block can hold a larger value, do not fabricate a larger allocation request. Replacing/growing a cell counts the newly allocated cell and retires the old cell at its actual release; their overlapping lifetime can contribute to peak.
- Retaining aliases changes no allocation totals. Only final release retires a cell. Static/immortal constants were never dynamically allocated and are excluded.
- Start with exact aggregate totals. Category attribution should follow only with a complete construction/reuse retagging contract: cross-type reuse makes an allocation-time tag alone misleading.

## Coherent snapshots

The registry owns its cumulative successful allocation count/bytes, current live count/requested bytes, and peak live count/requested bytes. One mutex protects table changes and all snapshot fields. A snapshot copies all fields at one instrumentation point into thread-local native storage; the standard library reads that frozen copy before constructing public Plum values. Registry keys encode addresses to avoid becoming conservative LSan roots. Allocation and release events linearize at registry insert/remove; this describes tracked cells rather than instantaneous physical allocator residency.

Never hold the registry mutex while invoking destructors, callbacks, or allocating Plum objects. Registry storage uses native allocation and is excluded from payload totals. Failure to grow metadata aborts with a fixed diagnostic instead of silently undercounting.

Public shape: Memory.snapshot() returns an optional snapshot (None if the program was built without diagnostics). Fields are cumulative allocations/requested bytes, live allocations/requested bytes, and peak live allocations/requested bytes. Deltas compare two snapshots from the same process. Peaks are process-lifetime high-water marks and are never reset by a snapshot. Snapshot and Delta records expose these values through the Memory standard-library module.

Take the native snapshot before constructing its public Plum record; document the observer allocation that record introduces. Tests must keep equal observer state at both baseline points. Do not hide snapshot records from ordinary ownership accounting.

Legacy PLUM_RT_STATS stays cumulative. #270 makes its individual accesses race-free; its separate atomic loads do not promise a coherent multi-field snapshot while workers run. New live snapshots use the registry's own coherent event ledger, not a collection of legacy atomic loads.

## Build policy and sanitizer stage

Introduce one validated build policy for diagnostic options, threaded through generation, native compilation/linking and cache identity. A supported sanitizer option must instrument generated IR, embedded C and dependency/project C, preserve symbols/frame pointers, and reach both cached and uncached build paths. Generated function definitions need the LLVM sanitize_address attribute: a local volatile use-after-free probe compiled with only -fsanitize=address produced no ASan diagnostic, while the same IR with that attribute reported heap-use-after-free. See https://llvm.org/docs/LangRef.html#function-attributes. Reject unsupported combinations explicitly. Avoid another independent boolean at every build helper.

Use supported Clang sanitizer behavior, keeping ASan/LSan checks separate from payload statistics. A documented runner enables requested leak detection and fails if runtime support or permissions prevent it; no fallback that silently disables leaks. Exact flag/API spelling and platform support are to be verified against toolchain documentation and executable probes during this stage.

## Delivery and acceptance

1. #270 PR: atomic legacy counters, exact joined-worker regression, refreshed seed, measured compiler cost.
2. #437 first PR: validated sanitizer build policy, cache separation, documented runner; known invalid access and leak fixtures fail, clean ownership fixture passes, symbols resolve.
3. #437 second PR: opt-in cell boundary and synchronized registry, public snapshot/delta API, documented coverage. Tests cover bounded return to baseline, aliases across workers, reuse and capacity growth, monotonic peaks, internally consistent active-worker snapshots, and disabled-build behavior.

The registry decision is approved. Keep these delivery stages reviewable independently, and retain the coverage contract as implementation progresses.
