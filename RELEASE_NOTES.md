Plum is a small, statically typed, compiled language.

Child processes that do not block, stdin reads with a deadline, and the
rest of the numeric surface — trigonometry, logs, constants, and a
seeded generator you can replay.

## Child processes, without blocking

`Process.run` waits for the child to finish, which is right for a build
step and wrong for anything that has to stay responsive meanwhile.

```plum
use Process;
use Time;

let child = Process.start(opts)?;

match child.poll() {                       // never blocks
    Ok(None) => draw_spinner(),
    Ok(Some(res)) => finish(res),
    Err(e) => report(e),
}

child.wait_timeout(Time.seconds(5))        // None = still going, untouched
child.terminate()                          // SIGTERM
child.kill()                               // SIGKILL
child.pid()
```

`Child` is a handle, so **a child whose handle dies is killed and
reaped**. That is deliberate and worth knowing before you rely on the
other behaviour: once the value is gone nothing can poll the process,
wait for it, signal it, or read its output, so leaving it running would
leak something the program can no longer name — along with the temp
files it is still writing to.

Output is readable only once the child has exited, and then any number
of times. `wait` on a finished child returns the same result rather than
failing, so it is safe after a `poll` that already said `Some`.

It is `start`, not `spawn`, because `spawn` is a keyword.

**On Windows, `terminate` is `kill`** — there is no SIGTERM, so a child
that would have cleaned up on a polite request does not get the chance.

## Reading stdin with a deadline

```plum
use Os;
use Time;

match Os.read_stdin_line_timeout(Time.millis(200)) {
    Ok(Got(line)) => handle(line),
    Ok(Eof) => stop(),
    Ok(TimedOut) => redraw(),
    Err(e) => report(e),
}
```

Also `Os.read_stdin_timeout(max, duration)` for bytes.

Three outcomes, in a type with three names. `Option` was already spent:
`read_stdin_line` uses `None` for end of stream, and telling that from a
blank line is the distinction it exists for.

**The timeout bounds the whole call**, not just the wait for the first
byte — and a partial line survives it. Bytes already read stay buffered,
so a call that times out mid-line loses nothing and the next one
continues where it stopped. Bounding only the first byte is what most
such APIs do; it passes every test written against a terminal, where a
line arrives at once, and hangs past its deadline on a pipe.

## Trigonometry, logs, and constants

```plum
Float.sin(x)   Float.cos(x)   Float.tan(x)
Float.asin(x)  Float.acos(x)  Float.atan(x)
Float.atan2(y, x)
Float.log(x)   Float.log2(x)  Float.log10(x)  Float.exp(x)
Float.pi()     Float.tau()    Float.e()
Float.radians(deg)            Float.degrees(rad)
```

Angles are radians. `atan2` takes `(y, x)`, the order libm, Go, Python
and Java all use, and knows which quadrant the point is in — which
`atan(y / x)` cannot.

`examples/asteroids` now uses these instead of declaring `sin` and `cos`
in its own `extern "C"` block beside a hand-typed `3.14159265358979`.

## Seeded random numbers

`Float.random` reads a process-global generator seeded from the clock,
which is right for "different each run" and useless for a test or a
replay.

```plum
let r = Rng.from_seed(42);
let (r1, roll) = Rng.int_range(r, 1, 7);      // 1..6 — upper bound EXCLUDED
let (r2, f) = Rng.float(r1);                  // [0.0, 1.0)
let (r3, deck) = Rng.shuffle(r2, cards);
let (r4, pick) = Rng.choice(r3, options);     // Option[T]
```

Every call returns the next generator alongside the value rather than
mutating in place, so a generator is as ordinary a value as an `Int` —
and the same seed replays exactly, on every platform and every run. A
`Ref[Rng]` is the opt-in for in-place update.

`int_range` uses rejection sampling rather than a modulo, so small
ranges are not biased toward their low end. Any `Int` is a legal seed,
including 0 and negatives.

**Not for cryptography.** It is a statistical generator — good for
games, replays and tests — and its entire state is recoverable from two
outputs.

## A runtime declaration no longer breaks your `extern "C"`

If your program declared a C function the compiler's runtime also uses,
it did not link:

```
error: invalid redefinition of function 'sin'
```

This was always possible — `sqrt` and `pow` have been declared by the
runtime for a long time — but the trigonometry above would have made it
common, since `sin` and `cos` are exactly what a program declares for
itself. A duplicate declaration is now dropped rather than emitted, so
your `extern "C"` block can name whatever it needs to.
