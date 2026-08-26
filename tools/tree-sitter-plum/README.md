# tree-sitter-plum

A [tree-sitter](https://tree-sitter.github.io/) grammar for
[Plum](../../GRAMMAR.md), used for editor syntax highlighting and
indentation — **not** a second implementation of the language for any
correctness-bearing purpose. See `grammar.js`'s own header comment for
the full scope note and the (few, documented) places this grammar makes
a different implementation choice than the compiler's own
hand-written parser (`bootstrap/self_host/parser/`) for a genuinely
context-free-grammar-unresolvable ambiguity.

## Layout

- `grammar.js` — the grammar itself, one rule per `GRAMMAR.md`
  production (plus two real syntax forms `GRAMMAR.md` doesn't document
  yet: array literals and `select`).
- `queries/highlights.scm` — highlight query, using nvim-treesitter's
  standard capture names (`@keyword`, `@function`, `@type`, …), so it
  works with any existing theme.
- `test/corpus/` — regression tests (`tree-sitter test`).

## Rebuilding

```sh
npm install
npx tree-sitter generate   # regenerate src/parser.c from grammar.js
npx tree-sitter test       # run test/corpus/
npx tree-sitter parse <file.plum>   # eyeball a real parse tree
```

## Editor integration

See [`editors/nvim`](../../editors/nvim) at the repo root for a
ready-to-use Neovim runtime bundle (this grammar isn't published to
nvim-treesitter's own registry, so it needs manual registration —
that directory handles it). Its `queries/plum/highlights.scm` is a
symlink to this directory's `queries/highlights.scm` — one source of
truth; re-run that directory's `tree-sitter build` step after any
grammar change here to pick it up.

## Known simplifications

Both documented inline in `grammar.js` at the relevant rule:

- **`Thing[T]` vs. `arr[i]`** (generic instantiation vs. indexing) —
  the real parser disambiguates by identifier capitalization at parse
  time (`Parser::next_bracket_is_generic`); this grammar can't run
  that check, so both collapse into one neutral `bracket_postfix` node.
- **Struct literals in `if`/`match` condition position** (`if Point {
  x: 1.0 } { ... }`) — the real parser threads a `no_struct_literal`
  flag through condition/scrutinee parsing; this grammar instead gives
  `struct_literal` a negative dynamic precedence, biasing tree-sitter's
  GLR resolution toward "it's the block," the far more common real-
  world shape (the same technique tree-sitter-rust and tree-sitter-go
  use for the identical ambiguity).
