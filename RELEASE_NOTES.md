Plum is a small, statically typed, compiled language.

Three changes, and they are all the same change: the compiler stops
allocating in three more places. Nothing about how you write Plum
changes, and no program produces a different answer. The programs just
do less work.

The pitch this is serving is VISION.md's — you write code that looks
like it assumes a garbage collector exists, and the compiler makes it
behave like it doesn't.

## `Array.filter` recycles its source, like `Array.map` already did

```plum
let evens = Array.filter(xs, |x| x % 2 == 0)   // xs not used again
```

When the array you filter is finished with, the result is built into
its memory instead of a new allocation. `map` has worked this way since before
0.0.8; `filter` sitting next to it and not doing so was a gap people
would trip on.

A filter per iteration over 500 numbers: **1009 allocations to 9.**

## Both of them now work on arrays of strings and structs

Before this release, recycling applied only to arrays of numbers. That
is the restriction that mattered, because the arrays real programs map
and filter hold strings and structs — and it meant the optimisation
almost never fired in practice.

The loop now releases each element as it passes, so:

```plum
let long = Array.filter(names, |s| s.len() > 2)   // names not used again
```

A filter per iteration over 200 strings: **508 allocations to 8.** The
same for `map`.

## Building a value in a loop reuses the last one's memory

```plum
for i in 0..1000 {
    let p = P { x: i, y: 2 };
    ...
}
```

Every `p` used to be a fresh allocation. The moment you store into `p`,
whatever was there is finished with — so the new value is built into
it. This works for structs, enum variants and array literals, and for
both `let` and assignment.

**1001 allocations to 2**, for each of those shapes.

Constant literals were already free — they have been hoisted to static
cells since before 0.0.8. This is the case that could not be hoisted,
because the contents are different every iteration.

## What this cost

The compiler's own allocation count went the **wrong way**: about 1,200
more out of 199,000, or 0.6%, measured compiling a fixture.

It gains nothing from any of the three changes. It is written in a
recursive style with almost no assignment inside loops, so it does not
contain the shapes being optimised — it only pays for the checks that
look for them. The array case is the larger half of the regression, and
the likely reason is that recycling an array literal means computing
every element before deciding the cell, which keeps more values alive
at once, and a value that is alive is one some other recycling
declines. That is a hypothesis and it is labelled as one in DESIGN.md;
no allocation fixture regressed, which is the check that guards this,
so it was not chased further.

It is a real cost and it is in the release notes because it is a real
cost. If your program looks more like the compiler than like the
examples above, this release is very slightly worse for you.

## Upgrading

Nothing to do. No syntax changed, no API changed, and no program
computes anything different.
