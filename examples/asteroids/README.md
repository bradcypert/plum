# Asteroids

A full, playable Asteroids clone in Plum, rendered with real
[raylib](https://www.raylib.com/). The most complete demonstration in
this repo of Plum's "functional but in place" story: the entire game
state is one `Game` struct, threaded through `update`/`draw` and
rebuilt every frame with `..` struct-update spreads — never mutated in
place — while FBIP reuse-in-place still makes that compile down to real
mutation whenever the old `Game` isn't needed anymore (see `main.plum`'s
own top-of-file comment, and the plum repo's own README, for the full
story).

Unlike every other example in `examples/`, this one links a native C
library (raylib) and can't be run with plain `plum run`/`plum build` —
it needs `make`, and raylib installed and on your linker path first.

## 1. Install raylib

Pick whichever matches your system:

```sh
# Debian/Ubuntu
sudo apt install libraylib-dev

# Fedora
sudo dnf install raylib-devel

# Arch
sudo pacman -S raylib

# macOS (Homebrew)
brew install raylib
```

If your package manager doesn't have it (or you want the latest),
[raylib's own "Working on GNU Linux" / "Working on macOS" build
instructions](https://github.com/raysan5/raylib/wiki) walk through
building and installing from source — this example only needs the
shared library and headers on your normal include/link paths
afterward, nothing raylib-version-specific.

## 2. Install `plum`

From the root of the `plum` repo — `clang` is the only requirement:

```sh
./bootstrap/from-seed -o plum
./plum build bootstrap/self_host -o plum
```

Or download a binary from
[Releases](https://github.com/bradcypert/plum/releases).

## 3. Build and run

From this directory:

```sh
make        # builds ./asteroids
make run    # builds (if needed) and runs it
make clean  # removes the built binary
```

`make` runs `plum build . -o asteroids --link-lib raylib` — the
`--link-lib raylib` is what actually links raylib in; `native/
raylib_shim.c` (auto-discovered from this project's `native/`
directory) is what makes that possible at all — see its own doc
comment for why a shim is needed instead of binding raylib directly
(raylib's real C ABI uses 32-bit `float`/`unsigned char`-sized struct
fields, outside Plum's `extern "C"` closed type surface).

## Controls

| Key                 | Action                     |
|---------------------|----------------------------|
| `A` / `Left Arrow`  | Rotate left                |
| `D` / `Right Arrow` | Rotate right               |
| `W` / `Up Arrow`    | Thrust                     |
| `Space`             | Fire                       |
| `Enter`             | Restart (after game over) |

Survive the asteroid field, split large asteroids into smaller ones by
shooting them, and clear each wave to advance — you have 3 lives and a
couple seconds of invulnerability after each respawn.
