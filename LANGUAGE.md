# Language tour

## Functions, inference, recursion

```plum
// No `return`. A function's body IS its value.
//
// A top-level signature is written out in full: parameter types and a
// return type. Inference works INSIDE a function, not across its
// boundary, so a public API is pinned on purpose rather than drifting
// with whatever currently infers.
let sum (n: Int) (acc: Int): Int = if n == 0 { acc } else { sum(n - 1, acc + n) }

let main (): Unit = {
    // Local bindings and closure parameters ARE inferred; neither
    // `total` nor `v` says what it is.
    let total = Array.fold([1, 2, 3], 0, |a, v| a + v);
    println(sum(3, total).to_string())
}
```

```
12
```

A tail-recursive function runs in constant stack space. A call to the
function itself in tail position is emitted as an LLVM `musttail` call
that returns the result directly, which is a **guarantee at every
optimisation level** rather than something the optimiser may or may not
do for you. `bootstrap/check-build-modes` runs a three-million-deep tail
recursion in the default, `--release` and `--trace` builds, so this
cannot regress quietly.

**Mutual recursion counts too.** `ping` tail-calling `pong` tail-calling
`ping` runs in constant stack space as well. Any tail call that is part
of a recursion cycle returns directly. Where the two prototypes happen
to match it is also a `musttail`; where they do not, the frame is still
reused from `-Og` up.

This holds whatever the parameter types are. Until 2026-09-02 it did
not: a function's slot releases are emitted on the way out, so with a
heap-typed parameter or `let` they sat between the recursive call and
the return of its value, which stopped the recursion becoming a loop.
`count` over two `Int`s was fine while the same function over a `String`
overflowed. Those releases now happen before the call.

One visible consequence, in `--trace` builds: a recursive chain shows
**one** frame rather than one per call, because a tail call really does
replace its caller's frame. A tail call that is *not* part of a cycle —
an ordinary one-line delegation, keeps its frame and still appears in
traces.

## Tuples

```plum
let pair (): (Int, String) = (1, "a")
let swap (p: (Int, String)): (String, Int) = match p { (n, s) => (s, n) }

let main (): Unit = println(match swap(pair()) { (s, n) => s.concat(n.to_string()) })
```

```
a1
```

Tuples work anywhere a type does: nested, inside arrays, as struct
fields, returned from generic functions. The one thing they lack is
`.to_string()`; render the elements instead, as above.

## Types: `Int`, `Float`, `Bool`, `String`, `Unit`

`String` is the surface-syntax keyword for text (its type is
occasionally referred to as `Str` in compiler-internal contexts, but
`String` is what you write in source). `Int`/`Float` conversions are
explicit, never implicit: `n.to_float()` (widening, always succeeds,
though not always *exact* for very large `Int` values, since `Float`'s
53-bit mantissa can't represent every `i64` value precisely),
`x.to_int()` (truncates toward zero), `x.round_to_int()` (rounds to the
nearest integer first, same convention `Float.round()` itself uses).
Both `Float`-to-`Int` conversions are saturating, never undefined
behavior: `NaN` becomes `0`, and a value outside `Int`'s range becomes
whichever bound it overshot. `Float.to_int(x)` and `x.to_int()` are
the same call.

```plum
let z (): Float = 0.0

let main (): Unit = {
    println((3.7).to_int().to_string());
    println((0.0 - 3.7).to_int().to_string());
    println((3.5).round_to_int().to_string());
    println((z() / z()).to_int().to_string());
    println((1.0 / z()).to_int().to_string())
}
```

```
3
-3
4
0
9223372036854775807
```

**`==` works on anything; `<`, `<=`, `>` and `>=` need an ordered
type.** Equality is structural: arrays, structs, enums and their
payloads, all the way down. Ordering is defined for `Int`, `Float` and
`String` only, and comparing anything else is a compile error naming
the type. Arrays and structs could be given a lexicographic order and
deliberately have not been: nothing needs it, enums have no obvious
answer, and rejecting can be relaxed later while the reverse cannot.

## Algebraic data types and pattern matching

```plum fragment
struct Point { x: Float, y: Float }

enum Shape {
    Circle(Point, Float),
    Rectangle(Point, Point),
}

let area (s: Shape): Float = match s {
    Circle(_, r) => 3.14159 * r * r,
    Rectangle(a, b) => (b.x - a.x) * (b.y - a.y),
}
```

`match` is exhaustive; the compiler rejects a `match` missing a
variant, unless a trailing wildcard (`_ => ...`) catches the rest.
Struct/enum equality (`==`) and `.to_string()` are structural and work
recursively through nested structs/enums/arrays, generated for every
type automatically, with no `derive` needed or available.

Field access (`.radius`, `.x`, ...) needs its receiver's type to
already be known at that point in inference. An unannotated
function/closure parameter that's only ever used for field access
won't infer a struct type from that alone, so give it an explicit type
annotation (as `area` does above).

## Struct updates

There's no field mutation (`p.x = 5` isn't valid); structs are updated
functionally, by spreading the rest of an old value's fields into a new
one, with the spread always last:

```plum fragment
let p2 = Point { x: 9.0, ..p }
```

For a NESTED field, a dotted path in the field key avoids having to
hand-reconstruct every intermediate level:

```plum fragment
struct Vec2 { x: Float, y: Float }
struct Ship { position: Vec2, rotation: Float }
struct Game { ship: Ship, score: Int }

let move_ship (g: Game) (nx: Float) (ny: Float): Game =
    Game { ship.position.x: nx, ship.position.y: ny, ..g }
```

This desugars, before type inference runs, into the fully-nested
version you'd otherwise write by hand (`Game { ship: Ship { position:
Vec2 { x: nx, y: ny, ..g.ship.position }, ..g.ship }, ..g }`); paths
sharing a prefix merge into one nested literal per level. Requires the
literal to also have a `..` spread (nothing else to read the
intermediate values from), and every intermediate segment (`ship`,
`position`) must have a concrete struct type, not a still-generic type
parameter.

## Generics

```plum fragment
struct Pair[T] { first: T, second: T }

let swap[T] (p: Pair[T]): Pair[T] = Pair { first: p.second, second: p.first }
```

**A comparison through a type parameter is checked where the type is
known.** `<` needs an ordered type and `==` needs one that is not a
function, and both rules apply inside a generic just as they do outside
it:

```plum
struct Point { x: Int }

let biggest [T] (a: T) (b: T): T = if a > b { a } else { b }

let fine (): Int = biggest(3, 7)
let broken (): Point = biggest(Point { x: 1 }, Point { x: 2 })

let main (): Unit = println(fine().to_string())
```

```
error: call to biggest: T is Point, but biggest requires T to be ordered
```

The error lands on the **call**, because that is where the type is
chosen. The definition is correct code and stays legal. It is only the
attempt to use it on something unorderable that is not.

The same applies to `.to_string()`: rendering a type parameter requires
the type argument to have a text form, so `show(ref(1))` is rejected at
the call rather than accepted and then refused by the build.

Bounds may also be written down. `[T: Ord]`, `[T: Eq]` and `[T: Show]`
— and are then required of callers whether or not the body compares or
renders anything. Writing one is optional: it pins the requirement into
the signature so a body that stops needing it does not silently widen
what callers may pass.

## Associated functions: `Type.func(args)`

```plum
struct Point { x: Int, y: Int }

let Point.add (a: Point) (b: Point): Point = Point { x: a.x + b.x, y: a.y + b.y }
let Point.show (p: Point): String = p.x.to_string().concat(",").concat(p.y.to_string())

let main (): Unit = {
    let p = Point { x: 1, y: 2 };
    let q = Point { x: 10, y: 20 };
    // The same call, written both ways.
    println(Point.show(Point.add(p, q)));
    println(Point.show(p.add(q)))
}
```

```
11,22
11,22
```

`let Type.func (...) = ...` declares a real, per-type associated
function. It can be called either way: `Type.func(receiver, args)` with
the receiver as an ordinary first argument, or `receiver.func(args)`.
The two are the same call; the second is defined as the first, which is
why `xs.map(f)` works at all (see [Standard
library](MODULES.md#standard-library)).

This works for any struct/enum you declare, not just the standard
library's own types, which is exactly how `Option.map`,
`Array.reverse`, `Map.get`, and the rest of the standard library below
are themselves built.

Two types can each declare a function with the same name (`Point.add`
and `Circle.add` coexist fine); there's no collision, since each
lives in its own type's namespace. This is unrelated to (and doesn't
change) qualified enum-variant construction, `Type.Variant(args)`,
(e.g. `Shape.Circle(radius)`, always legal even without a `use`, the
same as a bare `Circle(radius)`) still constructs a variant, not an
associated-function call. The two are disambiguated by capitalization:
an associated function name is always lowercase, a variant tag is
always UpperCamelCase.

## Option and Result: no null, anywhere

```plum fragment
enum Option[T] { Some(T), None }
enum Result[T, E] { Ok(T), Err(E) }
```

These are ordinary generic enums, available in every program with no
`use`/declaration of your own, the same as if you'd written them
yourself at the top of the file. There's no `?`-operator/early-return
sugar yet; propagate a `Result` with an explicit `match`:

```plum fragment
use Os;

let read_two (): Result[String, String] = match Os.read_file("a.txt") {
    Err(e) => Err(e),
    Ok(a) => match Os.read_file("b.txt") {
        Err(e) => Err(e),
        Ok(b) => Ok(a.concat(b)),
    },
}

let main (): Unit = println(read_two(()))
```

Note the call site: `read_two(())`, not `read_two()`. Every function
takes exactly one (possibly curried) argument; a `()` parameter list
in a declaration is shorthand for one `Unit`-typed parameter, not zero
parameters, so calling it explicitly passes the unit value `()`.

For the common cases, combinators avoid writing the `match` out by
hand. [MODULES.md](MODULES.md#standard-library) has the full list;
the common ones are:

```plum fragment
let doubled = Option.map(Some(21), |x| x * 2);          // Some(42)
let total = Result.unwrap_or(Os.read_file("a.txt"), "");    // "" if the file is missing
```

## Arrays

```plum fragment
let xs = [1, 2, 3];
let doubled = Array.map(xs, |x| x * 2);          // [2, 4, 6]
let evens = Array.filter(xs, |x| x % 2 == 0);    // [2]
let total = Array.fold(xs, 0, |acc, x| acc + x); // 6
let ys = xs.push(4);                             // [1, 2, 3, 4]; xs itself is untouched
```

`map`/`filter`/`fold` are the one part of the standard library that's
implemented as compiler primitives (not ordinary Plum functions) —
called as `Array.map(xs, f)`, never `xs.map(f)`. Dot-call syntax
(`value.name(...)`) is reserved exclusively for the small, fixed set of
zero-argument core value conversions (`.to_string()`, `.to_int()`,
`.round_to_int()`, `.to_float()`, `.as_cstr()`, plus true mutation-shaped
array/string operations like `.push()`/`.len()`); every stdlib function
that takes real arguments, `map`/`filter`/`fold` included, is always
`Type.func(value, ...args)`. This keeps the rule simple to remember
("does it take extra arguments? then it's `Type.func(...)`") and avoids
ambiguity between field access and method dispatch.

Array mutation-shaped methods (`.push()`, `.pop()`, `.set()`,
`.remove()`) are all *functional*: they return a new array rather than
mutating in place, but the compiler applies a reuse-in-place
optimization (FBIP) under the hood when it can prove the original array
is no longer needed, so this is often as cheap as a real mutation
without giving up value semantics.

## Pipe

`x |> f(a, b)` inserts `x` as `f`'s *last* argument, `f(a, b, x)`, and
`x |> f` (no parens) means `f(x)`. Chains read top to bottom instead of
inside out:

```plum fragment
[1, 2, 3]
    |> Array.map(_, |x| x * 2)
    |> Array.filter(_, |x| x > 2)
    |> Array.fold(_, 0, |acc, x| acc + x)   // 10
```

Since most stdlib functions take their array/subject *first*, not last,
a bare `_` in one of `f`'s arguments marks where `x` actually goes
instead of appending it. That's what `_` is doing in every call above:
without it, `[1,2,3] |> Array.map(f)` would (wrongly) mean
`Array.map(f, [1,2,3])`. At most one `_` per call; a plain single-
argument call like `xs |> Array.reverse` doesn't need one.

**Pipe + `Result.and_then`/`Result.map` is the house style for
chaining fallible calls**. Plum has no `?`/early-return (deliberately
not built, see DESIGN.md's own section: it would need a `return`
statement the language doesn't have at all, plus a `From`-style error-
conversion mechanism the closed trait set has no room for):

```plum fragment
Net.write(fd, request)
    |> Result.and_then(_, |ignored| read_response(fd))
    |> Result.and_then(_, parse_response)
```

reads top-to-bottom instead of the nested-`match` alternative
(`match x { Err(e) => Err(e), Ok(v) => match ... }`). It has one real
limit worth knowing: a later step needing a value from TWO steps back
can't stay flat; wrap that one step in a closure so the earlier
binding stays in scope via capture (`Result.and_then(_, |head| Result
.map(read_body(head), |body| Response { head, body }))`).

## Strings

```plum fragment
let s = "hello";
s.len()                    // 5
s.concat(" world")         // "hello world"
s.split(",")                // Array[String]
s.trim()
s.to_upper() / s.to_lower()
s.starts_with("he") / s.ends_with("lo") / s.contains("ell")
s.replace("l", "L")
s.runes()                  // Array[Int]: Unicode codepoints
s[0]                       // indexing returns a raw byte, not a character
```

**Bytes or characters?** `.len()` counts BYTES; everything else in the
string library counts characters. `String.slice` can never split a
multi-byte character in half, because it works on codepoints and never
sees bytes at all. When you need a count that matches, use
`String.char_len`:

```plum fragment
"café".len()               // 5 (bytes)
String.char_len("café")    // 4 (characters)
String.slice("café", 0, 3) // "caf"
```

Padding counts characters too, which is the point of it: text lined
up in columns by byte count puts an accented name in the wrong place:

```plum fragment
String.pad_left("7", 5, "0")     // "00007"
String.pad_right("ab", 5, ".")   // "ab..."
```

Both return the string unchanged rather than truncating it when it is
already at least that wide, and unchanged rather than panicking when
the fill is not exactly one character.

**Interpolation**: `"${...}"` inside any double-quoted string, no
prefix needed —

```plum fragment
let name = "world";
let n = 41;
println("hello, ${name}! n=${n + 1}")   // hello, world! n=42
```

is exactly `"hello, ".concat(name.to_string()).concat("! n=").concat((n
+ 1).to_string())`, pure syntax sugar over `.concat()`/`.to_string()`
(both already generic over every type), resolved entirely by the
lexer/parser, so it works everywhere a string literal does. A bare `$`
not followed by `{` is always literal; `\$` escapes one that would
otherwise start an interpolation. `${...}`'s contents can be any
ordinary expression (arithmetic, field access, calls, ...) but can't
itself contain a block expression, a closure with a block body, or a
nested string with its own `${...}`; pull those into a variable first.

## Local mutability, `if`/blocks as expressions

```plum fragment
let go (): Int = {
    let mut total = 0;
    let mut i = 0;
    for i in 0..10 {
        total = total + i;
    };
    total
}
```

`if`/`match`/blocks are all expressions; the last expression in a
block (no trailing `;`) is its value. `else` always requires either
`else if` or a `{ }` block; a bare `else <expr>` isn't valid syntax.

## Compile-time builtins: `@name(...)`

A leading `@` marks a call the **compiler** performs while reading your
source, rather than one your program performs while running. There is
one today:

`@embed_file("path")` is replaced by that file's contents, as a
`String`, while the source is being parsed:

```plum fragment
let template (): String = @embed_file("templates/adr.md")
```

The path resolves against **the source file's own directory**, not the
working directory the compiler was launched from, so a build does not
depend on where you started it. A module in a subdirectory embeds
relative to itself.

The argument must be a literal string. The file is read at compile
time, so it cannot depend on a value, and an interpolated string
counts as a value, not a literal:

```plum fragment
@embed_file("templates/${name}.md")   // error, and says why
```

Embedded text is data. It is never re-lexed as Plum, so `${...}` inside
an embedded file stays exactly as written.

The sigil also keeps builtins out of the identifier namespace, so
nothing is reserved; `embed_file` remains an ordinary name you are
free to define:

```plum fragment
let embed_file (p: String): String = read_or_default(p)   // fine
```

Text only: the result is a `String`, so this covers templates, schemas,
SQL, fixtures and help text, but not images. A missing file is a
compile error pointing at the call.

## Concurrency

`spawn`/`.join()` for tasks, and channels (`Sender`/`Receiver`) with
`send`/`recv` for communication between them. `channel[T]()` needs its
type argument written out, because there is nothing else in the expression to
infer it from.

**`select`** waits on several channels at once and takes whichever is
ready first:

```plum fragment
let got = select {
    n = numbers => n.to_string(),
    s = words => s,
};
```

Arms are swept in written order, so an earlier one wins a tie. Two ways
of not waiting forever:

```plum fragment
// Take one if ready, otherwise carry on. Never blocks.
select {
    n = rx => Some(n),
    else => None,
}

// Wait up to a Duration, then take the timeout arm.
select {
    n = rx => Ok(n),
    Time.millis(200) => Err("timed out"),
}
```

A timeout arm is any expression of type `Time.Duration`, so a
configurable deadline can be held in a variable. `else` is exactly a
timeout of zero, and having both in one `select` is rejected; `else`
would fire first every time, so the timeout could never be reached.
An empty `select {}` is rejected too, as is one with no channel arm:
neither is waiting for anything.

See DESIGN.md's "Concurrency" section for the full memory-ownership
story around sending heap values across task boundaries.

## FFI

```plum fragment
extern "C" {
    fn strlen(s: CStr) -> Int;
}

let go (): Int = unsafe { strlen("hello".as_cstr()) }
```

Extern calls are only allowed inside an `unsafe { }` block. The extern
type surface is intentionally closed: `Int`/`Float`/`Bool`/`CStr`/a
callback/a struct made of those: no raw pointers, no C-variadic
functions, no extern global variables.

`.as_cstr()` goes `String -> CStr` (for passing Plum strings out to C);
`.as_string()` goes the other way, `CStr -> String` (for turning a C
function's returned string data (a socket's `tcp_recv`, say) into a
real, usable Plum value). `CStr` otherwise has no operations of its
own.
