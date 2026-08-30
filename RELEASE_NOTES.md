Plum is a small, statically typed, compiled language.

Format-on-save, and four fixes — one of which was producing wrong code.

## Formatting in your editor

`plum lsp` now advertises `documentFormattingProvider`, so any editor
that speaks LSP can format a Plum file, including on save. The rules are
the ones `plum fmt` already had.

It formats the buffer you are looking at, not the file on disk. That
distinction is the whole of the work: an editor asks to format what it
is showing, which when you save is a document the file system has not
seen yet. Formatting the saved copy and replacing the whole file with
the result would revert whatever you had just typed.

## `Ref[T]` can be written down

```plum
let r: Ref[Int] = ref(1)
```

`ref(v)` has always produced a `Ref`, and the type has always existed
inside the compiler — but it could not be NAMED. Annotating one failed
with `unknown type: Ref` while the identical line without the annotation
compiled.

## A global's declared type is used

```plum
let NOTHING: Option[String] = None
```

This produced **wrong code**. A reference to a global re-inferred the
global's body at every mention and took the type from that, ignoring the
annotation. For a value that determines its own type — `let N: Int = 3`
— that is fine. For `None`, or `Map.new(())`, or `[]`, it is not: the
type parameter stays unknown, and the compiler fell back to a default.
Reading a `String` out of that `Option` loaded a boolean-shaped slot, and
the build failed with an LLVM error naming a register number.

Locals were always fine, which is what made it puzzling: the same two
lines inside a function worked.

Annotated globals of `Option`, `Map`, `Array` and `Ref` types are all
covered by a new fixture.

## `plum check` checks globals

`check` skipped every top-level `let` with no parameters, so

```plum
let M: Int = "hello"
```

passed `plum check` and failed `plum build`. A checker that misses what
the compiler rejects is worse than no checker, because it is the one
people run in an editor.

## A `pub` global can be reached from another module

```plum
// inner/table.plum          // main.plum
pub let LIMIT: Int = 3       use inner;
                             println(inner.LIMIT.to_string())
```

This did not work by any spelling. `inner.LIMIT` reported `unbound
variable: inner`, and a bare `LIMIT` reported `unbound variant/function`
— so a `pub` global was visible to its own module and to nobody else,
and `pub` on one meant nothing at all.

A global without `pub` is now private to its module, like a function or
a type, and says so.

An unqualified `LIMIT` from another module is still unbound, on purpose:
that is how functions already behave, and a module's names should not
leak into whoever imported it.

## Upgrading

Nothing that worked before stops working.

The `pub` rule for globals cannot break existing code, because reaching
a global across a module boundary did not compile at all until now.

One thing that used to be accepted is now rejected: a global whose
declared type does not match its value, like `let M: Int = "hello"`.
That was never doing what it said.
