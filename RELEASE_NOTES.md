Plum is a small, statically typed, compiled language.

Two changes, and together they close the module system. Both were filed
as missing conveniences. Both were bugs, and the second one
miscompiled.

## Two modules may declare the same type name

```plum
// main.plum                    // inner/i.plum
pub struct P { pub x: Int }     pub struct P { pub y: String }

println(P { x: 42 }.x.to_string())
```

Before this release:

```
struct P has no field named x
```

**Adding a struct in a subdirectory broke an unrelated root file that
had never referenced it**, and the error blamed the file that had done
nothing wrong. Types were identified by their bare name, so whichever
declaration the lookup found first won — for everybody.

A type is now identified by the module that declared it. `shapes.Circle`
and `render.Circle` are different types, and a bare `Circle` means the
one declared where you wrote it: your own module first, then the root,
then the prelude. A root declaration **shadows** a prelude one of the
same name rather than being confused with it.

They also no longer silently unify, which was the quieter half of the
same bug — before, both were `ITStruct("P")`, so this type-checked and
then read a field that was not there:

```
let a: P = inner.make();
// let a: declared type P doesn't match value type inner.P (inner.P != P)
```

Nothing changes for code that does not reuse a name. Root-module type
names are unchanged, and a module appears in a diagnostic only when it
is the thing telling two types apart.

## Two enums may declare the same variant name

```plum
// main.plum                    // inner/shade.plum
enum Weight { Light, Heavy }    pub enum Shade { Light, Dark }

match inner.dim() { Light => "shade light", Dark => "shade dark" }
```

Before this release that was `Weight != inner.Shade`: a tag was looked
up by scanning every enum, and the scan stopped at the first one that
declared it, whatever the value being matched actually was.

A pattern now resolves against **the scrutinee's type**, which is
already known where the arm is written. `Light` matched on an
`inner.Shade` is that enum's `Light`, however many other enums declare
one.

### It was also a miscompile

```plum
enum Verdict { Ok, Bad }
println(match Bad { Ok => "ok", Bad => "bad" })
```

This type-checked and then crashed. The backend resolved variant tags
independently of the checker, by the same flat scan — so the two could
answer differently, and here they did: the checker meant `Verdict.Ok`
and the generated code meant the prelude's `Result.Ok`. The compiler's
IR now carries the enum the checker chose, and nothing downstream
resolves a tag a second time.

### Saying which one you mean

`Enum.Variant` works in expressions and in patterns:

```plum
let v = Verdict.Ok(7);
match r { Result.Ok(n) => n.to_string(), Err(e) => e }
```

A tag you write unqualified means, in order: the scrutinee's enum in a
pattern; then an enum declared in **your** module, so a local `Ok`
shadows the prelude's exactly as a local type name does; then the only
enum that declares it. If more than one still fits, that is now an
error rather than a guess:

```
`Light` is a variant of Weight and Shade -- write Weight.Light to say
which one you mean
```

## Upgrading

Nothing to do for either change.

If you were relying on two same-named types being interchangeable, they
are not — but that only ever worked by accident, and produced the wrong
field rather than an error.

If your module declares an enum with a variant named like a prelude one
(`Ok`, `Err`, `Some`, `None`), a bare use of that tag in that module now
means **yours**. Write `Result.Ok` or `Option.Some` for the prelude's.
Programs where two enums in the same module share a tag name and nothing
says which is meant are now rejected instead of silently compiled
against whichever was declared first.
