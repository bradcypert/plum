# Maintaining Plum

How to change this compiler without breaking it, and what has broken it
before.

[DESIGN.md](DESIGN.md) is the history — why things are the way they
are. This is the operating manual.

## Before you commit

```sh
./sh build bootstrap/self_host -o sh.real   # your change, compiled in
for h in check-version check-shims cross-check lsp-smoke test-smoke net-smoke \
         property-check \
         corpus-check example-sweep \
         bootstrap-check self-sufficiency check-seed; do
    ./bootstrap/$h || echo "FAILED: $h"
done
```

About two minutes. If you only run two, run `corpus-check` and
`bootstrap-check`.

## The harnesses

| script | what it proves | time |
|---|---|---|
| `check-version` | the version string, the tag and the built binary agree | <1s |
| `check-shims` | the embedded C shims match `native_stdlib/`, and include no non-portable header outside a platform guard | <1s |
| `lsp-smoke` | the language server answers a real session: live diagnostics on unsaved text, hover, go-to-definition, and completion from all three sources | 1s |
| `test-smoke` | `plum test` really runs tests, and both engines agree | 1s |
| `property-check` | invariants hold over generated inputs -- the only harness that can catch the compiler being confidently wrong | 1s |
| `net-smoke` | TCP and HTTP work in a compiled binary | 1s |
| `cross-check` | every C shim compiles, and the compiler links, for macOS arm64/x86_64 and Windows | 2s |
| `platform-smoke` | a compiler *binary* builds and runs every execution fixture on the machine it is sitting on | 21s |
| `example-sweep` | every `examples/` project matches its recorded output | 5s |
| `bootstrap-check` | the compiler compiled by itself is the same compiler | 14s |
| `check-seed` | the checked-in seed still bootstraps to today's compiler | 26s |
| `self-sufficiency` | it builds itself with no Rust, from any directory | 27s |
| `corpus-check` | every corpus fixture compiles, runs, prints the right thing, aborts when it should, and leaks nothing | 29s |

`cross-check` needs `zig` and skips cleanly without it. Run it after
touching anything in `native_stdlib/`: it compiles every shim for macOS
and Windows from this Linux box in about two seconds, using Zig's
vendored libc headers, so no Xcode SDK or Windows toolchain is needed.
It proves compilation and linking only -- **nothing it produces is ever
run**. It exists because two consecutive Windows CI failures were the
same shape, a POSIX header or call left outside a platform guard,
invisible on Linux where the guard is inert. Both would have been
caught here in a second rather than a round trip. It has since caught a
third.

`platform-smoke` is the odd one out and is not part of the loop above.
It is the only harness that runs on macOS and Windows, so it is written
in POSIX `sh` and uses no GNU `timeout`, no ASan and no `./sh` wrapper —
see its header and [PORTING.md](PORTING.md). On Linux it is redundant
with `corpus-check`, which checks strictly more; run it when you have
changed the shims, the shell-outs, or anything about how `plum build`
reaches `clang`.

Every harness here runs with `clang` alone. There is no Rust in this
repository — see "Things that are deliberately true".

`property-check` is the one to reach for after touching the prelude or
the runtime. Everything else here compares against a CHECKED-IN answer,
which is whatever the compiler produced last time -- so a bug that was
present when the answer was recorded is invisible to all of them. The
properties compare against invariants instead. They cover round trips
(int, float, JSON, split/join, slice), the laws arrays and strings obey
(reverse twice, take ++ drop, contains agreeing with index_of), float
ordering, sorting, and arithmetic near the overflow guards.

Add properties when you add stdlib functions. Two of the first six
found real bugs, and both were bugs the retired Rust interpreter had
too -- which is why they had survived.

Three generators, run deliberately rather than routinely:
`gen-seed`, `gen-shims`, `record-examples`. And one packager,
`package-release`, which the release workflow calls once per platform;
it can be run by hand to reproduce exactly what a release job produced.

## The seed

`bootstrap/seed/plum.ll` is the compiler as LLVM IR. It is **the only
way to get a compiler from a clean clone** — nothing else in the
repository can build `bootstrap/self_host`.

**Refresh it when `check-seed` fails, and not before.** The seed does
not need to be current; it needs to be new enough to compile today's
source.

```sh
./bootstrap/check-seed     # fails when the seed has fallen behind
./bootstrap/gen-seed       # then, and only then
./bootstrap/check-seed     # confirm
```

Each refresh puts ~6MB of generated text into history, so it belongs in
its own commit with a reason. A good reason: `check-seed` failed, or
you are cutting a release.

`check-seed` proves four things, and the last is the one that
matters: the seed carries every shim in `native_stdlib/`, at its
current content; clang can build the seed; the seed compiler can build
today's source; and what it produces emits IR identical to the current
compiler. A seed that built
but produced a *different* compiler would silently bootstrap something
other than this source tree.

**What makes the seed go stale** has two causes, and only the first is
obvious.

1. The compiler's own source uses a language or prelude feature the
   seed's compiler does not have. Adding `print` to the prelude and
   calling it from `main.plum` did exactly that.
2. **A shim was added, renamed, or EDITED.** The seed embeds the shim
   sources it writes out when *it* builds something, so a seed that
   predates a shim change bootstraps a compiler carrying the old one.
   Presence is checked by name; content is checked against
   `bootstrap/seed/shims.sha256`, a fingerprint `gen-seed` records of
   `native_stdlib/*.c` at the moment the seed was built. Nothing else
   can see this: the seed's *output* is identical whether its embedded
   copy is current or three edits stale.

Cause 2 was found on 2026-08-23 and is why `check-seed` grew its first
step. It is invisible on Linux — every harness passes, because the
harnesses pass the shims to clang themselves — and shows up only on a
platform where a shim supplies a libc name that is genuinely absent
(`compat_shim.c` on macOS and Windows), as an unresolved-symbol error
with nothing connecting it to the seed.

## Bootstrap generations

**A compiler carries the prelude it was built with.** So
`bootstrap/self_host` cannot call a prelude function that the compiler
compiling it does not have — even though that function is right there
in the source you are compiling.

Symptom: you add a prelude function, use it in the compiler, and the
build fails with `unbound function: yours`.

Do it in two steps:

```sh
# 1. add the prelude function ONLY. Do not use it yet.
./sh build bootstrap/self_host -o sh.real     # now the compiler HAS it

# 2. now use it in the compiler's own source.
./sh build bootstrap/self_host -o sh.real
```

A prelude change that the compiler itself does not use needs only one
rebuild — but it takes **two** to show up in behaviour that the
compiler's own code depends on. When a prelude fix appears not to work,
this is usually why: `String.parse_float`'s precision fix took two
generations to reach the compiler's own lexer.

## Traps that have bitten more than once

**Duplicate `declare`.** A symbol belongs in the runtime's declare list
OR in a user `extern` block, never both. LLVM rejects the second one.
This has happened five times — `memcmp`, `strlen`, `stdout_flush`,
`process_run_inherit`, `getenv`. Grep before adding.

**Runtime symbol names collide with Plum function names.** A Plum
function `f` compiles to `@plum_f`. So a runtime function named
`@plum_print` collides with the prelude's own `print`. Name a runtime
function after its intercepted *stub* (`print_raw` → `@plum_print_raw`)
— a stub is never emitted, so its mangled name is free.

**Do not copy IR out of DESIGN.md's older sections.** The Rust runtime
used `@plum_alloc_array` with elements at offset 24; this one uses
`@plum_array_new` with elements at 16. Those snippets are history, and
transplanting one is often right and sometimes silently wrong. The
wrong *name* fails to link, which is lucky; the wrong *offset* links
fine.

**Currying hides a missed call site.** Add a parameter to a function
and miss a caller, and the call becomes a partial application rather
than an error. It surfaces far away as `expected Function(...), found
X`. If you see that message, look for a call with too few arguments.

**Temp directories leak.** Two separate leaks filled `/tmp` with 21GB
across 54,000 directories, and the resulting failures looked like real
compiler bugs. Anything that `mktemp`s must clean up on every path,
including the failing ones.

**Your machine has libraries CI does not.** `example-sweep` passed
locally and failed the first release because raylib happened to be
installed here. A harness that depends on a system library should
degrade with a stated reason, not fail.

**A directory is a project.** Nesting a test fixture inside another
project makes it part of that project. A fixture with deliberate errors
will poison the enclosing project's diagnostics.

## Where a change goes

**A new prelude function** → `bootstrap/self_host/codegen/prelude.plum`.
If the compiler itself will use it, remember the two-generation rule.
There is one prelude now; a second one in the Rust interpreter used to
drift from it, and that is how the missing networking stack was
found.

**A new runtime primitive** → three places: a stub in `prelude.plum`, an
interception in `cg_runtime_fn` (`codegen.plum`), and the implementation
in `runtime.plum`.

**A new C shim** → `native_stdlib/`, then `./bootstrap/gen-shims` to
embed it, or `check-shims` will fail. A shim that a compiled *program*
calls must be embedded even if the compiler itself never calls it.

**A new fixture** — pick by what it needs to prove:

| | |
|---|---|
| a program that runs and prints | `bootstrap/exec_corpus/` |
| a program that must be REJECTED | `bootstrap/typecheck_corpus/` |
| a program that must DIE with a message | `bootstrap/abort_corpus/` |
| a token or AST shape | `bootstrap/corpus/` |
| anything needing a live process, a port, or a timeout | its own `*-smoke` script |

That last row is not a formality. A networking fixture in
`exec_corpus` hung for ten minutes, and a hang there blocks every
future run of everything.

## Documentation rots faster than code

Every false claim found in this repository was in a comment or a README
that nothing executed. Four in one week: a test runner documented as
interpreting when it compiled, two corpora described as validated that
nothing ran, a `Ref[T]` limitation that had been fixed months earlier,
and a promise about division by zero that was not kept.

Two habits help:

- **Run it, do not recall it.** Before writing that something works,
  run it. Before writing a number, count it.
- **Prefer a script to a sentence.** `bootstrap/example-sweep` exists
  because a hand-maintained gap table in DESIGN.md was wrong three
  times running, always understating what was left.

Numbers in prose are the worst offenders. Neither this document's
script table nor `bootstrap/README.md`'s carries fixture counts, for
that reason — the scripts print their own. The timings above are
approximate on purpose.

## Cutting a release

The version lives in exactly one place: `plum_version` in
`bootstrap/self_host/main.plum`.

```sh
# 1. bump it, rebuild, verify
$EDITOR bootstrap/self_host/main.plum
./sh build bootstrap/self_host -o sh.real
./bootstrap/check-version

# 2. the seed will usually need refreshing for a release
./bootstrap/check-seed || ./bootstrap/gen-seed

# 3. full validation, then commit

# 4. tag -- this publishes
git tag v0.0.2 && git push origin v0.0.2
```

Pushing the tag runs `.github/workflows/release.yml`. It has three
build jobs and a publish job, and produces **four archives**: Linux
x86_64, macOS arm64, macOS x86_64, Windows x86_64. Each is built on the
platform it targets — nothing is cross-compiled — from the checked-in
seed, which is the same path a user takes.

The jobs check different amounts, deliberately. `linux` runs every
harness; `macos` and `windows` run `bootstrap/platform-smoke` only,
because the rest of `bootstrap/` is Linux-only development tooling.
macOS x86_64 is here and not in `ci.yml`: Intel Mac runners are scarce
enough to queue for hours, which is tolerable on a tag and not on a
push.

Every job ends in `bootstrap/package-release`, which packages the
archive and then **unpacks it and uses the binary inside** to build and
run a program before anything is published. It names the file from the
version the binary reports, and adds `.exe` for Windows.

`check-version` compares the tag against the version the *built binary*
reports, not just against the source — checking the source alone passes
on a stale binary.

The release workflow runs it twice, at different strengths. A `version`
job gates all three builds with `--source-only`, which needs no
compiler and so fails a mis-tagged commit in seconds; the `linux` job
then runs the full three-way check. Both exist because they catch
different mistakes: `v0.0.3` was first tagged on a commit still saying
0.0.2, and without the gate, macOS and Windows each built and packaged
a wrongly-named artifact before Linux noticed.

Update `RELEASE_NOTES.md`; it becomes the release body. Check its
claims by running them. Two of the 0.0.1 notes were wrong when drafted
from memory.

## Things that are deliberately true

Worth knowing before you "fix" them:

- **`plum test` compiles.** It does not interpret. It used to, and
  every test calling `assert_eq` failed for months.
- **Integer overflow stops the program.** `+`, `-`, `*` are checked.
  Roughly 1.6x on arithmetic-dense loops, unmeasurable on real code.
- **There is no Rust here.** The Rust front end and interpreter were
  retired on 2026-08-25. `property-check` replaced them: an oracle can
  only find *disagreements*, and the two bugs found the day it was
  written were ones the interpreter shared. See DESIGN.md's
  "Properties, and two bugs an oracle could never find".
- **The guard wrapper degrades.** `./sh` uses a cgroup memory cap where
  one is available and a plain timeout where it is not. The cap exists
  because of a real 44.9GB OOM that killed a terminal.
