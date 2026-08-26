# A tour of Plum in twenty minutes

This walks from nothing to a program you would recognise as real. Every
snippet is a complete program — copy it into `main.plum` and run it.

If you have not installed Plum yet:

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

You also need `clang` on your PATH. Plum shells out to it to assemble
and link; that is the whole toolchain.

## 1. A project

```sh
plum new hello
plum run hello
```

A project is a **directory**, not a manifest file. There is nothing to
configure — `plum new` writes one `main.plum` and that is the project.

## 2. Functions, and types you mostly do not write

```plum
let double (n: Int): Int = n * 2

let main (): Unit = println(double(21).to_string())
```

```
42
```

Parameter and return types are annotations you write on purpose;
everything inside is inferred. `let total = double(21)` needs no type.

A function body is an **expression**, not a block of statements — which
is why there is no `return`. `{ ... }` is itself an expression whose
value is its last line:

```plum
let describe (n: Int): String = {
    let label = if n > 0 { "positive" } else { "not positive" };
    label.concat(": ").concat(n.to_string())
}

let main (): Unit = println(describe(7))
```

```
positive: 7
```

## 3. Structs

```plum
struct Point { x: Int, y: Int }

let shifted (p: Point) (by: Int): Point = Point { x: p.x + by, y: p.y + by }

let main (): Unit = {
    let p = Point { x: 1, y: 2 };
    let q = shifted(p, 10);
    println(q.x.to_string().concat(", ").concat(q.y.to_string()))
}
```

```
11, 12
```

Values are immutable. `shifted` builds a new `Point` rather than
changing one, and `p` is still `{1, 2}` afterwards.

To change one field of a big struct, use functional update — `..base`
fills in the rest:

```plum
struct Config { host: String, port: Int, verbose: Bool }

let main (): Unit = {
    let base = Config { host: "localhost", port: 80, verbose: false };
    let dev = Config { port: 8080, ..base };
    println(dev.host.concat(":").concat(dev.port.to_string()))
}
```

```
localhost:8080
```

## 4. Enums, and matches that cannot forget a case

```plum
enum Shape {
    Circle(Int),
    Rect(Int, Int),
}

let area (s: Shape): Int = match s {
    Circle(r) => 3 * r * r,
    Rect(w, h) => w * h,
}

let main (): Unit = println(area(Rect(3, 4)).to_string())
```

```
12
```

Delete the `Rect` arm and the compiler stops you:

```
error: match is not exhaustive — missing variant(s): Rect
```

That is the point of enums here. A new variant added a year later
becomes a compile error in every place that has to care, instead of a
bug you find in production.

## 5. There is no null

A value that might be missing is an `Option[T]`, and the only way to
read it is to handle both cases.

```plum
let main (): Unit = {
    let found = Array.find([1, 2, 3], |n| n > 2);
    let text = match found {
        Some(n) => "found ".concat(n.to_string()),
        None => "nothing",
    };
    println(text)
}
```

```
found 3
```

An operation that can fail is a `Result[T, E]`:

```plum
let main (): Unit = {
    match String.parse_int("41") {
        Ok(n) => println((n + 1).to_string()),
        Err(e) => println("bad number: ".concat(e)),
    }
}
```

```
42
```

When you genuinely do not care, `unwrap_or` takes a fallback:

```plum
let main (): Unit =
    println(String.parse_int("nope").unwrap_or(0).to_string())
```

```
0
```

## 6. Arrays, and the method rule

```plum
let main (): Unit = {
    let xs = [1, 2, 3, 4];
    let doubled = xs.map(|n| n * 2);
    println(doubled.to_string());
    println(Array.fold(doubled, 0, |acc, n| acc + n).to_string())
}
```

```
[2, 4, 6, 8]
20
```

`xs.map(f)` and `Array.map(xs, f)` are the same call. **A method is a
namespaced function whose first parameter is the receiver** — so
anything named `Array.something` is a method on arrays, `String.something`
is a method on strings, and a function you write yourself is no
different:

```plum
struct Point { x: Int, y: Int }

let Point.magnitude_squared (p: Point): Int = p.x * p.x + p.y * p.y

let main (): Unit = println(Point { x: 3, y: 4 }.magnitude_squared().to_string())
```

```
25
```

## 7. Getting it wrong

Type errors point at the source and say what they wanted:

```plum
let main (): Unit = println(1 + "one")
```

```
error: '+': Int != String
```

Integer overflow is checked, not wrapped — a program that overflows
stops rather than quietly continuing with a wrong number. Division by
zero stops too.

## 8. Tests

Any zero-argument function named `test_*` is a test. `plum new` writes
one for you.

```plum
let double (n: Int): Int = n * 2

let main (): Unit = println(double(21).to_string())

let test_double (): Unit = assert_eq(double(21), 42)
let test_double_zero (): Unit = assert_eq(double(0), 0)
```

```sh
plum test hello
```

```
running 2 tests
test test_double ... ok
test test_double_zero ... ok

test result: ok. 2 passed; 0 failed
```

Each test runs in its **own process**, so one that crashes does not take
the rest of the run with it.

## 9. A binary

```sh
plum build hello -o hello/out
./hello/out
```

`run` and `build` compile the same way and differ only in whether the
binary is kept. There is no interpreter, so nothing can behave one way
under one and differently under the other.

## Where to go next

- **[README.md](README.md)** — the reference: modules, the standard
  library, concurrency, the C FFI, and the editor integration.
- **`examples/`** — nine real projects, from a JSON round-trip to an
  HTTP server to an asteroids game.
- **`plum lsp`** — a language server with live diagnostics, hover and
  completion. See the README's "Editor support".
