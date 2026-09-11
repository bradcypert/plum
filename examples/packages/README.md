# Using another project as a dependency

Two projects, side by side:

```
examples/
  semver/            a LIBRARY: no main, exists to be depended on
    plum.pkg         declares that it is called `semver`
    semver/          module `semver`
    compat/          module `compat`
  packages/          this project
    plum.pkg         declares the dependency
    main.plum
```

Run it:

```sh
plum run examples/packages
```

And check the library on its own, because that is the point of it being
an ordinary project:

```sh
plum check examples/semver
```

## The manifest

```
Package {
    name: "packages",
    version: "0.1.0",
    deps: [
        Dep { name: "semver", path: "../semver" },
    ],
}
```

`plum.pkg` is data. It is read by Plum's own parser, which is why there
is no second configuration format, but it is not a `.plum` file and
nothing in it runs. Anything that is not a string, a number,
`true`/`false`, an array or a struct literal is rejected, so try
changing the path to `"../sem".concat("ver")` and see what the compiler
says.

A project with no dependencies needs no manifest at all.

## Where a library's code goes

A dependency's code lives in **module subdirectories**, never at the
project root. `examples/semver/semver/semver.plum` is module `semver`
and `examples/semver/compat/compat.plum` is module `compat`, whether
the library is built on its own or used from here.

Two reasons, and the second is the one you would hit:

- The root module is where `main` lives and its names are unqualified.
  A package putting names there would be writing into the unqualified
  namespace of every project that depends on it.
- A module is named by its directory, chosen by the library's author,
  and it is the same name in both contexts. `compat/compat.plum` says
  `use semver;`, and that has to keep working when the library is
  checked alone.

## What it costs you

Nothing else changes. `plum run`, `build`, `test`, `check` and `doc` all
read the dependency's source, and the language server sees it too. A
dependency's types are types: `main.plum` sorts by the library's own
`Order` enum with an ordinary `Array.sort_by`.

`pub` means the same thing across a package boundary as across a module
boundary. `semver.plum` has a private `part` helper, and no amount of
depending on the package will reach it.

## What is not here

Versions are recorded and not used. There is no registry, no fetching
and no lockfile, so a dependency is a directory you already have. See
[issue #36](https://github.com/bradcypert/plum/issues/36).
