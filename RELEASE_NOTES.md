Plum is a small, statically typed, compiled language.

`Type.func(x)` and `x.func()` are the same call, in both directions.

## Method calls work both ways

Plum has one calling rule: `Type.func(receiver, args)` and
`receiver.func(args)` are the same call. It is why `xs.map(f)` works —
there is no separate method system, only `Array.map`.

The rule did not hold. Nine ordinary things did not compile:

```plum
m.len()          // Map      error: .len(): Map[String, Int] != Array[T0]
s.len()          // Set      error: .len(): Set[Int] != Array[T0]
a.concat(b)      // Array    error: .concat() receiver: Array[Int] != String
m.remove(k)      // Map      error: .remove() requires an Array

Array.len(xs)              // error: unbound variant/function: Array
String.concat(a, b)        // error: unbound variant/function: String
Int.to_float(n)            // error: unbound variant/function: Int
Ref.get(r)                 // error: unbound variant/function: Ref
Array.set(xs, i, v)        // error: unbound variant/function: Array
```

All of them work now, and each error above named a type the author had
not written — `m.len()` on a `Map` complained about `Array`.

## `Array.push(xs, x)` type-checked and would not compile

Worse than an error message. `plum check` said `ok`, and then the build
failed:

```
self-hosted codegen: prelude function Array.push has no implementation
in this backend's runtime yet
```

`plum check` is the one that runs in an editor, so this was a program
your editor called fine and your build refused — reachable by writing
about the most ordinary thing in the language.

## Your editor knows about `map` now

Typing `xs.` offered twenty-three methods on an array and **not** `map`,
`filter`, `fold`, `len` or `push` — the five you reach for first.
Typing `Array.` offered one of them.

Separately, `use Os;` put nothing in the completion list at all. No
standard-library module — `Os`, `Time`, `Net`, `Http` — was ever
offered, in any project.

Both are fixed, and names are now offered in the form you have to write
them: `Os.read_file`, not a bare `read_file` you cannot call.

## Upgrading

**Nothing that worked before stops working.** This release only accepts
more programs than the last one; no spelling that compiled under 0.0.17
is rejected under 0.0.18.

If you worked around any of the above — writing `Map.len(m)` because
`m.len()` was refused, or `xs.push(x)` because `Array.push(xs, x)` broke
the build — those workarounds are still correct. They are simply no
longer necessary.

## Under the hood

The fix is the rule rather than a table of exceptions: a namespaced call
to a builtin method is inferred AS the method call. The alternative —
giving each builtin its own second signature — is exactly what
`Array.push` already had, and `Array.push` is the one that compiled to
nothing.

Two new checks keep it that way. `bootstrap/check-builtins` requires
every method the type checker recognises to be offered by completion and
exercised as a dot call in a fixture that runs; it caught a gap
introduced while writing this release. `bootstrap/check-doc-names`
verifies every standard-library name the documentation mentions in prose
actually exists.

The README's code examples are compiled and run now, the same as the
tutorial's already were. That found four wrong claims in it, including
one example that could not compile and one describing a feature the
language does not have.
