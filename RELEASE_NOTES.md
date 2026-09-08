Plum is a small, statically typed, compiled language.

Bitwise operators — the language had none — and control over how numbers
render inside string interpolation.

## Bitwise operators

```plum
a & b     a | b     a ^ b     a << n     a >> n     ~a
```

Until now `&` did not even lex. That ruled out bit flags, binary
formats, hashing, and every modern random number generator — `Rng` uses
a 1988 combined generator because PCG and xoshiro were unreachable.

Also `Int.shr_logical` (an unsigned right shift, since `>>` is
arithmetic), `Int.count_ones`, and the formatting below for reading the
results.

**Two places this deliberately differs from C.**

Bitwise binds **tighter than comparison**, so `a & b == 0` means
`(a & b) == 0`. In C it means `a & (b == 0)` — a mistake Ritchie
described as unfixable once code depended on it. Nothing depended on it
here.

Shifts bind like multiplication, as in Go, so `1 << n + 1` is
`(1 << n) + 1` rather than `1 << (n + 1)`.

**Shift counts are defined for every value.** A count of 64 or more
gives `0` for `<<` and a sign fill for `>>`, so `-1 >> 99` is `-1`. A
negative count stops the program, alongside division by zero and
integer overflow.

`|` is both the closure delimiter and bitwise-or, and they never
collide: a closure can only start where an expression is expected, and
`|` can only be an operator where one has ended. `Array.map(xs, |x| x | m)`
parses with no ambiguity.

## Formatting numbers

String interpolation already existed, so building output was never the
problem:

```plum
println("${user} ${n} items")
```

What was missing was control over how a number renders inside one.

```plum
Float.to_fixed(1.0 / 3.0, 2)     // "0.33"
Float.to_fixed(19.999, 2)        // "20.00"

Int.to_hex(255)                  // "ff"      — signed: to_hex(-255) is "-ff"
Int.to_binary(10)                // "1010"
Int.to_octal(64)                 // "100"
Int.to_radix(1295, 36)           // "zz"

Int.to_bits(5)                   // 64 binary digits, two's complement
Int.to_hex_bits(255)             // "00000000000000ff"

String.pad_center("hi", 8, ".")  // "...hi..."
```

`to_radix` is **signed and reversible** — `to_radix(-255, 16)` is
`"-ff"`, which reads back. `to_bits` is the bit-pattern view instead:
`to_binary(-1)` is `"-1"` while `to_bits(-1)` is sixty-four ones. Both
answer different questions.

**`Float.to_fixed` gives the same answer on every platform**, which
took more work than expected. `snprintf` is correctly rounded
everywhere and does not agree across platforms: glibc rounds exact ties
to even, Microsoft's CRT rounds them away from zero, so `to_fixed(2.5, 0)`
was `"2"` on Linux and `"3"` on Windows. The rounding is now applied to
the digits rather than left to the C library.

Ties round **half to even** — IEEE 754's default, and unbiased, since
rounding every tie away from zero accumulates. Note that `Float.round`
rounds half *away* from zero: that pair is not an inconsistency, it is
what C, Python, Rust and Java all do, because `round` is arithmetic and
formatting is rendering.

## Also

`examples/asteroids` no longer declares `sin` and `cos` in its own
`extern "C"` block beside a hand-typed `3.14159265358979`; it uses the
`Float` trigonometry added in 0.0.24.

Internally, about 275 chains of `.concat(...)` in the compiler became
interpolation. Verified by diffing the emitted LLVM IR for every
execution fixture before and after — 93 of 93 byte-identical — so the
change is provably invisible.
