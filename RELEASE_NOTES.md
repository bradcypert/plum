Plum is a small, statically typed, compiled language.

Plum can depend on code it did not ship with.

## `plum fetch`

A dependency can name a repository and a commit:

```plum manifest
// plum.pkg
Package {
    name: "myapp",
    version: "0.1.0",
    deps: [
        Dep {
            name: "parsec",
            git: "https://github.com/someone/parsec",
            rev: "9f2a1c4e8b7d3f6a0c5e2b9d4a8f1c3e7b6d0a52",
            sha256: "3b8f1d0c6a24e7593f8c1b0d4a6e29f7c53b8d1a0f462e97c8b3d5a1f0e6c294",
        },
    ],
}
```

```sh
plum fetch     # download it, into a cache shared across your projects
plum run .     # and everything after that is unchanged
```

Path dependencies landed in 0.0.34 and were the whole of the package
system until now. This is the half that lets code come from somewhere
you have not already got.

## Building never touches the network

A dependency that has not been fetched is an error, not a download:

```
dependency `parsec` has not been fetched.
  https://github.com/someone/parsec at 9f2a1c4e8b7d
  Run `plum fetch` to download it.
```

Cargo and Go both fetch implicitly, and it is a real convenience. It is
also a build that reaches the network without being asked. Fetching
implicitly can be added later; it cannot be taken back once builds
depend on it.

## The manifest is the lockfile

There is no second file, no resolver, no version solving and no
registry. A dependency is a URL and a commit, and `sha256` pins the
bytes at that commit.

The hash is checked on **every build**, not only on the fetch that
downloaded it — a cache entry is a directory on a disk that other
programs can reach, and verifying only what was just downloaded checks
the one case that was never in doubt. `plum fetch` prints the hash of
what it got, so pinning a dependency is a copy and a paste.

A `rev` is a full commit hash, and that is enforced:

```
plum.pkg: dependency `parsec` has `rev: "main"`, which is not a commit.
  A `rev` is a full 40-character commit hash. A branch or a tag moves, and a dependency that moves is not pinned.
```

An abbreviated hash is refused for the same reason one level down: it is
a pin that can become ambiguous as a repository grows.

## A global cache

`$PLUM_CACHE`, else `$XDG_CACHE_HOME/plum`, else `~/.cache/plum`, laid
out so you can read it:

```
<cache>/pkg/github.com/someone/parsec/9f2a1c4e8b7d3f6a0c5e2b9d4a8f1c3e7b6d0a52/
```

Shared between your projects rather than copied into each one, which is
safe *because* of the hash: packages are addressed by URL and commit and
verified by content, so two projects naming the same commit are naming
the same bytes. Each commit is its own directory, so two projects
wanting two versions is two directories rather than a conflict.

## Plum did not write its own TLS

It shells out to `git`, which makes git a **conditional** dependency:
needed to fetch packages, and for nothing else. A project whose
dependencies are all local paths never needs it, and neither does
building the compiler. Missing, it says so by name rather than failing
as something that looks like a network problem.

The tempting argument for the other side is a false one, and it is worth
recording. "Nobody writes their own TLS" is not true: Go wrote
`crypto/tls` in Go and `go get` uses it, Zig wrote `std.crypto.tls` in
Zig precisely so its toolchain would not need OpenSSL, and Java has done
TLS in Java since the 90s. Languages whose identity is "no C
dependencies" wrote their own.

What actually rules it out here is the starting point. Go and Zig built
their TLS on crypto their languages already had. Plum has SHA-256, as of
last release, and three encodings. No AES, no ChaCha20, no elliptic
curve, no bignum, no constant-time comparison. TLS from there is a
research programme, and this project's property tests can prove sorting
correct while being completely unable to tell you whether your AES is
constant-time.

**The bootstrap path stays `seed -> compiler` with no network, forever.**
Nothing here is on it.

## `plum doc` documents your project, not your dependencies

`plum doc myapp` used to write pages for every package `myapp` depends
on, under `myapp`'s name, in `myapp`'s output directory. Four of five
pages describing code the project did not write.

It was a consequence rather than a decision: dependencies reach every
command through one function, which is exactly the design that let
`check`, `run`, `build`, `test`, `doc` and the language server all see
them without any of them learning what a package is. `doc` is the one
command where it is wrong, because its output is something you publish
under your own name.

`--with-deps` keeps the old behaviour, and it earns its place: with no
registry and no hosted documentation, generating a dependency's pages
locally is currently the only way to read its API.

## Also in this release

- **An interrupted `gen-seed` destroyed the bootstrap seed.** It wrote
  straight over `bootstrap/seed/plum.ll` with a shell redirect, which
  truncates before the compiler emits a byte — so a Ctrl+C left the one
  file a clean clone cannot build without at zero bytes, silently, until
  somebody tried. It writes to a scratch file and moves it into place
  now, and refuses to install an empty emission.

- **The harness timings in `MAINTENANCE.md` were wrong by up to 7x**, and
  the pre-commit loop documented as "about two minutes" takes thirteen.
  Nobody had measured them since the repository was much smaller. They
  are measured, dated, and reproducible with a script now
  (`bootstrap/time-harnesses --measure`), and its cheap half runs in the
  loop to check the table still names every harness that exists. A
  developer who budgets two minutes and spends thirteen stops running
  the loop, which is the opposite of what the table is for.
