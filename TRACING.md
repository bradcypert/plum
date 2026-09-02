# Stack traces and tail calls

How `plum build --trace` came to keep tail-call elimination, and what
was measured along the way.

**The short version.** Traces and tail-call elimination are not
incompatible. Three ways of building a shadow call stack each defeat the
optimiser, for two distinct reasons; a fourth — paying the frame pop on
each PATH, and popping *before* a self-call in tail position rather than
after it returns — does not. That fourth one is what ships. Sections 1–5
are the investigation that found it, section 6 the design and its
measurements.

**Status.** Implemented. `--trace` builds run deep tail recursion in
constant stack space, and `bootstrap/check-build-modes` asserts it along
with the two trace shapes that a plausible-looking but unbalanced shadow
stack would get wrong.

---

## 1. What the trace is

A shadow call stack. Each Plum function records its own name on entry
and removes it on the way out; when the program dies, the runtime prints
what is left.

The runtime half is `native_stdlib/io_shim.c`:

```c
static const char *plum_frame_names[PLUM_TRACE_CAP];
static long long plum_depth = 0;

void plum_frame_push(const char *name) { ... plum_depth++; }
void plum_frame_pop(void)              { if (plum_depth > 0) plum_depth--; }
void plum_trace(void)                  { /* innermost first, to stderr */ }
```

The emitting half is `bootstrap/self_host/codegen/codegen.plum`. The
push is one line in `cg_fn`, right after `entry:`:

```plum
let cg_frame_push (sym: String) (shown: String): String =
    if DEBUG_FRAMES.get() && cg_frame_visible(shown) {
        "  call void @plum_frame_push(ptr @.fname_".concat(sym).concat(")\n")
    } else { "" }
```

**Where the matching pop goes is the whole subject of this document.**
The obvious answer — once, before the single `ret` a Plum function has —
is what the rest of sections 3 to 5 measure, and it costs tail-call
elimination. What ships instead pays the pop on each PATH through the
function (section 6), so `cg_fn` emits no pop at all and `cg_expr_t`
emits them.

`DEBUG_FRAMES` is set from `--trace` by `wants_trace` in
`bootstrap/self_host/main.plum`. A build without it emits none of these
calls, so the depth stays zero and `plum_trace` prints nothing.

## 2. The claim under test

Plum's README promises that a tail-recursive function runs in constant
stack space. The test program:

```plum
let count (n: Int) (acc: Int): Int = if n == 0 { acc } else { count(n - 1, acc + n) }
let main (): Unit = println(count(3000000, 0).to_string())
```

Three million is chosen to be far past any plausible stack.

## 3. Where the guarantee actually comes from

Not from the compiler. **This backend emits no `musttail`** —
`grep -r musttail bootstrap/self_host/` finds one hit, and it is a
comment in `main.plum` saying so. Constant stack space is a property of
the optimisation level. The README claimed otherwise ("compiled to a
real LLVM `musttail` call") until this was measured:

| flags (all with `-g`) | `count(3000000, 0)` |
|---|---|
| `-O0` | **segfault** |
| `-Og` | returns |
| `-O1` | returns |
| `-O2` | returns |

This is why the debug build is `-Og` and not `-O0`: making debug the
default with `-O0` would have put a stack overflow into every default
build.

## 4. What the optimiser does, and what stopped it

*Historical: every measurement in this section and the next describes
the pop-before-`ret` placement, which is no longer what the compiler
emits. Section 6 has the current numbers.*

The emitted IR does **not** contain a tail call, with or without
tracing. `plum_count` ends:

```llvm
L23:
  %t25 = call i64 @plum_count(i64 %t12, i64 %t20)
  store i64 %t25, ptr %t5
  br label %L8
L8:
  %t26 = load i64, ptr %t5
  ret i64 %t26
```

The recursion becomes a loop only because LLVM's tail-recursion
elimination runs over it. That is the transformation `--trace` blocks,
and the two outputs say so directly. Compiling each with `-Og` and
looking at `@plum_count`:

| | `tailrecurse` block | remaining self-calls |
|---|---|---|
| without `--trace` | present (5 refs) | **0** |
| with `--trace` | absent | **1** |

Without tracing the function becomes a loop:

```llvm
define i64 @plum_count(i64 %p0, i64 %p1) local_unnamed_addr {
entry:
  br label %tailrecurse
tailrecurse:
  %p0.tr = phi i64 [ %p0, %entry ], [ %t12, %L23 ]
  %p1.tr = phi i64 [ %p1, %entry ], [ %t20, %L23 ]
```

With tracing it stays a call chain:

```llvm
  %t25 = tail call i64 @plum_count(i64 %t12, i64 %t20)
  br label %L8
L8:                                     ; and here: call void @plum_frame_pop()
```

Note the `tail` marker on that call. It is not a promise of
elimination — in LLVM `tail` only asserts the callee does not access the
caller's stack frame. The transformation that gives constant stack space
is the one that did not happen.

**The reason is the pop.** Tail-recursion elimination requires that
nothing observable happen between the recursive call and the return of
its value. `call void @plum_frame_pop()` is a side-effecting call, so
the recursion cannot be rewritten as a loop.

## 5. Two ways round it, both measured, both failed

### 5a. Reclaim dead frames by stack address, so nothing pops

If a frame is never popped, nothing sits between the call and the
return. Dead frames get reclaimed lazily on the next push: the stack
grows down, so any recorded frame at or above the new one has already
returned. A tail call reuses its caller's frame and therefore its
address, so it replaces that entry — which is what a trace *should*
show, since the optimiser really did replace it.

Each function passed an `alloca` as its frame marker:

```llvm
  %frame.mark = alloca i8
  call void @plum_frame_push(ptr @.fname_plum_count, ptr %frame.mark)
```

**Result: still a segfault at three million.** An `alloca` handed to
another function *escapes*: the callee might retain a pointer into the
caller's frame, so LLVM will not reuse that frame. The same optimisation
refusing for a different reason.

### 5b. Ask for the caller's frame address from inside the runtime

Nothing escapes if the Plum function passes no address at all and the
runtime asks instead:

```c
const char *mark = (const char *)__builtin_frame_address(1);
```

**Result: tail calls survived** — `count(3000000, 0)` returned
`4500001500000`. But the traces were wrong, showing only the innermost
frame.

The premise was false. A six-line C probe, built exactly as a debug Plum
program is (`-Og -g -fno-inline -fno-omit-frame-pointer`):

```c
__attribute__((noinline)) static void probe(const char *who) {
    printf("%-8s caller frame = %p\n", who, __builtin_frame_address(1));
}
```

```
main     caller frame = 0x7ffdd39572b0
outer    caller frame = 0x7ffdd39572a0
middle   caller frame = 0x7ffdd39572a0     <- equal to outer's
deepest  caller frame = 0x7ffdd39572b0     <- back up to main's
```

The addresses are not monotonic in call depth, so "frames at or above
this one are dead" discards live frames. That is what produced a trace
containing only `deepest`.

## 6. What works, now measured: one pop per path

Pop **before** a call in tail position, rather than before the `ret` —
the plan as first written here — is correct in spirit and was wrong as
stated, in a way only running it showed. This section replaces it with
the version that measures clean.

### 6a. The naive version leaves a stale frame

"Pop before the tail call" says nothing about the pop that already sits
before the `ret`. Keeping it double-pops the call path and still blocks
the optimiser (the second pop is between the call and the `ret`, which
is the original problem). So the first attempt removed it and moved the
pop in front of the self-call, by hand-editing `@plum_count`'s IR.

Result at `-Og`: `count(3000000, 0)` **returned** — tail-recursion
elimination came back — but a later out-of-bounds in a different
function printed:

```
stack trace:
  at boom
  at count      <- returned long ago
  at main
```

The recursion's *final* iteration takes the value-exit branch, which now
reaches `ret` without ever popping. One push is stranded per completed
recursion, and it sits in every later trace.

### 6b. Why balance is even possible: where TRE puts the loop

The worry that killed confidence in this design was the loop form. If
LLVM turns the recursion into a loop, the push (in `entry:`) runs once
while a pop inside the loop runs three million times — the shadow stack
would drift by the whole depth. The measured `-Og` output says the
worry is unfounded:

```llvm
entry:
  br label %tailrecurse
tailrecurse:
  %p0.tr = phi i64 [ %p0, %entry ], [ %t16, %L27 ]
  %p1.tr = phi i64 [ %p1, %entry ], [ %t24, %L27 ]
  tail call void @plum_frame_push(ptr nonnull @.fname_plum_count)
  ...
L27:
  tail call void @plum_frame_pop()
  br label %tailrecurse
```

**The push is inside the loop.** Tail-recursion elimination splits only
the static `alloca`s into the new entry block; every other instruction
of the old entry — the push is the first of them — lands in the loop
body. So each iteration pushes at the top and pops on the back edge,
and the depth oscillates instead of drifting. This is a property of how
the pass is constructed (it hoists allocas, nothing else), not luck
about this one function.

### 6c. The rule

Every path through a traced function owes exactly one pop, paid on the
path itself rather than at the shared `ret`:

* a **self-call in tail position** pops *before* the call;
* **every other tail expression** — a literal, a call to some other
  function, an `if` arm that is not itself a call — pops *after* its
  value is computed, in its own branch, before the jump to the merge
  block;
* the function's epilogue pops **nothing**.

Balance then holds in both worlds. Under real recursion every entry
pushes once and every exit path pops once. Under the loop, 6b's shape
makes push and pop per-iteration partners, and the exit leaf's pop
undoes the final push.

Measured, same three-million-deep `count` at `-Og` with tracing:

| | result |
|---|---|
| `count(3000000, 0)` then a later out-of-bounds | returns `4500001500000`; trace is `boom, main` — **no stale frame** |
| abort at the *base* of the recursion | trace is `count, main` — the chain collapsed to one frame |

The collapsed trace is the honest one: the optimiser really did replace
those frames, and the shadow stack now says so. The same rule applied
to a self-call the optimiser *cannot* eliminate (one with releases
after it, say) still balances — each frame pops before it calls — and
shows the same collapsed trace, so "a self-tail-call never appears as
its own caller" is the defined semantics, not an artifact of what LLVM
chose to do.

A call in tail position to a *different* function keeps the leaf rule:
call first, pop after. The callee dying therefore still shows this
frame — `middle` stays in `deepest`'s trace, which
`bootstrap/check-build-modes` asserts. That placement blocks
cross-function sibling-call elimination under `--trace`, exactly as the
current pop-before-`ret` does; the README promises constant stack for
tail *recursion*, and that promise this design keeps.

### 6d. What implementing it touches

All in `bootstrap/self_host/codegen/codegen.plum`; the runtime is
finished.

1. **Thread tail position without touching every call site.** Rename
   the real emitter to take one more parameter — `cg_expr_t(p, env, n,
   e, tail)` — and let `cg_expr` become a one-line wrapper passing
   `false`. Existing call sites keep their meaning unedited. The flag
   is *forwarded* in exactly four places: `cg_fn` emits the body with
   `true`; `cg_if` passes its own flag to both branches; `cg_match` to
   each arm body; block emission to the final expression only.
   Statements, arguments, operands, scrutinees, loop bodies: all
   `false`, which the wrapper already gives them.

2. **Know the current function.** The self-call test is symbol
   equality: the callee's `cg_sym(module, name, targs)` — already
   computed at call emission — against the symbol of the function being
   emitted. Carry `cur_sym` (and `cur_frame`, whether this function
   pushed a frame at all, i.e. `DEBUG_FRAMES && cg_frame_visible`) the
   same way the per-function `subst` already travels in `CgProgram`.
   Symbol equality is the right test under monomorphization for free: a
   polymorphically-recursive call to a *different* instantiation is a
   different symbol, and genuinely is not a frame-replacing loop.

3. **Emit the pops.** In the call arm of `cg_expr_t`: when `tail &&
   cur_frame` and the symbol matches, prefix the call with
   `call void @plum_frame_pop()`. Everywhere else that `tail &&
   cur_frame` holds and the expression is not one of the three
   forwarding constructs, append the pop to the emitted code after the
   value. Aborting checks inside the expression (bounds, div-zero) sit
   before the appended pop, so their traces keep the frame.

4. **Delete the epilogue pop** in `cg_fn`. The pop now precedes the
   releases on every path (releases run at the merge; they only
   decrement and free, nothing in them can abort), which is worth the
   one comment.

A construct not yet taught to forward the flag fails **safe**: its
inner self-call emits as an ordinary call, the leaf pop lands after it,
and that one shape misses the optimisation — a missed loop, never an
unbalanced stack. Start with `if`/`match`/block and extend if a real
program shows a gap.

### 6e. Assertions to add while implementing

To `bootstrap/check-build-modes`:

* the three-million-deep recursion built **with** `--trace` returns —
  the headline;
* after a deep traced recursion *returns*, a later abort's trace names
  the aborting function and `main` and does **not** name the recursive
  function — this is the stale-frame regression from 6a;
* an abort at the base of the deep recursion shows exactly one frame
  for it — the collapse is deliberate and should be pinned;
* the existing `middle`/`deepest` fixture stays as-is — non-self tail
  calls must keep their frames.

### 6f. `musttail`, and the bug it turned up — now done

*Implemented 2026-09-02, and it stopped being optional. What follows is
the note as originally written; section 7 records what happened when it
was picked up.*


A fourth hand-edited variant made the self-tail-call a real
`musttail call` followed immediately by `ret`, with the pop before it.
It passed everything the per-path variant passed **and survived three
million deep at `-O0`**, because `musttail` is a guarantee at every
optimisation level, not a request. Balance needs no argument about
pass internals there: the jump re-enters through `entry:`, so the push
re-executes per logical call by construction.

That is the eventual answer to §3's uncomfortable truth that constant
stack space is currently a property of the optimisation level. It is
not the tracing fix, though: `musttail` demands the call be the
literal last instruction before `ret`, so the branch must emit its own
early `ret` (today every branch funnels to one `ret` through a merge
slot, and an `Emit` has no way to say "this path terminated"), and
nothing may follow the call — a function with pending releases cannot
use it until the releases are hoisted before the jump. Both are real
emission surgery, both apply to traced and untraced builds alike, and
neither is needed to ship `--trace`. Recorded here so the measurement
is not lost: the two designs compose, since `musttail` only tightens
the case the per-path rule already handles.

Two smaller options existed and remain rejected:

* **Traces on by default, no tail-call elimination in debug.** Breaks a
  documented promise in the mode most people run.
* **Skip frames for self-recursive functions.** Keeps both properties by
  omitting exactly the functions a deep-recursion trace is about.

### 6g. Picking up `musttail` found a segfault that had nothing to do with tracing

The note above treats `musttail` as an optional tightening. It was not:
sizing it up turned up a live bug in builds with no `--trace` anywhere
near them.

**Slot releases are the frame pop all over again.** Reference counting
emits a function's releases just before its single `ret`. For a
tail-recursive function with a heap-typed parameter or `let`, those
releases sit between the recursive call and the return of its value —
the identical position, for the identical reason, arriving from
reference counting instead of from tracing.

```plum
let go (n: Int) (s: String): Int = if n == 0 { s.len() } else { go(n - 1, s) }
```

Three million deep: **segfault, in the default build and in
`--release`.** The same function over `Int` had always been fine, which
is exactly why nothing caught it — every tail-recursion fixture in the
repo used `count(n, acc)` over two `Int`s, the one shape with nothing to
release. The README promised constant stack space without qualification
and had been wrong about it for as long as reference counting has
existed.

The fix is the section 6 rule taken one step further. A self-call in
tail position releases first and returns its result directly:

```llvm
  %t28 = ...                          ; argument, holding its own reference
  %t16_end.t0 = load ptr, ptr %t16    ; every slot released HERE
  call void @plum_rel_str(ptr %t16_end.t0)
  %s1_end.t0 = load ptr, ptr %s1
  call void @plum_rel_str(ptr %s1_end.t0)
  %t29 = musttail call i64 @plum_go(i64 %t23, ptr %t28)
  ret i64 %t29
Ltail0:                               ; dead: catches the merge the parent appends
```

Releasing first is safe because an argument takes its own reference
before the call, and `movable` nulls any slot it moves out of, which
every release tolerates. Nothing reads a slot afterwards — the next
instruction returns.

Measured across all three levels, which is what settled plain-call
versus `musttail`:

| | `-O0` | `-Og` | `-O2` |
|---|---|---|---|
| today's epilogue release | segfault | segfault | segfault |
| releases hoisted, plain call | segfault | returns | returns |
| releases hoisted, `musttail` | **returns** | **returns** | **returns** |

So `musttail` earns its place on evidence rather than tidiness: it is a
guarantee at every level, and it makes the README's promise a property
of the compiler rather than of the optimisation flags.

**Two things made this far smaller than section 6d feared.**

*The dead label.* `Emit` never gained a "this path returned" field.
`cg_if` and `cg_match` append a store-and-branch after a branch's code,
and instructions after a `ret` in one block are invalid IR — so the
tail-call site opens a fresh label for that appended code to land in,
unreachable and dropped by the optimiser. One line, instead of editing
all 139 places an `Emit` is built to serve the four that merge.

*Two passes instead of a predictor.* The releases to hoist are only
known once the body has been emitted — they are what it accumulated. So
`cg_fn` emits the body twice: once to discover them, once using them.
Predicting them from the typed tree was the alternative and was
rejected, because a predictor that drifted from what emission actually
produces would silently drop a release. The two passes must number
registers identically for the first pass's text to mean the same thing
in the second, so nothing in the second draws from the register counter
— site ids come from their own counter — and `cg_fn` **checks** the two
agree rather than trusting it, falling back to the un-hoisted body if
they ever do not.

**The corpus caught what the analysis missed.** The first working
version passed every hand-written test and leaked one cell per
iteration on two fixtures. `cg_match` drops its scrutinee temporary
*after the arms merge*, in `code` rather than in `releases` — and an arm
that returns early never reaches the merge. Releases were function-wide
and easy to reason about; this cleanup is per-enclosing-construct, and
reasoning about "what runs on the way out" had silently meant only the
first kind. Tail position now carries a `pending` string of exactly that
cleanup, accumulated innermost-first because an inner value may be
borrowed from an outer one.

That is the third time in this project that a design correct about
ownership was wrong about *where* the work was written down, and the
second time the corpus, not the reasoning, was what said so.

## 7. What is asserted, so this cannot drift

`bootstrap/check-build-modes` builds the three-million-deep recursion in
the default, `--release` **and** `--trace` builds, and fails if any
overflows — over two `Int`s, and separately over a **heap-typed
parameter with a heap `let`**, which is the shape section 6g found
segfaulting. It also builds a failing program with and without
`--trace` and asserts that the trace appears only with the flag, that it
names the failing function and its caller, and that **stdout is
byte-identical either way** — the abort message is compared exactly by
`bootstrap/abort_corpus`, so a trace must stay on stderr. Two shapes of
trace are pinned besides: no stale frame after a deep recursion
returns, and exactly one frame for a tail-recursive chain.

`bootstrap/exec_corpus/tail_call_heap` covers what that harness cannot:
it runs under ASan with leak checking, so it pins that returning early
still RELEASES everything, including a match scrutinee dropped after
the arms merge. Shallow on purpose — the stack-space claim belongs to
the harness, the ownership claim to the fixture.

Both the `-O0` regression and the heap-parameter segfault were caught by
that harness after the fact, which is the reason it exists.

## 8. A note on the measurements themselves

Three of the results above were wrong before they were right, and all
three were caught the same way — by a result contradicting an earlier
one.

* A check of whether Plum functions survive as symbols read a **stale
  binary** left in a scratch directory by an earlier probe, and reported
  that they did not. They do. That mistake nearly turned a
  one-linker-flag plan into a debug-information project.
* A comparison of optimisation levels passed `"-O0 -g"` as a **single
  shell argument**, so every build failed silently and the previous
  binary was re-run. The table said every level passed, including the
  one that crashes.
* The first version of the address-comparison rule had the comparison
  **backwards**, evicting callers instead of returned frames.
* The first cut of the per-path design (§6a) was the plan this document
  itself recommended, run verbatim — and it left a stale frame in every
  trace after a deep recursion returned. The document was the thing
  refuted that time.
* The first working version of section 6g's release hoist passed every
  hand-written test and **leaked one cell per iteration** on two corpus
  fixtures. The reasoning about "what runs on the way out" had accounted
  only for slot releases and not for a match's scrutinee drop, which is
  written down somewhere else entirely.
* The first attempt to prove the hoist memory-safe used a **string
  literal**, which is a static cell with `rc = -1` that every release
  skips. It demonstrated nothing about reference counting and looked
  like a pass. Redone with a heap-allocated value.
* The first *measurement* of the corrected per-path rule segfaulted,
  which briefly read as the design failing. The hand-edit that produced
  the IR had matched `%t29 ` (trailing space) where the file says
  `%t29,` (trailing comma), so a pop landed after the self-call too —
  the one placement the whole design exists to avoid. The segfault was
  the mechanism confirming itself, not refuting it.

The habit worth keeping is that each design here looked correct when
written and was refuted by running something: a three-million-deep
recursion, a diff of two `.ll` files, four printed pointers.
