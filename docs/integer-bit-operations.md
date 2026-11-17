# Width-aware integer bit operations

This is the separate bit-operation design and implementation review requested
by #525, after wrapping arithmetic (#532) and truncating conversions (#551).
These operations support flags, binary formats, protocols, hashes and checksums.

## Values and counts

`&`, `|` and `^` require operands of the same integer type and return that
type. `~` complements exactly the operand's N bits and returns its type.
Signed results use two's-complement interpretation. Literal expressions can
take their type from context; already typed values never convert implicitly.

Both shift operators take an integer value on the left and an `Int` count
on the right. Their result retains the left operand's type. The count does
not inherit that type: a U8 value can be shifted by a normal Int loop index.
An I64 count is also an Int because those names alias the same type.

| Operation | Count below zero | Count 0 through N-1 | Count at least N |
| --- | --- | --- | --- |
| `x << count` | Panic | Shift left, retain low N bits | Zero |
| signed `x >> count` | Panic | Arithmetic right shift | Sign fill: -1 or 0 |
| unsigned `x >> count` | Panic | Logical right shift | Zero |
| `Type.shr_logical(x, count)` | Return x | Zero-fill right shift | Zero |

The logical helper also returns x for count zero, retaining a signed
negative value unchanged at that endpoint. This deliberately preserves the
existing Int helper's behavior for nonpositive counts. Operator shifts
retain the existing Int rule that negative counts panic. Oversized counts
are neither masked modulo the width nor rejected.

`Type.count_ones(x)` counts only the N-bit representation and returns an Int
between zero and N inclusive. In particular, I8.count_ones(-1) is 8, while
Int.count_ones(-1) remains 64. All existing Int/I64 behavior and precedence
remain unchanged. Ordinary arithmetic remains checked.

## Lowering and review coverage

Values keep the existing canonical sign- or zero-extended i64 storage. Narrow
complements and left shifts normalize to the destination width. Right shifts
select arithmetic or logical LLVM instructions from the value's signedness.
Counts are checked for negativity and clamped before any LLVM shift.

The named helpers use typed prelude wrappers and prelude-qualified runtime
bindings. Logical shifts mask off source sign extension; population counts
clear one low set bit per step without libc or target-specific intrinsics.
Int's existing helpers stay in place, and I64 resolves to them as before.

Validation compares against Python's unbounded integer/bit models in hosted
and freestanding debug/release builds. It includes width edges, negative
values and counts, high-half U64, contextual literals, type mixing rejection,
generics, closures, containers, globals, evaluation order, owned temporaries,
raw-name collisions and isolated function-value reachability. Existing integer
arithmetic/conversion suites, compiler self-tests, properties, bootstrap and
generated documentation checks protect prior behavior.

Scalar C FFI remains the next separate track in #526. These bit operations
make no promise about C-compatible record layout, raw pointers or storage.
