Plum is a small, statically typed, compiled language.

A `Terminal` module, and cleanup that a crash can no longer skip.

## Cleanup now runs on panic

A `handle` releases when its value dies — on a normal return, at the end
of a block, when an `Err` is returned. Plum has no early `return`, so
those are every path a function has except one: a **panic**, which does
not unwind.

That was not untidiness for the operating system to sweep up:

```
started true
parent dies here
parent exit=1
child survived the parent's panic: YES
```

A `Process.Child` outlived its parent and kept working, contradicting
its own documented contract. The OS reclaims file descriptors and
memory. It does not kill your grandchildren, remove your lock file, or
take your terminal out of raw mode.

Live handles are now released on the way out — on a panic, on
`Os.exit_with`, and on returning from `main` — in LIFO order, the same
order a scope releases in. Nothing to opt into: if you have a `handle`,
this already applies to it.

## `Terminal`

```plum
use Terminal;

Terminal.is_tty(Stdout)          // per stream: Stdin, Stdout, Stderr
Terminal.size()                  // Result[Size, String] — { cols, rows }
Terminal.write(text)             // no newline, no flush
Terminal.flush()

let raw    = Terminal.enter_raw()?;
let screen = Terminal.enter_alt_screen()?;
let cursor = Terminal.hide_cursor()?;
```

The three modes are handles, so **the terminal is restored on every exit
path, including a crash**. That is the machinery a terminal program
otherwise writes in C, and it is the first thing in the language to
depend on the panic cleanup above.

They release LIFO, so the cursor returns before the screen is given up
and the screen before the mode is restored — the right order, arranged
by nobody. Each works without the others.

Entering the same mode twice is **counted, not refused**: the first
entry saves the state, the last release restores it, so a library and
its caller can both ask without knowing about each other.

Three things worth knowing before writing a terminal program:

- **`is_tty` is asked per stream**, because the answer differs. A
  program in a pipeline routinely has a terminal on stderr and a pipe on
  stdout, and asking about the wrong one is how progress bars end up in
  log files.
- **Raw mode turns Ctrl+C into a byte** (0x03) rather than a signal.
  That is what raw mode means everywhere, and here it is also what keeps
  cleanup working — a default SIGINT ends a process *without* running
  any cleanup, so signals left enabled would hand back a broken
  terminal. Your program is responsible for noticing 0x03 and quitting.
- **Every mode change refuses when there is no terminal.** Escape
  sequences written into a pipe are not invisible, they are corruption.
- **`size` polls.** POSIX signals a resize with `SIGWINCH`, Plum has no
  signal handling, and comparing the size between iterations of an event
  loop costs one syscall against a redraw. It works the same way on
  Windows, which has no such signal at all.

Semantic key events — `Key.Up`, `Ctrl+C` — are deliberately **not** here.
That part is pure Plum with no C in it, which makes it the piece most
easily copied into a program and the one where a frozen API would hurt
most; it is tracked separately.

Windows needs Windows 10 or later, so the console can deliver the same
escape sequences a POSIX terminal does.

## Also

`bootstrap/mem-check`'s ceilings are per platform now. macOS costs about
1.5x Linux for the same work — likely 16 KB pages against 4 KB, so RSS
is not a comparable quantity — and it varies by 20 MB between runs where
Linux does not vary at all. A single shared ceiling was measuring which
runner a job landed on.
