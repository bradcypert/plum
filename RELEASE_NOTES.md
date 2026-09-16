Plum is a small, statically typed, compiled language.

Packages went from existing to being usable.

## Breaking: a dependency's modules are named by its package

A dependency contributed all of its modules as bare names, with nothing
saying which package they came from. `examples/packages` depends on one
package, `semver`, which has two modules, `semver/` and `compat/`:

```plum
// before: `compat` is semver's, and nothing here says so
compat.caret_allows(have, want)

// now
semver.compat.caret_allows(have, want)
```

`compat` is a name any package might ship, and two that did could not be
used together. It was a hard error, and its advice, "one of them has to
be renamed", was something the consumer could not act on: they own
neither package.

Now a dependency's modules live under its package name, so the collision
cannot occur rather than being reported.

(The `use` lines that named those modules were doing nothing, and still
are: `use` is decorative for anything that is not a standard-library
module. Giving it meaning is [#44](https://github.com/bradcypert/plum/issues/44).)

**A module named after its own package collapses to the package name.**
A library called `semver` whose main module is `semver/` is reached as
`semver.parse(..)`, not `semver.semver.parse(..)`.

**Inside a package, its own modules stay bare.** A library reads the
same whether it is built by itself or used from somewhere else, which is
most of what makes it a library.

**Your own project is untouched.** `use lexer;` and `lexer.tokenize(..)`
mean exactly what they did.

The one collision left is the only one a package prefix cannot remove:
your own module sharing a name with a package you depend on, since your
modules are bare by design. That is still an error, and unlike the old
message you can act on it, because you own one of the two:

```
`parsec` is both one of your modules and something dependency `parsec` provides.
  Your own modules are named bare, so rename yours: a directory called something else.
```

Short names are coming back: `use parsec.json;` then `json.parse(..)`,
with `use parsec.json as pj;` when one file needs two. That is
[#44](https://github.com/bradcypert/plum/issues/44), and it is
ergonomics rather than correctness now, so it gets its own release.

## A package can ship C

A dependency's `native/*.c` is compiled and linked. It was not before:
the Plum half of a package with a shim resolved perfectly and the link
failed with `undefined reference` naming a symbol whose source was
sitting in the dependency.

That ruled out every package touching C, which is the difference between
having packages and having an ecosystem.

Target selection applies per dependency, so a package carries
`native/posix/` and `native/windows/` the same way a project does.

## A package declares what it links against

```plum fragment
Package {
    name: "sqlite",
    link: [ "sqlite3" ],
}
```

Collected transitively, so a consumer names nothing. Before this, every
user of a package had to know an implementation detail of it and repeat
it on their own build.

Libraries differ by platform, so there are four more optional fields
using the same names as `native/`'s subdirectories, `posix` meaning
Linux and macOS in both:

```plum fragment
Package {
    name: "term",
    link: [ "m" ],
    link_posix: [ "pthread" ],
    link_windows: [ "ws2_32" ],
}
```

`link` takes library **names**, what `-l` takes. A linker flag is not a
library name and is rejected:

```
plum.pkg: `-Wl,--wrap=malloc` is not a library name.
  `link` takes what `-l` takes: `sqlite3`, `ws2_32`, `stdc++`. A linker
  FLAG is not a library name, and a manifest may not pass one into
  someone else's build.
```

A manifest that could carry linker arguments would be a manifest that
injects arbitrary behaviour into someone else's build, which is the same
line `plum.pkg` holds everywhere else: it is data.

## Upgrading

If you depend on a package, qualify its modules with the package name
and drop the `use` for them. Nothing else changes: your own modules,
the standard library, and every project without dependencies behave
exactly as before.
