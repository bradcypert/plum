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

## Packages

A project can use code from another directory on disk. It says so in a
`plum.pkg` file at its root:

```plum fragment
// plum.pkg
Package {
    name: "myapp",
    version: "0.1.0",
    deps: [
        Dep { name: "parsec", path: "../parsec" },
    ],
}
```

A dependency is an ordinary project: a directory with `.plum` files in
it, which builds and tests on its own. Nothing has to be published, and
nothing is fetched. `plum check`, `run`, `build`, `test` and `doc` all
read the dependency's source alongside your own, and the language
server sees it too.

A dependency's modules arrive under the names its own directories give
them, which are the same names it uses when built alone:

```plum fragment
use parsec;
use json;

let main (): Unit = println(parsec.greet(json.tag()))
```

`pub` means the same thing across a package boundary as it does across
a module boundary: a dependency's unexported names are unavailable, not
merely undocumented.

Dependencies of dependencies come along, with their paths resolved
relative to the manifest that names them. Two packages that depend on
each other terminate rather than recursing.

A package's name belongs to the package. If a dependency has its own
`plum.pkg` declaring a `name`, the `Dep` naming it has to agree, so a
`path` edited to point somewhere else cannot go on claiming to be the
old dependency.

### A library's code goes in module subdirectories

The root module is where `main` lives and its names are unqualified, so
a package may not put anything there. A dependency contributes only its
module **subdirectories**, and source at a dependency's root is an
error:

```
dependency `parsec` has Plum source at its root: ../parsec/parsec.plum
  A dependency's code lives in module subdirectories, so its module
  names are the same whether it is built on its own or used from
  another project.
```

So a library is laid out one level deeper than you might first write
it, the same shape as Rust's `src/lib.rs`:

```
parsec/
  plum.pkg          declares that it is called `parsec`
  parsec/           module `parsec`
  json/             module `json`
```

The second half of that error message is the half that matters. A
module is named by its directory, chosen by the library's author, in
every context. That is what lets `json/json.plum` say `use parsec;` and
keep working when the library is checked on its own, which is most of
what "a dependency is an ordinary project" means.

[`examples/packages/`](examples/packages/) is the whole thing, two
projects and about a hundred lines.

### Two module names that collide

Function and type names are namespaced by module. Module names are not
namespaced by package, so two dependencies that both ship a `util`
module are a genuine ambiguity. That is an error naming both sides,
rather than one of them silently winning:

```
dependency `core` provides a module `json`, and so does something
already loaded.
```

Rename-on-import is the usual answer and can be added later. Until it
exists, one of the two has to be renamed.

### A manifest is data

`plum.pkg` is read by Plum's own lexer and parser, which is why there
is no second configuration format to learn. It is deliberately **not** a
`.plum` file, and it holds a bare value rather than a declaration.

That distinction is the whole design. `setup.py`, `build.rs` and
`build.zig` all began as configuration and became programs, because a
config file written in a general-purpose language invites computing in
it, and then the tool has to *run* the file in order to read it. Reusing
a parser to read data carries none of that risk: text goes in, a tree
comes out, and nothing executes. Making the file look like a program
carries all of it.

So the shape is enforced rather than trusted. Anything that is not a
string, a number, `true`/`false`, an array or a struct literal is
rejected, including string interpolation:

```
plum.pkg: a manifest is data, not a program, and a function call is
not data.
```

An unknown field is an error too, so `dependencies:` written for
`deps:` says so instead of quietly building a project with no
dependencies.

### What is not here yet

Version numbers are recorded and not used. There is no registry, no
fetching, no lockfile and no way to depend on a package you have not
already got a copy of. Those are separable, and the ordering is
discussed in [issue #36](https://github.com/bradcypert/plum/issues/36).

Whatever arrives, the compiler's own bootstrap stays what it is today:
a seed, and no network.

## Standard library

Two generated references, neither written by hand:

- **[`docs/stdlib/`](docs/stdlib/)** is one page per module, with the
  documentation written on each declaration. Start with
  [`prelude`](docs/stdlib/prelude.md), which is what every program gets
  without asking for it.
- **[`docs/stdlib/index.md`](docs/stdlib/index.md)** is every
  declaration in one file, each linked to the page that documents it,
  for when you want to search rather than read.

Both come out of `plum doc`, in one pass, and `bootstrap/check-docs`
fails if either has fallen behind the compiler. This section used to
list the library by hand; it was accurate, and nothing checked it, which
is a promise that keeps for exactly as long as somebody keeps
remembering.

The functions the compiler implements itself, `Array.map` and
`String.concat` among them, are in there too, even though they have no
Plum source to read.

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
