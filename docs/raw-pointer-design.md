# Raw pointers and volatile scalar access

This is the scalar pointer contract for [#512](https://github.com/bradcypert/plum/issues/512).

Plum-OS already captures borrowed boot data in its C adapter for later memory management. A first compiler step is to let Plum read and update individual scalars in storage supplied by C. A small application can use the same operations for native buffers it owns outside the Plum heap. The boot adapter can continue to expose scalar views while it owns the boot protocol and the layout of its records. [Plum-OS adapter](https://github.com/bradcypert/plum-os/blob/cfe75db1969410efac04eda887256022a4d719a7/boot/adapter.c)

## Interface

`RawPtr[T]` is an unmanaged, nullable data pointer. The scalar operations below use a concrete Plum integer pointee. `I64` and `Int` identify the same pointee type. The pointee uses the corresponding C integer's width, alignment, and target byte order. Conformance targets are hosted x86-64 Linux and the existing `x86_64-unknown-none` profile. For pointers to native records, see [native aggregate layout](native-aggregate-layout.md).

| Operation | Result | Requires `unsafe` |
| --- | --- | --- |
| `RawPtr.from_address[T](address: U64)` | `RawPtr[T]` | Yes |
| `p.to_address()` | `U64` | Yes |
| `p.is_null()` | `Bool` | No |
| `p.offset_bytes(delta: Int)` | `RawPtr[T]` | Yes |
| `p.load()` or `p.load_volatile()` | `T` | Yes |
| `p.store(value: T)` or `p.store_volatile(value: T)` | `Unit` | Yes |

`from_address` takes exactly one explicit type argument so every access has a known width. The parser already represents explicit type arguments in call syntax. The checker can recognize this one constructor the way it recognizes `channel[T]()`. The operations that require `unsafe` are direct calls. They cannot be captured as function values or partially applied.

Direct `extern "C"` arguments and returns of `RawPtr[T]` use the C data pointer ABI. For example, a C function returning `uint32_t *` can be declared as returning `RawPtr[U32]`. Native calls still require `unsafe`. An extern declaration must match the native signature and the caller must respect native access permissions, including a read-only pointee. Passing a pointer preserves its bits, including null. The existing fixed-width integer ABI applies to separate scalar arguments and results. [Scalar integer C ABI](integer-scalar-ffi.md)

```plum fragment
extern "C" {
    fn sample_words() -> RawPtr[U32];
}

let second_word (): Option[U32] = unsafe {
    let words = sample_words();
    if words.is_null() { None }
    else { Some(words.offset_bytes(4).load()) }
}

let read_sample (address: U64): U32 = unsafe {
    RawPtr.from_address[U32](address).load_volatile()
}
```

In `second_word`, the C provider promises two initialized, contiguous `uint32_t` values, valid through the read. In `read_sample`, the caller supplies the address of a readable, naturally aligned 32-bit scalar whose accesses must be volatile. The conversion interprets the bit pattern as a virtual address for the current execution environment. An address taken from a physical memory map requires translation by its owner before use.

## Ownership and access

A pointer can be copied, placed in ordinary Plum values, returned from a function, or passed through an ordinary generic value parameter. These operations carry the address only. They perform no pointee allocation, retain, release, or copy. Keeping a pointer in a record or closure does not keep its storage alive. Calling an unsafe operation requires the caller to establish lifetime, permission, alignment, and the initialized extent of each read. Concurrent access needs synchronization supplied by the caller or native owner.

`from_address(0)` produces null on the initial targets. `.is_null()` tests address zero. Null, dangling, misaligned, out-of-bounds, or read-only storage used for a write violates the access contract. Access has no runtime validity check or recovery guarantee. `offset_bytes` applies a signed byte displacement. The caller must keep the computed address within the same valid native range or at its one-past boundary and must avoid arithmetic wrap. A one-past pointer has no readable or writable bytes. Accessors require natural alignment, including for volatile access.

The pointer type is invariant. The first interface exposes `.is_null()` as its pointer predicate. The checker rejects comparisons, structural hashing, implicit pointer conversions, and aggregate or callback extern signatures that contain `RawPtr[T]`. CStr retains immutable Plum bytes, RawCStr keeps its existing copy operations, and NativeBuffer access continues to require a lease. Integer conversion grants no lifetime for managed Plum heap storage or native buffer leases. [Native buffer contract](native-buffers.md)

## Ordinary and volatile operations

An ordinary load or store follows the optimizer's normal rules. A volatile load or store produces one LLVM volatile memory operation at the declared width and natural alignment. Preserve the number and order of volatile operations relative to other volatile operations. That contract concerns compiler access to the location. Atomicity, ordering of ordinary memory operations around an access, hardware fences, and synchronization between threads or interrupt handlers each require their own contract. The caller supplies any ordering required by the platform or device.

## Compiler fit

Reserve unqualified `RawPtr` as a built-in type and constructor namespace. A qualified user type ending in `RawPtr` remains nominal. Keep the pointee kind in typed IR and specialization identities, even though the LLVM value is an unmanaged `ptr`. Follow the existing `RawCStr` ownership treatment for the pointer itself. Audit aggregate release, closure capture, thread copying, and generic substitution so none of those paths treat the pointee as a Plum cell. Preserve `RawPtr[I64]` and `RawPtr[Int]` identity. Resolve the new type for both extern signature checking and declaration emission, so a call and its declaration use the same representation. Gate target support during emission, where the effective target is available.

Use LLVM `inttoptr` and `ptrtoint` with `i64` only for the explicit address conversions. Lower byte offsets as `getelementptr i8` without `inbounds` or no-wrap flags. At a load or store, access `i8`, `i16`, `i32`, or `i64` according to the pointee. Extend narrow loaded integers by their signedness to Plum's internal word representation and truncate the already type-checked input on stores. Emit neither stronger alignment assumptions nor inferred alias promises. These operations require LLVM instructions only. [LLVM volatile memory access](https://llvm.org/docs/LangRef.html#volatile-memory-accesses)

## Conformance

- Exercise direct C pointer parameters and returns with static C arrays in hosted and freestanding debug and release builds. Check null, a valid pointer-address round trip, positive and negative byte offsets, signed minima, high-half U64 values, and adjacent sentinels after a narrow store. Keep the test backing storage alive throughout each access.
- Compare emitted pointer signatures, access widths and alignments with Clang. Check that repeated volatile operations survive release optimization without an atomic operation or fence. Inspect freestanding objects for unexpected runtime or native library dependencies.
- Reject unsupported pointees and targets, mixed pointer types, attempts to operate outside `unsafe`, first-class use of unsafe operations, and callback or aggregate extern signatures. Cover source diagnostics for bad annotations and incorrect `from_address` type arguments.
- Pass pointers through a Plum record, closure, and generic function. Instrument native storage and Plum hook allocations to show that ordinary value lifetimes do not retain or free the pointee. Exercise existing CStr, RawCStr, NativeBuffer, and checked integer fixtures with a source-built compiler.

Run the native and core pointer checks with `python3 bootstrap/raw-pointer-check.py ./sh.real` on x86-64 Linux.

C-callable Plum exports extend the pointer contract with an inbound ABI and runtime lifecycle. [C-callable exports](c-callable-exports.md)
