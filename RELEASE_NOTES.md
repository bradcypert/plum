Plum is a small, statically typed, compiled language.

This release adds a formatter.

## `plum fmt`

```sh
plum fmt --check src/     # list what needs formatting, non-zero exit
plum fmt --write src/     # format in place
plum fmt one.plum         # to stdout
```

A file or a directory; a directory is walked for every `.plum` file
under it. `--check` is the CI shape and `--write` is the everyday one.

### What it decides

Five things, and nothing else:

- the line a statement, item or match arm begins on is indented four
  spaces per enclosing block;
- a closing brace sits one level out from what it closes;
- a run of comment lines is indented with the code it documents;
- a run of blank lines collapses to one;
- a comma has no space before it and exactly one space after.

Every other character in the file is passed through untouched.

### Where the rules came from

They were measured, not chosen. Across the 243 `.plum` files in the
compiler's own repository, indentation was already a multiple of four
everywhere, with no tabs and no trailing whitespace; there were 1,586
runs of a single blank line against two runs of a double; and of 20,585
commas the only one not already followed by a space was inside an array
literal. The formatter encodes what the code already did.

The test of that is that `plum fmt` leaves all 243 of those files
byte-identical.

### What it will not do

It does not move a line belonging to a construct with no braces of its
own — the `=` continuation of a long `let`, a `|>` chain broken across
lines, the arguments of a multi-line call. Those are alignment choices
inside an expression, and the compiler does not yet record where an
expression begins, so a formatter guessing at them would be moving code
whose shape it cannot see. It leaves them exactly as written.

It also does not reflow: nothing is joined onto one line or split
across two. Line breaks are yours.

### It will not corrupt a file

Before writing anything, `--write` re-lexes its own output and compares
the token sequence against the original's. Whitespace and comments are
exactly what lies between tokens, so two files whose tokens agree in
order differ only in formatting. If a rule ever changed the tokens,
`fmt` refuses to write and exits non-zero.

Writes go to a temporary file beside the original and are renamed into
place, which is atomic: the path is either the old file or the new one,
never half of each.

## `Os.rename_file`

```plum
Os.rename_file(from, to)   // Result[Unit, String]
```

Atomic within a filesystem, and an error rather than a silent copy
across two. Added for the formatter's own writes, and useful anywhere
a file has to be replaced without a window where it is incomplete.

## Upgrading

Nothing breaks. `plum fmt` is new, `Os.rename_file` is new, and no
existing behaviour changed.

If you adopt the formatter on an existing codebase, `plum fmt --check`
first: it tells you what would move without touching anything.
