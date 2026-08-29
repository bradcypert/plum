Plum is a small, statically typed, compiled language.

Two changes. `select` works, so a task can wait on several channels at
once. And channels and tasks now clean up after themselves, which they
never have: this is the first release where nothing in the test corpus
is allowed to leak.

## `select` waits on several channels at once

```plum
let got = select {
    n = numbers => n,
    s = words   => s.len(),
};
```

`select` blocks until one of its channels has a value, binds it, and
runs that arm. Without it you had to pick one channel and wait on it —
and if the message arrived on the other one, you sat there.

Arms are tried in the order you wrote them, so if two channels are ready
at the same moment the earlier arm wins. A channel that is always busy
can therefore starve one written below it.

It really blocks. It does not wake up periodically to check, so there is
no polling cost while it waits and no delay once a message lands.

An empty `select {}` is rejected. Go allows it as a way to block
forever; nothing else in Plum spells "hang", so here it is an error.

## Channels and tasks clean up after themselves

Before this release, every `channel[T]()` leaked its queue and every
task you never joined leaked its handle and result. Not a slow leak — a
permanent one, for the life of the program.

The cause was that `Sender`, `Receiver` and `Task` were plain integers
underneath. An integer has no lifetime, so nothing could run when one
went out of scope, and nothing could free what it pointed at. They are
proper values now: when the last reference to a channel end goes, the
queue is freed, and anything still sitting unread in it is released too.

The test corpus used to carry one fixture excused from leak checking.
It leaked 608 bytes, then 288 after a related fix, and now leaks
nothing. There are no excused fixtures left.

### Dropping a task without joining detaches it

```plum
let t = spawn { work() };
// t goes out of scope, never joined
```

The thread keeps running and its result is discarded — the same
fire-and-forget behaviour `spawn` always had, except that now nothing
leaks. The alternative, blocking at the end of the scope until the task
finished, would have meant `{ spawn { forever() }; }` never exiting,
with nothing at the call site saying so.

### Joining twice is now an error instead of undefined

```plum
let a = t.join();
let b = t.join();   // panic: task already joined
```

This was previously prevented by `.join()` consuming the task, which
only held while a `Task` was an integer nobody could copy. Now that it
is a real value, the check moved somewhere it can actually be enforced.

## Upgrading

**One thing stops compiling.** `.to_string()` on a `Task`, `Sender` or
`Receiver` is now an error. It used to print the raw address of the
underlying handle, which was never meaningful and never stable, and it
only "worked" because these were integers.

It is reported by `plum build`, not by `plum check` — the same as
`.to_string()` on a tuple, a `Ref` or a closure, which have always been
refused at code generation rather than by the type checker. If your CI
runs `check` alone it will not see this one.

Everything else is source-compatible. Programs that used channels or
spawned tasks will use less memory without being changed.
