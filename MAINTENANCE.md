# Maintaining Plum

How to change this compiler without breaking it, and what has broken it
before.

[DESIGN.md](DESIGN.md) is the history — why things are the way they
are. This is the operating manual.

## Before you commit

```sh
./sh build bootstrap/self_host -o sh.real   # your change, compiled in
for h in check-version help-check check-shims check-declares cross-check lsp-smoke test-smoke net-smoke \
         property-check doc-check alloc-check lossless-check fmt-check \
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
| `help-check` | `plum help`/`--help`/`-h` print usage, extra args ignored | <1s |
| `check-shims` | the embedded C shims match `native_stdlib/`, and include no non-portable header outside a platform guard | <1s |
| `check-declares` | every symbol the runtime declares is actually called -- an unused one silently blocks a user `extern "C"` block | <1s |
| `lsp-smoke` | the language server answers a real session: live diagnostics on unsaved text, hover, go-to-definition, and completion from all three sources | 1s |
| `test-smoke` | `plum test` really runs tests, and both engines agree | 1s |
| `property-check` | invariants hold over generated inputs -- the only harness that can catch the compiler being confidently wrong | 1s |
| `doc-check` | every snippet in `TUTORIAL.md` compiles, runs, and prints what the tutorial says it prints | 6s |
| `alloc-check` | allocation counts have not RISEN -- the only harness that measures the memory model rather than correctness | 2s |
| `debug-info-check` | a debug build carries Plum line information at the right LINES, and a release build carries none | 2s |
| `mem-check` | peak RSS of `emit-llvm` and `check` is under a ceiling -- the `SH_MEM` cgroup guard is inert on CI, so this is the only memory assertion that runs there | 3s |
| `net-smoke` | TCP and HTTP work in a compiled binary | 1s |
| `cross-check` | every C shim compiles, and the compiler links, for macOS arm64/x86_64 and Windows | 2s |
| `platform-smoke` | a compiler *binary* builds and runs every execution fixture on the machine it is sitting on | 21s |
| `example-sweep` | every `examples/` project matches its recorded output | 5s |
| `fmt-check` | every repo file is already formatted, `fmt` touches only leading whitespace, and `fmt_corpus` reformats as recorded | 40s |
| `lossless-check` | every `.plum` file survives a round trip through the token stream, and nothing but trivia sits between tokens -- the floor a formatter stands on | 30s |
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

## Merge conflicts in generated files

Four checked-in files are build OUTPUTS, and every one of them conflicts
whenever two branches touch the compiler:

| file | produced by |
|---|---|
| `sh.real` | `./sh build bootstrap/self_host -o sh.real` |
| `bootstrap/seed/plum.ll` | `./bootstrap/gen-seed` |
| `bootstrap/seed/shims.sha256` | `./bootstrap/gen-seed` |
| `bootstrap/self_host/shims/shims.plum` | `python3 bootstrap/gen-shims` |

**Never resolve one by choosing a side, and never hand-edit one.**
Picking "ours" or "theirs" gives you an artifact built from source that
no longer exists — a compiler binary missing half the merge, or a seed
that disagrees with `native_stdlib/`. Neither fails at merge time. Both
fail later, somewhere confusing.

`sh.real` makes this obvious because git cannot even pretend: it is a
binary, so you get `warning: Cannot merge binary files` and whichever
side git happened to leave in the tree. The other three are text and
will happily produce a "clean" result that is garbage — `plum.ll` is a
quarter of a million lines of generated IR, and a three-way merge of it
means nothing at all.

So: **resolve the real source first, then rebuild the outputs from it**,
in dependency order, because each one feeds the next.

```sh
# 1. Resolve every hand-written conflict (source, docs) and check it.
git status --short | grep '^UU'
./sh check bootstrap/self_host

# 2. Shims first -- the compiler EMBEDS them, so they must be right
#    before it is built. Needed whenever native_stdlib/ changed on
#    either side.
python3 bootstrap/gen-shims

# 3. The compiler, from the merged source, using whatever working
#    compiler you have (the conflicted `sh.real` in your tree is fine
#    -- it only has to compile the merge, not contain it).
./sh build bootstrap/self_host -o sh.real.new && mv sh.real.new sh.real

# 4. The seed, from the compiler you just built.
./bootstrap/gen-seed

git add sh.real bootstrap/seed bootstrap/self_host/shims/shims.plum
```

Then **verify before committing**, because a merge is the one moment
when two sets of compiler changes run together for the first time and
nothing has ever tested that combination:

```sh
./bootstrap/bootstrap-check     # the fixed point is the real test
./bootstrap/corpus-check
./bootstrap/check-seed
./bootstrap/check-shims
```

A merge that compiles is not a merge that works. The fixed point is
what catches a compiler that builds but miscompiles itself, which is
exactly the failure a bad artifact resolution produces.

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

### Emitted IR depends on the PATH you name, not just the source

Since debug builds carry line tables, the IR records the path it was
given: `DIFile(filename: "bootstrap/self_host/parser/parser.plum")`.
Name the same source `$PWD/bootstrap/self_host` instead and every
filename in the output differs, so two emissions compare unequal for a
compiler that is byte-identical.

Anything diffing two `emit-llvm` runs must therefore name the source
the SAME way on both sides, from the same directory. `check-seed` did
not -- it ran one side from a scratch directory with an absolute path --
and reported a stale seed twice for a compiler that was fine, including
once immediately after `gen-seed` had just refreshed it, which is the
tell: a genuinely stale seed does not survive being regenerated.

`bootstrap-check` and `self-sufficiency` were already consistent.
Whether the compiler works from another directory is
`self-sufficiency`'s question, and it varies the directory on both
sides rather than one.


**A static cell is only safe because every in-place path tests
`rc == 1` exactly.** There are four -- `@plum_reuse_ok`,
`@plum_array_reuse`, `@plum_str_concat_reuse`, `@plum_array_push_grow`
-- and a fifth that tested `<= 1`, or `!= 0`, would silently start
writing through string literals, hoisted constants and capture-free
closures. Check that property before adding one. `plum_rc_inc` and
`cg_rel_head` skip negative counts, which is the other half.

**Constant cells are written out BY HAND, so their layout is not the
one the stores produce.** Eight bytes per slot means an `i1` needs seven
bytes of padding after it and the cell must be PACKED -- unpacked, LLVM
lays two adjacent `i1`s at offsets 8 and 9 and every `cg_field_offset`
reading them is wrong. An enum constant pads to the enum's WIDEST
variant, because pattern matching loads a payload before confirming the
tag. `const_literal_layout` covers both.

**Closures capture free variables, and the scan must account for the
closure's OWN parameters.** `cg_used_captures` asks `cg_reads_max`
whether the body reads each enclosing slot, but that function applies
the parameter-shadowing rule only when IT walks a `TClosure`; asked
about a bare body it skips the step, and `|x| x * 10` kept capturing an
outer `x`. Dropping a capture that is needed fails the compile loudly
("unbound variable reached codegen"); KEEPING an outer binding that an
inner one shadows computes the wrong number silently, which is what
`closure_capture_shadowing` is for.

**Two analyses now move values out of slots.**
`cg_movable_params` hands a slot's reference to a `concat`; the
last-use pass (`lv_*`) hands one to `@plum_alloc_reuse`. A slot moved
twice is a double free. They are kept apart in `cg_reuse_slot`, which
declines any name `cg_movable_params` already claimed, and `movable` is
computed from the REWRITTEN body so the read a reuse adds is counted.
If you add a third thing that moves, it belongs in that same
arrangement — do not reason about why the shapes cannot overlap, they
overlapped once already.

**`cg_movable_params` is not filtered by type, on purpose.** It looks
like it should be -- its consumer `cg_move_or_own` seems to serve only
`cg_concat` -- but it has two callers, and the other is `cg_array_push`,
whose in-place growth depends on it. Narrowing the set to String-shaped
parameters took `array_push` from 10 allocations to 1002 and nothing
failed; only `alloc-check` noticed.

**Each reuse path decides its own drop, and they are not
interchangeable.** A struct's children are dropped INLINE under the
`rc == 1` branch, because its layout is fixed. An enum's are dropped by
a generated companion (`cg_emit_relc_fn`), because which children a
cell has depends on its runtime tag -- inlining that switch at every
reuse site is what the companion exists to avoid. `@plum_array_reuse`
deliberately does NOT release the source when it declines, because the
caller is about to read it; the caller compares pointers and releases
only when they differ. Six exec fixtures hold these apart
(`reuse_aliased`, `reuse_heap_fields`, `reuse_replaced_field`,
`reuse_array_aliased`, `reuse_enum_payload`, `reuse_enum_aliased`);
each fails a different way, none is redundant.

**Array reuse releases each source element INSIDE the loop, and the
branch guarding that must not be hoisted away.** `map` and `filter`
recycle a dead source cell, so nothing else will ever release the
elements it held: the slots are overwritten (`map`) or compacted over
(`filter`), and the cell itself becomes the result and is never
released. So the loop drops the source's reference to element `i` as it
passes -- `cg_reuse_elem_drop`.

It does that **only when `@plum_array_reuse` returned the pointer it
was given**, which is the loop-invariant `same`. A declined reuse means
either another owner still holds the array or it did not fit; in the
first case dropping its elements corrupts an array somebody else is
still reading, and nothing would catch it but a crash somewhere else
later. The branch looks like something an optimiser pass should hoist,
and hoisting it is a correctness bug.

`exec_corpus/array_reuse_heap_elems` covers this under ASan with leak
detection, which is the only way any of it is visible: every case in
that fixture prints the right answer while leaking.

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
- **`known_64_bit_arches()` is an allow-list, on purpose.** Adding a
  new architecture means adding its name there, and forgetting to is a
  clear refusal rather than a bug. Inverting it into a block-list would
  read as more permissive and be strictly worse: cell layout assumes
  8-byte slots, so an unrecognized 32-bit target would not fail to
  link, it would silently miscompile. See DESIGN.md's
  cross-compilation section.
- **`platform_libs` takes the TARGET, never `Os.platform()`.** Reading
  the host's platform there was the single line that made a cross-build
  impossible, and it is an easy one to reintroduce because on a native
  build the two are the same and nothing fails.
- **`plum run` and `plum test` ignore `--target` by design.** Both
  execute what they build. For `run`, everything after the project
  directory belongs to the program being run, so a `--target` there is
  the program's argument and must stay that way.
- **`embed_file` is handled in the PARSER, not codegen.** It has to be:
  it resolves against the source file's own directory, and the parser
  is the only stage that knows which file it is reading. A consequence
  is that `embed_file` is a reserved name — a user function by that
  name is intercepted and never reached.
- **The calendar functions use `plum__floor_div`, never `/`.** Plum's
  `/` truncates toward zero, which silently moves every date before
  1970 into 1970. If you add a calendar function, use the floor
  helpers, and add a timestamp before the epoch to
  `exec_corpus/time_calendar`.
- **`String.len` is bytes; everything else in that section is
  codepoints.** `String.char_len` is the character count. Padding uses
  the character count deliberately — that is what padding is for.
- **Builtins are `@name`, lexed as ONE token.** Adding one means a
  branch in `builtin_expr` and an entry in the error message that lists
  what exists. There is deliberately no bare `@` token — `@ 5` is a lex
  error, not a parse error.
- **`parser.std_module_names()` is THE list of stdlib modules.**
  `codegen` looks source up by those names; `typecheck` uses them for
  the missing-`use` hint. Adding a module means adding the name there
  and the source in `codegen/stdlib.plum` — a name with no source fails
  the build loudly, which is the intended direction.
- **`use` is load-bearing for stdlib modules only.** For directory
  modules it is still documentation the compiler does not check; module
  membership comes from the directory. Do not assume removing a `use`
  will break a directory-module call, because it will not.
- **`Os`, `Time`, `Net`, `Http` and `Process` are modules; the type namespaces
  are not, and cannot be.** `T.f(x)` is the method-call mechanism, so
  `Array.map` being in scope is what makes `xs.map(f)` work. Adding a
  module means a name in `parser.std_module_names()` and source in
  `codegen/stdlib.plum`.
- **A new fixture that calls `Os.` needs `use Os;`.** The harnesses
  will catch it, but the error is a type error at build time rather
  than something obviously about the fixture.
- **A prelude RENAME needs three builds; a prelude MOVE needs one.** A
  compiler carries the prelude it was built with, so source using a new
  prelude name cannot be compiled until a compiler knows it: build with
  both names, switch the call sites, build, delete the old names, build
  again. Then run `bootstrap/gen-seed` — `check-seed` fails until you
  do, and says so. A move behind `use` needs none of this, because the
  old compiler simply ignores the `use` and resolves from its own
  prelude.
- **`StdModule.needs` is how one stdlib module uses another.** `Http`
  needs `Net`. Adding an edge means adding it to `cg_std_needs`;
  forgetting produces a module whose body references something never
  injected, which surfaces as a confusing user-facing type error.
- **`pub` is enforced for functions, types and fields.** Functions:
  `check_visible` in `typecheck/infer.plum`. Types:
  `check_type_visible` in `typecheck/context.plum`, from annotations
  (`resolve_named`), construction (literal, variant) and destructuring
  (both pattern forms). Fields: `check_field_visible`, from reads,
  literals, named patterns, POSITIONAL patterns and nested update.
  Miss one and the hole is silent, so add a `typecheck_corpus/` fixture
  for any new path. The positional pattern is the easy one to forget —
  it names no field and reaches every one.
- **Marking a `pub struct`'s fields is not automatic.** `Map`, `Set`
  and `MapEntry` keep private fields on purpose; every other prelude
  and compiler struct had its fields opened when this landed. If you
  add a `pub struct` meant to cross a module boundary, its fields need
  `pub` too. `bootstrap/gen-shims` also emits a struct declaration —
  keep it in step or `check-shims` fails.
- **Type IDENTITY is still the bare name.** `ITStruct(name, args)`
  carries no module, so two modules cannot share a type name. The
  visibility lookup prefers the viewer's own module precisely so that a
  duplicate name cannot make a module think its own type is private —
  but the type the checker then uses is still whichever the flat lookup
  finds. `lsp-smoke` has a fixture with two `struct P` and is the thing
  that catches regressions here.
- **`resolve_ann` does not check; `resolve_ann_seen` does.** The
  wrapper passes `ANY_MODULE` so context building is exempt. If you add
  a site where user source names a type, call the `_seen` form with the
  right viewer, or it silently will not be checked.
- **A new cross-module call needs `pub` on the callee.** The compiler
  had zero violations when this landed, so a failure here is almost
  certainly a genuinely missing `pub` rather than a false positive.
- **The prelude is its own module, `parser.PRELUDE_MODULE()`.** A new
  prelude function that user code should reach needs `pub`, or it will
  read as unbound from outside. That includes anything the PARSER
  generates a call to — see `__contract_require`.
- **`cg_mangle` sanitizes every non-identifier character, not just
  `.`.** Do not narrow it back: module names come from directory names,
  and a directory with a dash in it produced invalid LLVM before this.
- **A type's identity is its QUALIFIED name**, via
  `parser.qualify_type` — the one rule, used by the checker and the
  backend. Root-module types stay bare; builtins (`Array`, `Task`,
  `Sender`, `Receiver`, `Box`) have no module and stay bare too, which
  is why `name == "Array"` tests still work.
- **`find_struct`/`find_enum` take a QUALIFIED name;
  `best_struct`/`best_enum` take a bare one** and resolve it the way
  the viewer should see it. Passing the wrong form does not error, it
  returns `None` — which for a visibility check means it silently
  passes. Check both forms when a lookup is on a checking path.
- **`ity_namespace` must return the BARE name.** An associated function
  is declared `let Circle.area (..)` whatever module it lives in.
