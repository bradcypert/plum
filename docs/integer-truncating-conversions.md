# Truncating integer conversions

Design approved on 2026-11-16 as the next part of #525, after wrapping
arithmetic shipped in #532.

## API and semantics

`Destination.truncating_from(value)` accepts any known integer source type
and returns the destination type. The conversion evaluates its argument once.
It computes `r = value mod 2^N` in the mathematical, nonnegative sense, where
N is the destination width. An unsigned destination returns r. A signed
destination returns r when r < 2^(N-1), otherwise r - 2^N.

This rule applies to narrowing, widening, equal-width signedness changes,
and identity conversions. Signed negative sources therefore sign-extend on
widening; unsigned sources zero-extend. The source width does not limit the
bits of a negative value's mathematical two's-complement extension.

| Source value and type | Destination | Result |
| --- | --- | --- |
| 511: Int | U8 | 255 |
| 255: U8 | I8 | -1 |
| -1: I8 | U16 | 65535 |
| -1: I8 | I16 | -1 |
| 255: U8 | I16 | 255 |
| -1: Int | U64 | 18446744073709551615 |
| 18446744073709551615: U64 | Int/I64 | -1 |
| 9223372036854775808: U64 | Int/I64 | -9223372036854775808 |

All eight canonical integer kinds participate. `I64` remains an alias for
`Int`. Ordinary arithmetic stays checked. `from` still requires full source
range inclusion, and `try_from` still checks the actual value and returns
structured errors. Truncation has no conversion error or conversion-storage
allocation; evaluating the source expression can still allocate or panic.

Like the existing integer conversions, this is an associated compiler
builtin with call syntax, not a new overloading mechanism. The source type
must be known; the destination does not contextualize its argument. Bare
literals default to Int, and values above Int's maximum require a U64 source
annotation. Typed closures can pass conversions through generic functions.

## Implementation and validation

The typed expression records a conversion mode: preserving, checked, or
truncating. The backend retains the canonical i64 slot for 64-bit
destinations. For narrower destinations it truncates to iN and then sign-
or zero-extends according to the destination. No libc helper is necessary.

The conversion matrix uses Python's unbounded integers as its independent
oracle for every source/destination pair and boundary values. Hosted and
freestanding builds run in debug and release modes. Additional cases cover
aliases, globals, argument evaluation and owned temporaries, generics,
closures, containers, shadowed namespaces, invalid sources, hover and
completion. Existing checked conversion and arithmetic checks protect their
behavior. Source-built compiler, property, documentation, formatting and
bootstrap checks complete validation.

Width-aware bit operations have a [separate contract](integer-bit-operations.md);
existing Int shifts retain their behavior. Scalar C FFI has its own
[ABI contract](integer-scalar-ffi.md).
