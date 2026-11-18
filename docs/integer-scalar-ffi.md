# Fixed-width scalar C FFI

Scalar `extern "C"` parameters and returns use these C types:

| Plum | C (`<stdint.h>`) | LLVM boundary type |
| --- | --- | --- |
| I8 | int8_t | i8 |
| I16 | int16_t | i16 |
| I32 | int32_t | i32 |
| Int / I64 | int64_t | i64 |
| U8 | uint8_t | i8 |
| U16 | uint16_t | i16 |
| U32 | uint32_t | i32 |
| U64 | uint64_t | i64 |

```plum fragment
extern "C" {
    fn device_byte(port: U16) -> U8;
    fn native_counter() -> U64;
}

let read (port: U16): U8 = unsafe { device_byte(port) }
```

The C declaration must match the Plum declaration. In particular, `Int` is
always 64 bits; it is not C `int` or a portable spelling of C `long`. `Bool`
continues to cross as C `int` (i32), with any nonzero return interpreted as
true. `Float` remains C `double`. An omitted return or `Unit` return is C
`void`. The existing CStr and RawCStr ownership/lifetime contracts are unchanged.

## Boundary lowering

Plum keeps its word-sized integer representation internally. Arguments are
truncated to the declared C width. Returns are extended to a word with the
declared signedness; 64-bit values keep every bit, including U64 values above
Int's maximum. This is a representation change at a typed boundary, not an
implicit numeric conversion between Plum types.

LLVM's `signext`/`zeroext` attributes are distinct from those conversion
instructions. They appear on both declarations and calls according to the
selected target's default C ABI:

| Target | I8 / I16 | U8 / U16 | 32- and 64-bit integers |
| --- | --- | --- | --- |
| x86-64 Linux / macOS / unknown-none | signext | zeroext | no extension attribute |
| Apple arm64 | signext | zeroext | no extension attribute |
| Linux arm64 / Windows x64 / Windows arm64 | no extension attribute | no extension attribute | no extension attribute |

Other architectures reject new-width scalar externs during emission until
their ABI is implemented and validated. Existing Int/I64 ABI support on those
architectures is unchanged. Custom calling conventions are outside this contract.

`build --target <triple>` and `emit-llvm --target <triple>` select the same ABI.
Without `--target`, the compiler asks the selected C driver (`PLUM_CC`, default
`clang`) for `-dumpmachine`. An explicit target permits IR emission without a
native C driver. IR cache keys include the effective target, so a Linux ABI
module cannot be reused for a Windows or arm64 build. When compiling emitted
hosted IR separately, pass the matching target to the C driver too.

## Boundaries kept closed

This support covers direct scalar calls. Extern functions still cannot be
ordinary Plum function values or partially applied. The self-hosted compiler
does not expose C-callable Plum exports or C callbacks for these types;
function signatures containing new-width integers are explicitly rejected at
the extern boundary. A `pub let` remains an ordinary Plum function with Plum's
calling convention and symbol naming.

Records, tuples, enums, arrays, references and other aggregates containing
new-width integers are also rejected at the extern boundary. Scalar ABI support
does not promise C-compatible ordinary record layout, member offsets, packing,
alignment or by-value aggregate calling conventions. Native layout, C `float`,
general raw pointers and volatile access remain separate work.

## Validation

`bootstrap/integer-ffi-check.py` compares emitted declarations and calls with
Clang's C frontend for all seven target profiles above, then compiles the LLVM
IR to objects. The native fixture checks minima/maxima, signed negatives,
unsigned high bits, widening inside C, independent return constants, mixed
register/stack arguments and existing Bool/Float/Unit/CStr/RawCStr crossings.
It runs in debug and release modes on each native CI platform. Optional core
tests link freestanding objects to hosted test hooks and check allocation
balance; they do not boot a kernel. Optional Linux arm64 tests run cross-built
programs under qemu. Cross-target IR/object checks alone do not prove execution
on macOS or Windows; native CI supplies that evidence.
