Plum is a small, statically typed, compiled language.

**This release is about memory.** A loop that rebuilds a value —
the ordinary functional shape, `Entity { x: e.x + 1, ..e }` in a
recursion — used to allocate once per iteration. It now allocates a
constant number of times, because the old cell is recycled when nothing
else can see it.

On a benchmark of 2,000,000 struct updates and 200,000 `Array.map`
passes, the same program went from **251 ms to 22 ms** — and the new
build reports **five allocations** for the whole run. The old figure is
around 2.4 million, extrapolated from the per-iteration rates rather
than measured, because the 0.0.5 compiler predates the allocation
counter this release added.

That benchmark is shaped like the thing being optimised and real
programs will see less; it is included in full below so you can judge
it for yourself.

Nothing about this is visible in the source. There are no annotations
and no new syntax: the decision is a runtime check on whether anything
else still holds the value, so it is never wrong, only sometimes
unavailable.

The compiler is written in Plum. It compiles itself to a fixed point —
the compiler it produces emits byte-for-byte identical output to the
compiler that produced it — and building it needs no toolchain beyond
`clang`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

It picks the right archive for your platform, verifies it against the
published checksum, installs `plum` into `~/.local/bin`, and runs it to
prove it works. It **does not edit your shell configuration** — if that
directory is not on your `PATH` it prints the line to add and stops.
`PLUM_PREFIX` and `PLUM_VERSION` override where and which.

Or download an archive directly:

| Platform | Archive |
|---|---|
| Linux x86_64 | `plum-0.0.6-x86_64-linux.tar.gz` |
| Linux arm64 | `plum-0.0.6-arm64-linux.tar.gz` |
| macOS Apple Silicon | `plum-0.0.6-arm64-macos.tar.gz` |
| macOS Intel | `plum-0.0.6-x86_64-macos.tar.gz` |
| Windows x86_64 | `plum-0.0.6-x86_64-windows.tar.gz` |

You need **`clang`** on your PATH; the compiler shells out to it to
assemble and link. Nothing else is required.

## What got cheaper

Five separate things, each measured by a fixture in
`bootstrap/alloc_corpus/` whose allocation count is recorded and
checked on every build.

**Rebuilding a struct or an enum** recycles the old cell. This works
whatever the value holds — a `String` field, an `Array` field, an enum
payload — not only scalars. 1002 allocations to 2 over a thousand
iterations, and an enum rebuilt from a string literal to 1.

**`Array.map`** builds its result into the source array's cell when
nothing else is holding it. A thousand chained maps allocate one array
cell rather than a thousand.

**A closure that captures nothing is no longer allocated at all.** It
is the same three words every time, so it becomes a module-level
constant. `|v| v + 1` inside a loop body was costing an allocation per
iteration.

**Closures now capture their free variables**, not the whole enclosing
scope. This is what made the previous item possible, and it makes every
closure cell smaller. It is not observable in behaviour — an unused
capture never was — and it removed 1.8% of the compiler's own emitted
code.

**Literals that cannot differ between evaluations are not allocated.**
`[1, 2, 3]`, `Point { x: 1, y: 2 }`, `None` — each becomes a
module-level constant. In practice the nullary variants matter most:
`None` and its kind appear inside loops everywhere. This one alone
removed 4.8% of the compiler's emitted code, since the compiler is
itself full of them.

`Array.push` and `String.concat` already grew in place when uniquely
held; that has not changed.

## What did not get cheaper

Stated plainly, because a performance release should say where it stops:

- **`Array.filter`.** It writes a shorter result than its source, so
  reusing the cell needs the length header patched, which is not done.
- **Mapping an array whose elements are themselves references.** The
  loop would have to release each old element after the closure has
  taken its own — a different loop body, not a different allocation.
- **Literals whose contents vary.** `[n, n + 1]` in a loop still
  allocates each time. Recycling it needs a dead cell of the right
  shape in scope, and a loop that consumes its literal immediately has
  none.

## The benchmark

```plum
struct Entity { name: String, x: Int, y: Int, hp: Int }

let step (e: Entity) (n: Int): Entity =
    if n <= 0 { e }
    else { step(Entity { name: e.name, x: e.x + 1, y: e.y, hp: e.hp }, n - 1) }

let pipeline (xs: Array[Int]) (n: Int): Array[Int] =
    if n <= 0 { xs } else { pipeline(Array.map(xs, |v| v + 1), n - 1) }

let main (): Unit = {
    let e = step(Entity { name: "hero", x: 0, y: 0, hp: 100 }, 2000000);
    let xs = pipeline([1, 2, 3, 4, 5, 6, 7, 8], 200000);
    println(e.x.to_string().concat(" ").concat(Array.fold(xs, 0, |a, v| a + v).to_string()))
}
```

Set `PLUM_RT_STATS=1` when running any compiled program to see its
allocation count on stderr.

## Correctness

Recycling a cell is only safe if nothing else can see it, and that is
checked at runtime rather than proved, so being wrong about it is
impossible — the check simply declines and the program allocates. What
had to be got right is what happens on each answer, and that is what
the new fixtures cover: a value the caller still holds, a field copied
across versus replaced, an enum changing to a variant with a different
payload, and `ref()`, which must never be shared no matter how constant
its initial value looks.

What is actually checked, rather than claimed: 73 corpus fixtures under
AddressSanitizer with leak detection, 102 lexer/parser goldens, 11
property tests, ten recorded allocation counts, every project in
`examples/` against its recorded output, and a real language-server
session — on Linux x86_64 and arm64, macOS, and Windows.

Plum is reference counted, so a leak is a miscompile. That is why the
corpus runs under leak detection on both Linux architectures, and why
this release added fixtures before it added optimisations.
