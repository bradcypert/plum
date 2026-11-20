# Native aggregate layout proposal

The first layout feature under [#512](https://github.com/bradcypert/plum/issues/512) would let Plum name a C record layout and project fields from native storage. A native library can return a typed pointer to a record or an array of records. Plum can then use declared fields and compiler-computed offsets instead of duplicating byte offsets at each use.

## Declaration and storage

`extern "C" struct` declares a nominal layout with C field order and natural alignment on the initial targets. Those targets are hosted x86-64 Linux and `x86_64-unknown-none`, matching scalar [RawPtr access](raw-pointer-design.md). Each field is a concrete integer kind or another admitted native record. `Int` and `I64` both describe an `int64_t` field. Nesting puts a complete record inline, including its tail padding. Fields use target byte order.

```plum
extern "C" struct Reading {
    code: U16,
    source: U8,
}

extern "C" struct SampleFrame {
    tag: U8,
    sequence: U64,
    reading: Reading,
    count: U32,
    flags: U8,
}

extern "C" {
    fn sample_frames() -> RawPtr[SampleFrame];
}

let second_code (): Option[U16] = unsafe {
    let frames = sample_frames();
    if frames.is_null() { None }
    else {
        let second = frames.offset_bytes(NativeLayout.size_of[SampleFrame]());
        Some(second.field_ptr("reading").field_ptr("code").load_volatile())
    }
}
```

The matching C declarations use fixed-width integers.

```c
#include <stdint.h>

struct Reading {
    uint16_t code;
    uint8_t source;
};

struct SampleFrame {
    uint8_t tag;
    uint64_t sequence;
    struct Reading reading;
    uint32_t count;
    uint8_t flags;
};

struct SampleFrame *sample_frames(void);
```

The C provider promises that a non-null result points to at least two contiguous, aligned `SampleFrame` records valid through the read. C or the caller supplies the storage. As with scalar `RawPtr`, pointer copies carry only the address, including when stored in ordinary Plum values or captured by closures. Native projection and scalar access require no Plum allocation or ownership traffic. Ordinary `struct` declarations keep their existing managed representation.

The compiler rejects empty records, duplicate fields, generic declarations, and cycles through inline fields. The first field set is integers and nested native records. Pointers as fields, fixed arrays, flexible tails, unions, bit-fields, packed layouts, and explicit alignment overrides need separate contracts. `Bool` keeps its existing function ABI. It has no implicit mapping to a C record field. Native records appear as pointees and inline fields of other native records. Using one directly in a Plum struct literal, managed record field, or by-value parameter or result is a type error.

## Sizes, offsets, and access

The declaration opts into the natural C layout. Each field starts at the next multiple of its alignment. The record alignment is the greatest field alignment, and the final size rounds up to that alignment. All offsets, alignment, and size must fit in a Plum `Int`. For the initial targets, each admitted integer has an alignment equal to its byte width.

| Layout | Field byte offsets | Size | Alignment |
| --- | --- | ---: | ---: |
| `Reading` | `code` 0, `source` 2 | 4 | 2 |
| `SampleFrame` | `tag` 0, `sequence` 8, `reading` 16, `count` 20, `flags` 24 | 32 | 8 |

| Operation | Result | Requires `unsafe` |
| --- | --- | --- |
| `NativeLayout.size_of[R]()` | Size in bytes, including tail padding, as `Int` | No |
| `NativeLayout.align_of[R]()` | Alignment in bytes as `Int` | No |
| `NativeLayout.offset_of[R]("field")` | Field offset in bytes as `Int` | No |
| `p.field_ptr("field")` with `p: RawPtr[R]` | `RawPtr[F]` for declared field type `F` | Yes |

`R` is a concrete native record type. Each field argument is a string literal naming an accessible field of that record. The compiler resolves it at the call site and reports unknown, inaccessible, or nonliteral field arguments. Qualifying the type and declaring public types and fields follow the existing module visibility rules. Reserve the unqualified `NativeLayout` name for these queries. Each new operation is a direct call with exact arity.

Projection requires a non-null, aligned pointer to a complete live record. Reading a projected scalar requires initialized bytes for that field. Writing requires permission to modify that field. Projection alone reads no storage. A projected pointer inherits the lifetime and permissions of its backing storage. Callers retain responsibility for bounds, synchronization, and the lifetime of any lease or owner. Record pointers support the existing address conversion, null check, and byte offset operations. Use `size_of` as the stride when walking a native array. Record pointers reject whole-record load and store, including volatile forms. Project a scalar and use its existing ordinary or volatile operation.

Direct C arguments and returns of `RawPtr[R]` use the data-pointer ABI. A function's C pointee layout must match the Plum declaration. Layout agreement is the caller's responsibility and should be checked against the actual C header, compiler target, and compilation flags. Packed declarations and differently aligned C declarations require a separate adapter. By-value ABI classification, callbacks containing native record pointers, and C-callable Plum exports remain separate designs.

## Compiler fit

- Reuse the struct field grammar after `extern "C" struct`. The parser can distinguish the `struct` token from the opening brace of the current extern function block. Preserve the distinction as a declaration kind through formatting, source tooling, name resolution and type checking.
- Resolve native record fields in a declaration table with qualified type identity, declaration order, visibility, and a cycle check. Check field kinds once and derive sizes, alignments and offsets from one record layout computation. Keep native records out of value representation and managed field layout. Extend the typed `RawPtr` pointee to distinguish an integer from a nominal native record while preserving existing integer identities and specialization symbols.
- The query call syntax already fits the parser's explicit-type-argument expression nodes. Resolve field names and admission during checking. Fold queries to integer constants at emission using the validated target layout. Reject unsupported effective targets during emission, including declarations that are unused. The first targets share one layout rule. Future target support needs evidence for record layout and scalar access ABI.
- Lower field projection to a byte `getelementptr` with the computed constant offset and without `inbounds` or no-wrap flags. Lower a projected scalar's access through the existing RawPtr LLVM path at its native width and natural alignment. Preserve the C pointer signature and the backend's non-heap treatment. The record view needs no new runtime helper.

## Conformance for implementation

- Compare every admitted scalar's size and alignment, and mixed and nested records' `sizeof`, `_Alignof`, and `offsetof`, against Clang on both targets. Include internal and tail padding and a declaration where field order differs from alignment order.
- Exercise pointer arguments and returns with a real native array. Read a nested field in the second record, write a narrow field, and check neighboring fields and padding sentinels. Verify debug and release execution and inspect optimized LLVM for one correctly aligned volatile operation per scalar access. Check freestanding objects for unexpected symbols or runtime dependencies.
- Cover module visibility, pointer type identity, unknown or nonliteral fields, cycles, unsupported field types and targets, and layout qualifiers outside the admitted grammar. Reject unsafe calls outside an unsafe block, whole-record access, by-value native record signatures, and implicit conversion from managed records.
- Keep the scalar RawPtr, scalar C ABI, and ordinary managed record suites passing. Use mock native storage and core runtime hooks for compiler conformance. External consumers own device, protocol, and kernel-specific fixtures.
