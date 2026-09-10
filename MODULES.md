# Modules and the standard library

## Modules

A directory *is* a module, with no `mod foo;` declaration anywhere. Every
`.plum` file in a directory shares one namespace; subdirectories become
nested child modules, discovered from the file tree itself.

```
myapp/
  main.plum
  shapes/
    circle.plum
    rectangle.plum   // both files are just the `shapes` module
```

```plum
// shapes/circle.plum
pub struct Circle { pub radius: Float }
pub let area (c: Circle): Float = 3.14159 * c.radius * c.radius
let internal_helper (c: Circle): Float = c.radius * 2.0   // private, no `pub`
```

```plum
// main.plum
use shapes;
let main (): Unit = println(shapes.area(shapes.Circle { radius: 2.0 }).to_string())
```

```
12.56636
```

`use` is qualify-by-default (Go-style): `shapes.area`, not a bare
`area`, so call sites stay self-explanatory without cross-referencing
imports.

**Functions are private by default.** `pub let` opts one into being
callable from outside its module; without it, a qualified call is
rejected:

```plum
// shapes/circle.plum
let secret_helper (): Int = 1
```

```plum
// main.plum
use shapes;
let main (): Unit = println(shapes.secret_helper().to_string())
```

```
error: shapes.secret_helper is private to module `shapes`. Add `pub` to its declaration to use it from the root module
```

**Types are private by default too.** `pub struct` and `pub enum` opt
one into being named from outside its module: in an annotation, in a
literal, and in a pattern:

```plum
// secrets/s.plum
struct Secret { n: Int }
pub let make (): Secret = Secret { n: 7 }
pub let read (s: Secret): Int = s.n
```

```plum
// main.plum
use secrets;

// Fine: the VALUE may cross. `hold` never names the type.
let hold (): Int = secrets.read(secrets.make())

// Rejected: the NAME may not.
let named (): secrets.Secret = secrets.make()

let main (): Unit = println(hold().to_string())
```

```
error: struct secrets.Secret is private to module `secrets`
```

A private type that escapes through a `pub` function is **opaque**
outside its module: you can hold it and pass it on, but not take it
apart, because taking it apart means naming it. That is the same shape
a handle type has.

**Fields are private by default too**, independently of their struct:

```plum
// counter/c.plum
pub struct Counter { pub label: String, n: Int }
pub let start (label: String): Counter = Counter { label: label, n: 0 }
pub let count (c: Counter): Int = c.n
```

```plum
// main.plum
use counter;

let label_of (c: counter.Counter): String = c.label   // fine
let count_of (c: counter.Counter): Int = c.n          // rejected

let main (): Unit = println(label_of(counter.start("hits")))
```

```
error: field counter.Counter.n is private to module `counter`
```

A struct with any private field **cannot be constructed from outside
its module**, because a literal has to name every field. That is the
point rather than a side effect: it makes a constructor function the
only way in. The same applies to destructuring. `Counter { label, n }`
and the positional `Counter(label, n)` both name `n`, so both are
refused.

**Two modules may declare the same type name.** A type is identified by
the module that declared it, so `shapes.Circle` and `render.Circle` are
different types, and a bare `Circle` means the one declared where you
wrote it: your own module first, then the root, then the prelude. A
root declaration shadows a prelude one of the same name rather than
colliding with it.

They do not silently unify:

```plum
// inner/p.plum
pub struct P { pub v: Int }
pub let make (): P = P { v: 1 }
```

```plum
// main.plum
use inner;

pub struct P { pub v: Int }

// `P` here is the root module's own, which is not `inner.P`.
let a (): P = inner.make()

let main (): Unit = println("unreachable")
```

```
error: declared return type P doesn't match body type inner.P
```

**Anywhere a type or variant can be named, the module can be part of
the name.** When two modules declare the same enum, that is the only
way to say which one you mean: in an annotation, an expression, and a
pattern alike:

```plum
// light/shade.plum
pub enum Shade { On, Off }
```

```plum
// dark/lamp.plum
pub struct Lamp { pub watts: Int }
```

```plum
use light;
use dark;

let describe (s: light.Shade): String = match s {
    light.Shade.On => "on",
    light.Shade.Off => "off",
}

let lamp (): dark.Lamp = dark.Lamp { watts: 60 }

let main (): Unit = println(describe(light.Shade.On).concat(" at ").concat(lamp().watts.to_string()))
```

```
on at 60
```

Naming the wrong module is an error rather than a correction: a
`dark.Shade.On` pattern matched against a `light.Shade` reports the
mismatch.

The prelude is a module of its own, so `pub` applies to the standard
library too: `Map.get` is part of the interface, `Map`'s buckets are
not, and reaching for the latter is an error wherever you are.

Its module cannot be named; there is no `use prelude;` and no
`prelude.println(..)`. Prelude names are reached the way they always
were, unqualified; the module exists so that what the prelude does not
export is genuinely unavailable rather than merely undocumented.
`use shapes.Circle;` (importing one specific name unqualified) is
available as an escape hatch for names used constantly in a file.

### Standard-library modules

Most of the standard library is in the prelude, with no `use` needed —
and for the type namespaces that is not a convenience, it is required:
`T.f(x)` *is* the method-call mechanism, so `xs.map(f)` only works
because `Array.map` is always in scope.

A namespace that names no type is a different thing. Those are
modules, and a file that wants one says so:

| module | what is in it |
|---|---|
| `Os` | files, directories, environment, subprocesses, platform, exit |
| `Time` | the clock, and the calendar on top of it |
| `Net` | TCP sockets |
| `Http` | HTTP client and server, built on `Net` |

```plum fragment
use Os;
use Time;

let stamp (): String = Time.rfc7231(Time.now())
let here (): String = Os.platform()
let conf (): Result[String, String] = Os.read_file("app.conf")
```

A module can depend on another. `Http` is ordinary Plum over `Net`'s
sockets, so `use Http;` brings `Net` in with it, and you do not have to
know what a module is built on to use it.

Without the `use`, the error says what to do:

```
unbound variant/function: Time -- `Time` is a standard library module;
add `use Time;` to this file
```

`Time` moved in 0.0.8 and the rest in 0.0.9. `Os` was held back
deliberately: unlike `Time`, it was reachable from every program
already written, so the break got a release of its own rather than
being slipped in beside the feature that revealed it.

What stayed in the prelude, with no `use` needed: `println`/`print`,
the `assert` family, `Json`, and every type namespace.

## Standard library

Two generated references, neither written by hand:

- **[`docs/stdlib/`](docs/stdlib/)** is one page per module, with the
  documentation written on each declaration. Start with
  [`prelude`](docs/stdlib/prelude.md), which is what every program gets
  without asking for it.
- **[STDLIB.md](STDLIB.md)** is every signature in one file, for when you
  want to search rather than read.

Both come out of `plum doc` and `plum stdlib-reference`, and
`bootstrap/check-docs` and `bootstrap/check-stdlib-reference` fail if
either has fallen behind the compiler. This section used to list the
library by hand; it was accurate, and nothing checked it, which is a
promise that keeps for exactly as long as somebody keeps remembering.

`Net`, `Http`, `Os`, `Time`, `Process`, `Encoding`, `Url`, `Path`,
`Json` and `Terminal` need a `use`. Everything else is in the prelude
and needs nothing.

Documentation comes from `///` comments in the source, so the same
mechanism works on your own code:

```sh
plum doc my-project -o docs           # Markdown, one page per module
plum doc my-project -o docs --html    # a browsable site, with search
```

`plum highlight <file>` prints Plum source as marked-up HTML, using the
compiler's own lexer rather than a regex approximation of it. It is what
colours the code on [plumlang.org](https://plumlang.org). Every static
site generator ships a highlighter, and none of them ships one for a
language this young. Because it is the real lexer over a lossless token
stream, stripping the tags back out gives the source byte for byte, and
`bootstrap/highlight-check` asserts exactly that over every fixture and
every file of the compiler's own source. A snippet that does not lex
renders plain rather than not at all.
