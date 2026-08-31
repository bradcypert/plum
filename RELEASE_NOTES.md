Plum is a small, statically typed, compiled language.

The formatter learned where expressions begin, the editor gained expand-selection and real ranges, and a module qualifier now means something everywhere a type can be named.

## Lines the formatter used to leave alone

`plum fmt` could indent anything a brace positioned — statements, match
arms, closing braces — and nothing else. A `let` body written on the
next line, a `|>` chain broken across lines, a multi-line call's
arguments: those were left exactly as typed, because nothing in the
compiler recorded where an expression BEGAN, so there was no way to name
the construct such a line belonged to.

The parser records that now, and one rule covers all three shapes: a
line that continues a construct begun on an earlier line is indented
four past the line that construct started on.

```plum
let describe (n: Int): String =        let bounds (xs: Array[String]) =
    if n > 10 {                            xs
        "big"                                  |> Array.map(_, String.trim)
    } else {                                   |> Array.join(_, ", ")
        "small"
    }
```

Both links of that chain sit inside the one expression that began at
`xs`, so both are indented from that line — not from each other.

## Three spacing rules

```plum
n*2 + (n-1)/3        becomes    n * 2 + (n - 1) / 3
P { x:n, y:n+1 }     becomes    P { x: n, y: n + 1 }
xs[ 0 ]              becomes    xs[0]
```

A binary operator gets one space on each side; a unary minus does not,
so `-n` and `max(-5, -3)` are left alone. A colon gets one space after
it — but the space BEFORE a colon is left as written, because `require
b != 0 : "message"` uses a colon as a separator and nothing at the token
level distinguishes that from `x: Int`.

None of this reaches inside a string literal. The rules work on the gaps
between tokens, and a gap is by definition not inside one, so
`",".concat("a,b")` is untouched.

## What the formatter will not do

It still does not reflow: no line is ever joined or split.

It also declines to place four kinds of line, each because this
repository either said nothing about them or disagreed with itself:

* a line that begins inside a multi-line string literal, where the
  leading spaces are part of your program;
* a line whose enclosing construct starts partway along the line above,
  where the only guide is a column somebody typed;
* a hand-aligned line sitting deeper than the grid answer — aligning
  under an opening bracket or a condition is what that always is;
* an item's `require`, `ensure` and `=` header lines.

## Every rule here was measured

The rules are not imported from another language. Each was counted
against the 249 `.plum` files in the compiler's own repository before it
was written, and the test of that is that formatting all of them changes
nothing.

The continuation rule came from 1,484 continuation lines, of which 936
are indented four past the line above and 526 are level with it — 98.5%,
and both fall out of the single sentence above. Where files disagreed,
the majority decided: arm bodies written on their own line run 26 to 1
in favour of indenting, and operator continuation lines 632 to 16.

The colon rule was corrected by that process. "No space before a colon"
is the obvious rule and it rewrote every contract in the repository, so
it is not the rule.

## Expand-selection, and ranges that are actually ranges

Put the cursor on a name and press expand-selection; the selection grows
one construct at a time:

```plum
let total (xs: Array[Int]): Int = Array.fold(xs, 0, |a, v| a + v * 2)
```

From `v`: `v` → `v * 2` → `a + v * 2` → `|a, v| a + v * 2` →
`Array.fold(xs, 0, |a, v| a + v * 2)` → the whole declaration. The chain
is the nesting of expressions around the cursor, so this is the request
that could not be answered at all until expression spans existed.

Every range the server produced before this release was **zero width** --
`start` and `end` the same point. That is legal LSP, and it is why a
diagnostic underlined nothing, a jump-to-definition selected nothing,
and a hover highlighted nothing. Now a diagnostic underlines the
construct it is about, hover highlights the name it is describing, and
go-to-definition lands on the name with it selected rather than on the
`pub` in front of it.

## The language server no longer dies while you type

Asking to format a buffer with a syntax error **killed the server**. It
exited mid-session, after printing a compiler error onto the JSON-RPC
stream, and every later request went unanswered until the editor
restarted it. Format-on-save runs while you are still typing, so a
half-written expression is the normal case rather than an exotic one.

The cause is a rule this server already followed everywhere else and had
not followed here: the parser reports errors by aborting the process, so
anything that parses has to run in a child. Diagnostics always did.
Formatting did not. It does now, and an unformattable buffer answers
"no edits" -- which is what a formatter should say about a file it
cannot read.

## A module qualifier is part of the name

When two modules declare the same enum, neither `On` nor `Shade.On` can
say which one you mean, and the checker said so:

```
`On` is a variant of light.Shade and dark.Shade
-- write light.Shade.On to say which one you mean
```

That advice named a spelling the compiler rejected. In an expression it
reported `unbound variable: light`; in a pattern it failed to parse at
the second dot. Both work now, and so does the same shape for a struct:

```plum
use light;
use dark;

let describe (s: light.Shade): String = match s {
    light.Shade.On  => "on",
    light.Shade.Off => "off",
}

let lamp (): dark.Lamp = dark.Lamp { watts: 60 }
```

Naming the wrong module reports the mismatch rather than being quietly
corrected to the right one.

## Two types that shared a name could be confused

The above turned out to be the smaller half. **A type annotation
resolved only its LAST segment**, so `light.Shade` and `dark.Shade` were
the same lookup -- for the bare name `Shade` -- and whichever module the
scan reached first answered for both. This compiled, and should never
have:

```plum
let mix (s: dark.Shade): light.Shade = s
```

Struct literals had it too: `dark.P { y: "no" }` built a `light.P` and
then reported `struct light.P has no field named y` -- an error naming a
module the author had not written, about a struct that does have that
field. Struct patterns were the same.

The qualifier was being parsed, carried around, and dropped at the point
of lookup. It is used now, in annotations, expressions and patterns.

This is the sort of gap that survives because the program that would
notice is one nobody writes: it needs two modules, the same type name in
both, and a value crossing between them.

## Upgrading

The formatting rules change no semantics.

The module-qualifier fixes do reject some programs that used to compile,
and every one of them was wrong: a program that assigned one module's
type to another module's same-named type, or built a struct literal with
a module qualifier that was being ignored. If your code compiles, it was
never relying on this — the two types have to have the same name and
different modules for the confusion to arise at all.

`plum fmt --check` will now report files that were passing before, if
they contain any of the shapes above. `plum fmt --write` updates them.

**It still cannot corrupt a file.** Before writing, `--write` re-lexes
its own output and compares the token sequence to the original's; a rule
that changed the tokens is refused rather than written. That check
earned its keep this release: the first version of the continuation rule
indented a line that began inside a multi-line string literal, which
would have added spaces to the string's contents, and the check caught
it on two files.
