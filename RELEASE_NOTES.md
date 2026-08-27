Plum is a small, statically typed, compiled language.

One change, and it makes a sentence in the README true that had not
been true since modules were added.

## `pub` is enforced

```plum
// shapes/circle.plum
pub let area (c: Circle): Float = 3.14159 * c.radius * c.radius
let helper (c: Circle): Float = c.radius * 99.0     // no `pub`
```

```plum
use shapes;
shapes.area(c)      // fine
shapes.helper(c)    // rejected
```

```
shapes.helper is private to module `shapes`. Add `pub` to its
declaration to use it from outside
```

`pub` was parsed from the beginning and read by nothing, so
*everything* was public. That was not a missing feature so much as a
documented guarantee that did not hold — the README has promised
"everything is private by default" the whole time.

It became load-bearing in 0.0.9, which shipped four standard-library
modules whose internals were all reachable: `Time.plum__floor_div`,
`Http.http_parse_headers`, `Os.join_args_acc`. Every one callable, and
each would eventually have acquired a caller.

### It costs almost nothing to adopt

Enforcing it broke nothing in the compiler: seven modules, 16,000
lines, and not one cross-module call to an unexported name. If your
code is similar, this release is invisible. If it is not, the error
names the module and the fix.

Crossing a module boundary requires naming the module, so that is the
only place the rule applies. A call inside one module is not a
crossing, however the item was declared.

## What `pub` still does NOT do

`pub` on a `struct`, an `enum`, or a struct field is accepted and
ignored, because **types are not scoped to modules at all**. A `struct
Secret` declared in `shapes/` resolves as a bare `Secret` from any file
in the program, with or without `pub`.

That is a real gap and it is stated plainly rather than left to be
discovered. A function is identified by `(module, name)`; a type is
identified by its bare name, threaded through the checker,
monomorphization and symbol mangling. Scoping types means changing what
identifies a type everywhere those reach — mechanical, but wide enough
to deserve its own release. Until then, read `pub` on a type as
documentation of intent.

The compiler declares 159 types across seven modules and no two share a
name, so the flat namespace has been kept unique by hand. That is what
makes the eventual change safe rather than a renaming exercise.
