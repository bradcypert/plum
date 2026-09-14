Plum is a small, statically typed, compiled language.

C sources can be platform-specific, and every checker error now says
where.

## `native/` selects by target

A project's C sources are discovered rather than listed, which is how an
example reaches its raylib shim without being told to. That worked while
every shim was portable and stopped working the moment one was not: a
POSIX-only terminal shim and its Windows counterpart cannot both be
compiled for either target.

```
myproject/
  main.plum
  native/
    helpers.c          every target
    posix/term.c       Linux and macOS
    linux/epoll.c      Linux only
    macos/kqueue.c     macOS only
    windows/term.c     Windows only
```

A `.c` file directly under `native/` is compiled for every target, so
every project written before this keeps working unchanged. A
subdirectory named for a target is compiled only when building for it.

The four names are `linux`, `macos`, `windows` and `posix`, the last
meaning Linux and macOS. They are the same strings `Os.platform()`
returns, so there is one spelling of "windows" to remember rather than
two. Files that are not `.c` are left alone wherever they sit, so
headers live beside the sources that include them.

Selection follows the **target**, not the machine you are on:
`plum build . --target x86_64-pc-windows-gnu` compiles `windows/` and
ignores `posix/`.

Anything else is an error rather than something skipped:

```
native/win32: not a target directory.
  Plum understands linux, macos, windows and posix. A `.c` file
  directly under `native/` is compiled for every target.
```

`native/win32/` would otherwise compile on no target at all, and the
symptom would be a linker error naming a symbol whose source is sitting
right there in the tree.

## Every checker error says where

Five diagnostics used to arrive with no source position and an internal
prefix in place of one:

```
self-hosted type checker: field shapes.Circle.radius is private to module `shapes`. ...
```

They now point at the line you have to change:

```
error: field shapes.Circle.radius is private to module `shapes`. Add `pub` to it to use it from the root module
  --> main.plum:9:5
  |
9 |     println(shapes.area(shapes.Circle { radius: 2.0 }).to_string());
```

The privacy errors point at the **use**, which is where you fix
something. The handle errors point at the **declaration**, which is the
thing that is wrong.

The cause was structural rather than five oversights. The error position
lived in one half of the type checker and these checks were in the
other, which could not reach it. Three of them ran during body
inference, with a perfectly good position sitting there that the call
across threw away.

The `self-hosted type checker:` prefix still exists and is now honest:
it marks the errors that could not say where, and what still reaches it
are internal-invariant checks, which are compiler bugs rather than
anything you wrote.

## Also in this release

- `bootstrap/cli-smoke` covers which `native/*.c` sources a target
  selects. The proof in both directions is an `#error` in the directory
  that must not be reached, which needs no cross toolchain: a file that
  is compiled says so, and a file that is skipped is silent.
