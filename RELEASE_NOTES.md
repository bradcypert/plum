Plum is a small, statically typed, compiled language.

`use` binds a module's short name, and manifest examples in the
documentation are checked.

## Short names are back

0.0.34 made a dependency's modules package-qualified, which fixed
collisions and left `parsec.json.parse(..)` as the only spelling. A file
can now say what it is using:

```plum fragment
use parsec.json;

let main (): Unit = println(json.tag())
```

and rename it, which is how one file reaches two packages that both ship
the same module name:

```plum fragment
use parsec.json as pj;
use core.json as cj;
```

`use parsec;` still works and still gives `parsec.json.parse(..)`.

Three rules worth knowing:

- **An alias replaces the short name rather than adding one.** After
  `use parsec.json as pj;`, `pj` works and `json` does not.
- **Binding one name twice in a file is an error** naming both and
  telling you to use `as`. Letting the second win silently would
  reintroduce, one level down, exactly what package qualification
  removed.
- **A binding wins over a module of the same name, including one of
  yours.** If your project has a `json/` module and a file says
  `use parsec.json;`, then in that file `json` is the dependency's. The
  `use` is at the top of the file and says so, which is the rule Python
  follows for `from parsec import json`.

## What changed underneath

`use` was decorative for anything but a standard-library module. Every
module was loaded and addressable by its own name, so `use lexer;` was a
comment: delete it and nothing changed.

It binds into a file's scope now, because per-file scope is the whole
mechanism. One file saying `use parsec.json;` while another says
`use core.json;` is what makes two packages usable in one program, and
that only works if a binding belongs to a file rather than to a program.

**Nothing you have written needs to change.** Your own modules still
resolve with no `use` at all, and `use Os;` still means what it meant.

## Manifest examples are checked

Three `plum.pkg` examples in the documentation were tagged as Plum
fragments, which was a small lie: a manifest is not Plum and will never
compile as one, and that is exactly why nothing checked them. Meanwhile
the manifest format is the newest and fastest-moving thing documented,
having gained four fields in 0.0.34 alone.

They are tagged as manifests now and parsed and validated on every
documentation run, via a new command:

```sh
plum check-manifest plum.pkg
```

It deliberately does not resolve dependencies, which is why it exists
rather than reusing `plum check`. The manifests in the documentation
name packages that do not exist and never will; what they claim is that
the form is right, and that is what is checked.
