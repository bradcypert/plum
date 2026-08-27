Plum is a small, statically typed, compiled language.

Everything is private by default, and `pub` is what opts it out. That
sentence has been in the README since modules were added and has not
been true until now — `pub` was parsed and read by nothing, so
everything was public.

It now holds for functions, types and struct fields.

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

Enforcing it broke almost nothing in the compiler: seven modules,
16,000 lines, and not one cross-module call to an unexported FUNCTION
— the discipline had been kept by hand all along. Types needed six
`pub`s added to the prelude, which had been getting away with it
because the prelude shares a module with root-level code. If your code
is similar, this release is close to invisible; if it is not, the error
names the module and the fix.

Crossing a module boundary requires naming the module, so that is the
only place the rule applies. A call inside one module is not a
crossing, however the item was declared.

## Types too

```plum
// shapes/s.plum
struct Secret { n: Int }
pub let make (): Secret = Secret { n: 7 }
```

```plum
use shapes;
let s = shapes.make();         // fine   — the VALUE may cross
let t: Secret = shapes.make(); // rejected — the NAME may not
```

`pub struct` and `pub enum` gate the three ways a type can be named
from outside: an annotation, a literal or variant constructor, and a
pattern.

A private type that escapes through a `pub` function is **opaque**
outside its module — holdable and passable, but not takeable-apart,
because taking it apart means naming it. That is the same shape a
handle type has, and it is deliberate: the alternative, rejecting the
signature outright, forbids a pattern that is useful.

The prelude's own types (`Map`, `Set`, `JsonValue`, `JsonEntry`,
`MapEntry`, `ParseResult`) were declared without `pub` and are now
marked properly. They were always reachable from root-level code; what
changed is that a real module can see them too.

## And fields

```plum
// counter/c.plum
pub struct Counter { pub label: String, n: Int }
pub let start (label: String): Counter = Counter { label: label, n: 0 }
pub let count (c: Counter): Int = c.n
```

```plum
use counter;
let c = counter.start("hits");
c.label   // fine
c.n       // rejected
```

Fields are private independently of their struct, so a visible type can
keep its representation to itself. Reads, literals, named patterns,
positional patterns (`Counter(label, n)` names every field) and nested
update all check.

**A struct with any private field cannot be constructed from outside
its module**, because a literal has to name every field. That is the
point, not a side effect — it makes a constructor function the only way
in.

## What this does and does not close

The case for field privacy was concrete: `Map.buckets` was public API,
so Plum's bucket count and hashing strategy were observable and
therefore frozen.

That is now true from a module of your own:

```
field Map.buckets is private to the root module
```

and still NOT true from a root-level file, because the prelude shares
the root module with your own top-level files. Same module, no check.
Closing it properly means giving the prelude a module of its own, which
changes every prelude symbol's mangled name — sequenced as its own
piece of work rather than smuggled in here. `Map`, `Set` and `MapEntry`
keep private fields already, so the protection lands the moment it
does.

## What `pub` still does not cover

**Two modules cannot declare the same type name.** A type's identity in
the checker is its bare name, so the type namespace is flat. Visibility
prefers your own module's declaration — you will never be told your own
type is private — but genuinely duplicated names still resolve to one
of them arbitrarily. Giving a type a module-qualified identity is a
wider change and is its own release.

## Upgrading

Most code will not notice. In the compiler itself the fallout was
mechanical and entirely in one direction: 131 fields on cross-module
structs, plus 33 in the prelude, needed the `pub` their use already
assumed. If a struct of yours is used from another module, its fields
need `pub`; the error names the field and the module.
