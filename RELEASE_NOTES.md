Plum is a small, statically typed, compiled language.

0.0.10 made `pub` mean something for functions, types and fields, and
shipped with one caveat: it did not apply to the standard library,
because the prelude shared a module with your own top-level files. This
closes that.

## The prelude is a module

```plum
let m: Map[String, Int] = Map.from_arrays(["k"], [1]);

Map.len(m)     // fine — part of the interface
m.buckets      // rejected
```

```
field Map.buckets is private to the prelude.
Add `pub` to it to use it from the root module
```

Before this, `Map`'s bucket array was public API from any root-level
file — which meant Plum's bucket count and hashing strategy were
observable, and so effectively frozen. They are now implementation
details that can change.

**The module cannot be named.** There is no `use prelude;` and no
`prelude.println(..)`. Prelude names are reached exactly as before,
unqualified. The module exists so that what the prelude does not export
is genuinely unavailable rather than merely undocumented — a private
JSON helper or a `*_raw` runtime stub now reads as unbound, which is
what it is.

## Twenty-three functions gained the `pub` they always needed

Every `Map.*`, every `Set.*`, `json_stringify` and `json_parse` were
declared `let`. They worked for precisely the reason this release
removes, and they are now marked properly. If you use the standard
library, nothing changes.

`__contract_require`/`__contract_ensure` are the interesting pair: the
parser generates calls to them when it desugars `requires`/`ensures`,
and that generated code lands in your module, so they are public
whether or not anyone would write them by hand. Two harnesses caught
that on a fixture nobody had touched.

## A symbol-mangling bug that was never about the prelude

The prelude's module name reaches the symbol mangler, and the build
failed on invalid LLVM:

```
@g.plum_<prelude>_MAP_INITIAL_BUCKETS = global i64 0
```

`cg_mangle` replaced `.` with `_` and passed everything else through.
That was fine while every module name came from a directory that
happened to be a plain identifier — and it means **a project with a
directory like `my-mod/` would have emitted invalid IR**, with nothing
to catch it. It now sanitizes every character that is not a letter,
digit or `_`.

## Upgrading

Nothing to do unless you were reaching into the standard library's
internals, in which case the error names the field or function. The
public API is unchanged.
