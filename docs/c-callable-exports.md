# C-callable Plum exports

Native adapters can use a C-callable Plum function to send data back into a running Plum program. The export has a stable C symbol and the target's C calling convention. The same source function can be called from Plum using the ordinary Plum calling convention. [#512](https://github.com/bradcypert/plum/issues/512)

## Declaration

`extern "C" let` defines a function and exports its unqualified name as a C symbol. The body follows ordinary Plum function rules. Module visibility follows `pub` as usual. C exports share a single symbol namespace per link unit, so two modules cannot export the same name. An export may live in either the root or a named module.

```plum fragment
extern "C" struct Sample {
    reading: I16,
    flags: U8,
}

extern "C" let sample_read (sample: RawPtr[Sample]) (delta: I8): I16 = unsafe {
    sample.field_ptr("reading").load() + I16.from(delta)
}
```

The native adapter declares the corresponding C function.

```c
#include <stdint.h>

struct Sample {
    int16_t reading;
    uint8_t flags;
};

int16_t sample_read(struct Sample *sample, int8_t delta);
```

The native call must supply a non-null, aligned pointer to a live `Sample` with an initialized `reading` and an input whose sum with `delta` fits in `I16`. The checked addition retains its ordinary Plum overflow semantics.

An export requires a plain ASCII C identifier, named and annotated parameters, and an explicit result type. `()` with no named parameters represents a C `void` parameter list. The checker rejects generic exports, name collisions with other imports or exports in the link unit, `main`, the builtin name `println`, and the reserved `plum_` prefix. Emission rejects a collision with a generated or runtime symbol. A duplicate definition in separately compiled native code remains a linker error. The symbol is available to native code linked into the same program or image.

## ABI and ownership

The first targets are hosted x86-64 Linux and the freestanding `x86_64-unknown-none` profile. Exports use the same type mapping and target attributes as direct C imports. The exported definitions carry the target's `signext` or `zeroext` attributes for narrow parameters and results. An LLVM wrapper converts at the seam and calls the compiler's existing internal Plum symbol. [Scalar C ABI](integer-scalar-ffi.md)

| Plum type | C type | Transfer |
| --- | --- | --- |
| Fixed-width integers, including `Int` and `I64` | Matching `<stdint.h>` type | Widen narrow incoming values by signedness and truncate narrow results to the declared width |
| `Bool` | `int` | Any nonzero incoming value becomes true. The result is 0 or 1 |
| `Float` | `double` | Pass by value |
| `Unit` result | `void` | Return without a value |
| `RawPtr[T]` for an admitted integer or native record | Corresponding C data pointer | Preserve the address, including null, without changing ownership |

The initial signature set above excludes managed values, resources, `CStr`, `RawCStr`, aggregates by value, function values, and variadics. An exported function's C address can be used with a compatible C function pointer under the same runtime contract. [Raw pointer contract](raw-pointer-design.md) · [Native aggregate layout](native-aggregate-layout.md)

Incoming pointer storage belongs to its native owner. The owner keeps it valid and grants the permissions used by the body. Returning or retaining a pointer in Plum carries only its address. Returning a pointer to shorter-lived storage does not extend its lifetime, including storage borrowed through a managed value or native buffer lease. C callers must match the declared signature and pointee layout, including compiler target and record layout flags.

## Runtime lifecycle

Use the existing supported executable and freestanding object builds. The hosted runtime enters through the generated C `main`. The freestanding runtime enters through `plum_kernel_start` after the native adapter establishes the machine and hook contract. Both entry paths initialize globals before enabling exports. Export wrappers call the internal body directly and leave runtime initialization to the entry point. [Freestanding entry and hooks](freestanding-x86_64.md)

The first contract admits synchronous calls on the Plum entry thread while source `main` is active. A Plum function can call a native adapter that calls an export and returns. An export can itself make a native call under the same rules. Native callers enforce the thread and context requirements, including deferring calls from signal and interrupt contexts. The entry wrapper disables exports before returning or halting.

An export called during global initialization fails using the selected runtime's nonreturning panic path, before entering the Plum body. The same readiness check rejects calls after the active region. Concurrent foreign-thread entry, a C-owned process entry point, unload, and unwinding across the seam require separate runtime contracts.

## Compiler fit

- Distinguish the `let` token after `extern "C"` from the brace of an import block and the `struct` token of a native record. Reuse function declaration parsing and Plum body checking. Track export metadata separately from the function's internal name, visibility, and inferred body.
- Validate the full signature and C symbol once when collecting exports. Reuse the scalar ABI type and attribute rules for imports and exports. Keep per-call conversion code in the C wrapper and keep the checked body on the internal convention.
- Root exported bodies in specialization discovery. Root the C wrappers in freestanding symbol selection so exports reachable only through native code survive. Emit the ready state only for builds with exports. Include the effective target in the cache key and reject unsupported targets at emission.
- Stable symbols and scalar wrappers use ordinary LLVM instructions. Consider an intrinsic when an external consumer identifies an operation with compiler-visible semantics that existing Plum operations and native helpers cannot express. Inline assembly would also need a target-specific contract for operands, clobbers, and effects. Use that decision for a concrete consumer operation with its own conformance tests.

## Conformance

- Compile matching C prototypes and compare Clang's call and definition attributes on both targets. Cover every integer width and signedness, mixed register and stack arguments, signed minima, high-half U64 results, noncanonical C Bool inputs, Float, and Unit.
- Call a Plum export from a linked native adapter in hosted debug and release builds, and from a freestanding object with mock hooks. Verify a native record pointer round trip, ordinary and volatile field access, synchronous nested calls, and a normal Plum call to the same function. Inspect object symbols for the exact C names, including an export referenced only from native code.
- Call an export from a native helper reached during global initialization. Check the panic message, lack of body effects, and single initialization. Check ordinary panic and return behavior, then verify normal entry and existing hook allocation tests.
- Reject duplicate symbols, unsupported signatures and targets, and reserved names. Check matching C layout and lifetime with compiler-owned fixtures. External users supply their own native adapters and hardware-specific checks.

Run the export checks with `python3 bootstrap/c-export-check.py ./sh.real` on x86-64 Linux.
