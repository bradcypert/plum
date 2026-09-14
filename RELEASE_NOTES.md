Plum is a small, statically typed, compiled language.

Three Windows path bugs, and a diagnostic that sent you to fix something
already correct.

## Paths on Windows

Three bugs, all pre-existing, and one of them is the kind worth being
uncomfortable about.

| | was | is |
|---|---|---|
| `Path.dirname("C:\a\")` | `C:.` | `C:\` |
| `Path.basename("C:\")` | `C:` | `\` |
| `Path.clean("//srv/sh")` | `\srv\sh` | `\\srv\sh` |

The first two share a cause: the drive letter was included in the search
for the last separator, so the root at index 2 was not recognised as
one. `C:.` is the current directory on drive C, not its top.

**The third silently rewrote a network path into a local one.** A UNC
root is two separators; collapsing it to one produces a path that names
something else entirely. Rewriting a path to point somewhere else is
worse than refusing to handle it.

Nothing on Linux or macOS is affected. These are Windows-only, which is
exactly why they lasted: on a Unix machine that code was not merely
untested, it was **unreachable**.

## Path takes the convention as an argument

Every `Path` function now has an `_in` form:

```plum fragment
Path.clean_in(windows, p)          Path.basename_in(windows, p)
Path.join_in(windows, a, b)        Path.dirname_in(windows, p)
Path.is_absolute_in(windows, p)    Path.separator_in(windows)
```

plus `join_all_in`, `extension_in`, `stem_in` and `with_extension_in`.
The plain forms are unchanged and read the convention from the host, so
nothing you have written needs to change.

Two reasons this is worth ten new functions. The first is that it makes
the Windows behaviour reachable from any machine, which is how the bugs
above were found: they turned up on the first run of the first test.
The second is that it is genuinely useful. If you are emitting paths for
a platform other than the one you are running on, which this compiler
does when it cross-compiles, the `_in` forms are what to call.

The property that caught all three is one the module was already written
to satisfy, and which nothing had checked under the Windows convention:

```
join(dirname(p), basename(p)) == clean(p)
```

## A diagnostic that blamed the wrong thing

```plum fragment
use Os;
let main (): Unit = Os.exit(0)
```

```
error: `Os` has no `exit`. Did you mean `exit_with`?
```

It used to say `` `Os` is a standard library module; add `use Os;` to
this file ``, to a file whose first line is `use Os;`. Following that
advice added a duplicate import and produced the same error again.

A module-qualified name fails two ways and they now read differently:
the module is not imported, or the module is imported and has no such
name. Suggestions are restricted to public names, because the first
version offered a private runtime primitive.

## Every rejection is now pinned

`bootstrap/typecheck_corpus` has 57 fixtures. Two of them asserted what
the compiler actually said; the rest asserted only that it exited
non-zero, so they passed whether the message was right, misleading, or
attached to the wrong line.

All of them pin the message and its position now. That is not a
user-facing feature, and it is in these notes because it is the reason
the next bad diagnostic gets caught by a test rather than by somebody
hitting it.

## Also in this release

- The seed was refreshed. It bootstraps to a compiler identical to this
  one, as `bootstrap/check-seed` asserts.
