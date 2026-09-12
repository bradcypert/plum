Plum is a small, statically typed, compiled language.

A Plum project can now depend on another one.

## Path dependencies

A project says what it depends on in a `plum.pkg` file at its root:

```
Package {
    name: "myapp",
    version: "0.1.0",
    deps: [
        Dep { name: "semver", path: "../semver" },
    ],
}
```

A dependency is an ordinary project: a directory of `.plum` files that
builds, tests and type-checks on its own. Nothing is published and
nothing is fetched.

There is no new build step and no new command. `plum check`, `run`,
`build`, `test` and `doc` all read the dependency's source, and the
language server sees it too, because the change is one function: a
dependency is source, and the compiler already knew how to read source
from a directory. The only new question was which directories.

`pub` means the same thing across a package boundary as it does across
a module boundary. A dependency's unexported names are unavailable, not
merely undocumented.

Dependencies of dependencies come along, resolved relative to the
manifest that names them. Two packages that depend on each other
terminate rather than recursing.

**A project with no dependencies needs no manifest at all.** `plum new`
still writes exactly one file.

## A manifest is data, not a program

`plum.pkg` is read by Plum's own lexer and parser. That is why there is
no second configuration format to learn, and it is also the whole risk:
`setup.py`, `build.rs` and `build.zig` each began as configuration and
became programs, because a config file written in a general-purpose
language invites computing in it, and then the tool has to *run* the
file in order to read it.

Two things were being conflated there, and only one is dangerous.
Reusing a parser to read data carries no risk: text goes in, a tree
comes out, nothing executes. Making the file look like a program carries
all of it. So `plum.pkg` has no `let`, no declaration, no `main`, and is
deliberately not a `.plum` file. It holds a bare value, in a file
nothing will ever compile.

And the shape is enforced rather than trusted. Anything that is not a
string, a number, `true`/`false`, an array or a struct literal is
rejected:

```
plum.pkg: a manifest is data, not a program, and a function call is not data.
  Only strings, numbers, `true`/`false`, arrays and struct literals are allowed.
```

String interpolation is caught by the same rule without needing its own
case, because an interpolated string parses to a concatenation. Text
after the value is rejected rather than ignored. An unknown field is an
error, so `dependencies:` written for `deps:` says so instead of quietly
building a project with no dependencies.

**There is no build file**, and that split is the point. `plum build`
knows how to build a Plum project because there is only one way to build
one, and nothing in a manifest can change that.

## Where a library's code goes

A dependency contributes its module **subdirectories**. Source at a
dependency's root is an error, so a library is laid out one level deeper
than you might first write it:

```
semver/
  plum.pkg          declares that it is called `semver`
  semver/           module `semver`
  compat/           module `compat`
```

The root module is where `main` lives and its names are unqualified, so
a package may not put anything there. More usefully: a module is named
by its directory, chosen by the library's author, and it is the same
name whether the library is built alone or used from somewhere else.
That is what lets `compat/compat.plum` say `use semver;` and keep
working in both.

This rule replaced a worse one within a day of shipping it, and the
first real example is what found the problem. The details are in
[issue #36](https://github.com/bradcypert/plum/issues/36); the short
version is that the previous rule let a *consumer* rename a library's
own module, and gave a library two different module layouts depending on
who was building it.

[`examples/packages/`](examples/packages/) is the whole thing: two
projects, a library and something that depends on it, about a hundred
lines with a README.

## What is not here

Versions are recorded and not used. There is no registry, no fetching,
no lockfile and no resolver, so a dependency is a directory you already
have. Those are separable and tracked on
[#36](https://github.com/bradcypert/plum/issues/36).

The compiler's own bootstrap is unchanged and stays that way: a seed,
and no network.

## Also in this release

- `bootstrap/pkg-check`, 27 checks. Half assert that dependencies
  resolve; half assert the rejections above, because the risk to a rule
  like "a manifest is data" is not a user hitting it once but a
  maintainer relaxing it twice over two years.
- `bootstrap/example-sweep` now understands a library. A directory under
  `examples/` with no `main.plum` is type-checked and must be named by
  some example's `plum.pkg`. It used to be skipped in silence, which is
  the exact failure that harness was written to stop.
- `INSTALL.md`'s macOS example uses a glob rather than a version number.
  The number had said `0.0.7` for nineteen releases.
