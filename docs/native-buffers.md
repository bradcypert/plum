# Native buffers: ownership and mutation

Design for #439 and #268. CStr owns immutable Plum input storage. RawCStr
carries no owner. Neither grants permission to mutate a shared String/Bytes.

## Design choice

Use an owned native Buffer handle plus an exclusive, scoped lease for access.
The handle's existing Perceus-managed cell runs a native finalizer. Each buffer
owns its own allocation; there is no process-global or thread-local scratch
storage. Aliases share the mutable native resource, not immutable Plum bytes.

| Approach | Benefit | Limitation |
| --- | --- | --- |
| Owned buffer plus scoped lease | Reusable across frames/calls; stable storage during calls; deterministic handle cleanup | Native metadata/mutex and a temporary Plum lease handle |
| Scoped allocation only | Simple per-call lifetime | Cannot reuse storage across UI frames; repeated allocation; still needs alias/reentrancy policy |
| Direct writes to String/Bytes | Avoids copy-in | Violates immutable aliases and Perceus uniqueness unless the language gains a checked mutable-borrow system |
| Thread-local scratch with exit destructors | Reuses one buffer per worker | Hidden lifetime and reentrancy constraints; does not provide a caller-owned UI buffer |

A lease retains the native resource independently of the public handle and
excludes copy, resize and other leases through aliases. Metadata reads remain
available. Conflicting operations return Err rather than blocking inside a
callback. The native mutex is never held while invoking user code. Distinct
buffers can be used independently or reentrantly.

## Interface and invariants

`use NativeBuffer;` exposes `Buffer`, creation, length/capacity queries, reserve,
copy-in/copy-out, checked text decoding, `with_mut`, `with_text` and diagnostic
snapshots. Capacity is the number of writable native bytes; length is the
committed readable prefix, from zero through capacity. Reserve only grows and
preserves contents. New capacity is zero-initialized. Copy-in that does not fit
fails without truncation. Discard resets logical length; it is not secure erasure.

- Binary `with_mut` passes `(RawCStr, capacity)` to an operation returning
  `Result[Int, String]`, where the Int is the initialized prefix length. A
  successful length must be within capacity. It returns the committed count.
- Text `with_text[T]` passes the same arguments to an operation returning
  `Result[T, String]`. Capacity includes the NUL terminator slot. The input
  must already have a NUL within initialized storage; after success, a bounded
  scan within capacity finds the new length. Copy and validate UTF-8 before
  ending the lease, returning `(callback result, independent String)`. Missing
  termination or invalid UTF-8 is an error.
- A write lease ending without a valid commit discards the contents (length zero).
  Failure to acquire a lease leaves existing contents unchanged.
  Mutation cannot be rolled back merely because an operation returns Err.
- Both callbacks require a direct, fully applied unsafe call. Raw pointers
  must not escape the lease or be retained by foreign code. Unsafe operations
  must initialize their reported prefix and respect the supplied capacity.
- Copy-out holds a read lease through the copy. It returns independently owned
  Bytes; text decoding validates UTF-8 and returns Result[String, String].
- Cleanup follows existing handle semantics: ordinary exits release leases and
  owners; panic/process exit runs registered native finalizers. Panic does not
  add general Plum-stack unwinding. Ordinary-type drop/Plum on_drop redesigns
  are unnecessary for this native resource.

## Network integration

Replace TCP/TLS receive scratch pointers with `recv_into(handle, pointer,
capacity)` returning a count. Net.read_into supports caller buffer reuse;
Net.read_bytes and HTTP's TLS wrapper use a private owned buffer and copy out
before releasing it. Thus short-lived workers leave no receive allocation
behind. Reads require positive capacity, keeping a zero-size request distinct
from peer EOF. The native receive ABI changes; ordinary Plum read signatures
remain available. A private-buffer read allocates native storage per call;
callers needing amortized reuse can retain a Buffer and use Net.read_into.

## Accounting and validation

Memory.snapshot continues to count Plum cells, including Buffer/lease handle
cells, and excludes their native allocations. NativeBuffer.snapshot is enabled
by --memory-stats and separately reports native-buffer requests, live/peak
buffer counts and live/peak requested payload capacity. It excludes native
metadata, allocator rounding, unrelated libraries and GPU allocations. Resize
traffic records each successful requested capacity; capacity peaks describe
committed resource capacities, not allocator-internal realloc overlap or RSS.
Snapshots are coherent individually; NativeBuffer.snapshot and Memory.snapshot
are separate observations, not an atomic combined total. Ordinary builds have
no diagnostic counters/lock.

Tests must cover exact/empty capacities, binary NUL and UTF-8, immutable input
aliases, copy independence, resize/error paths, busy/reentrant aliases, normal
and panic cleanup, threaded TCP/TLS receives, native capacity returning to its
baseline, and ASan/LSan. Diagnostic overhead and the per-read allocation tradeoff
belong in the PR alongside compiler memory measurements.

File-read scratch has the same ownership problem and is tracked in #462 for
migration onto this interface. Stdin reader-state synchronization remains #271.

## A reusable text-widget adapter

Reuse a Buffer sequentially across widgets or frames. Use distinct buffers for
simultaneous or nested edits; aliases share one mutable resource. `capacity` is
passed to the native widget unchanged and includes NUL space. Copy-in refuses
oversized input; errors can leave the original immutable String in application
state. The returned String is copied and validated before releasing the lease,
so a later edit through an alias cannot change this result.

```plum fragment
use NativeBuffer;
extern "C" { fn widget_edit(text: RawCStr, capacity: Int) -> Int; }

let edit_label (buffer: NativeBuffer.Buffer) (initial: String): Result[(Int, String), String] =
    match NativeBuffer.copy_in(buffer, initial.as_bytes()) {
        Err(e) => Err(e),
        Ok(_) => unsafe { NativeBuffer.with_text(buffer, |pointer, capacity| {
            Ok(unsafe { widget_edit(pointer, capacity) })
        }) },
    }
```

Keep the Buffer to reuse its capacity across frames. Use the callback result for
widget status/changed flags and the returned String for immutable application
state. The native widget must obey its supplied capacity and must not retain
the pointer. Input encoding must meet that widget's own requirements.
