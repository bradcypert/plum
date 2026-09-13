Plum is a small, statically typed, compiled language.

Two reviews ran over the codebase. This is what they found.

## An absolute dependency path never worked

```
Package {
    name: "myapp",
    deps: [ Dep { name: "parsec", path: "/opt/plum/parsec" } ],
}
```

That reported a dependency missing at `<your project>/opt/plum/parsec`,
a path you never wrote. It works now, and the way it broke is worth
recording.

`Path.join` does not let an absolute second segment win. Its rule is
Go's: `join("a", "/b")` is `a/b`. That is deliberate, and `join_all`'s
documentation says so clearly, because the alternative silently lets
user input escape the directory a caller meant to stay inside.

`Path.join`'s **own** documentation, twenty lines below, said the
opposite: "**An absolute `b` wins**: joining `/etc` onto `/home/x` gives
`/etc`". The package loader was written against that sentence, comment
and all.

So the fix is in both places. Fixing only the code would have left the
sentence that caused it sitting there for the next caller.

## Also fixed

- **An empty dependency path was silently skipped, and the build
  succeeded.** `path: ""` cleans to the project's own directory, which is
  already in the cycle-visited set. The only symptom was `unbound
  variable` at the `use`, an error pointing at your source rather than at
  the manifest line that was wrong. It is now rejected in the manifest.
- **`to_string` was documented as `let to_string (): String`**, a
  nullary free function that does not exist. It is a method on every
  value, written `x.to_string()`, and the reference now says so. This
  shipped in 0.0.29 alongside the change that documented the builtins in
  the first place.
- **Escaped quotes reached the site as backslashes.** `\"absolute\"`
  was visible in the `Int.abs` documentation on plumlang.org and in
  language-server hover.

## Seven harnesses that could not fail

Not user-visible, and the reason it is in these notes anyway is that
everything above was found by review rather than by a test.

The correctness of this compiler rests on 28 harnesses. Seven of them
could not fail:

- `check-doc-names` checked **zero** names in eight of ten documents. Its
  fixture named five of the ten standard-library modules, so every
  `Json.*` and `Terminal.*` name in the documentation was unverifiable,
  and fabricated ones passed under "all real".
- `fmt-check`'s safety property, that formatting never changes a
  program's tokens, ran only on files the formatter had not touched. It
  was checked on no input where the formatter acts.
- `self-test` and `property-check` passed on a run that discovered zero
  tests. Both had comments naming that exact hazard.
- `example-sweep` and the tutorial checker never read a program's exit
  status, so one that printed the right answer and then died was
  reported as working.
- `check-builtins` stopped reading at the first blank line.

Plus four harnesses that compared output through shell command
substitution, which strips trailing newlines from both sides, so a
change in trailing whitespace passed 161 corpus fixtures unnoticed.

Every fix is proven by fault injection rather than by argument: each one
was demonstrated to fail on a deliberately broken input before being
trusted.

**`bootstrap/cli-smoke`** is new, and covers what nothing ran at all:
`plum new` (whose scaffold embeds a test, and which is the first command
anyone types), `plum doc` on an ordinary project directory rather than
the standard library, `dump-tokens` and `dump-ast`.

The pattern behind all of it: a harness written alongside a feature
tends to share that feature's blind spot. Two harnesses each kept a
private copy of a list the compiler derives, which is the same mistake
the compiler's own source has a comment about having fixed.
