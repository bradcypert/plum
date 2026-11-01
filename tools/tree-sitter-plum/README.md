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
  production (plus array literals).
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

Float literals accept decimal mantissas, integer mantissas with an
exponent, and decimal mantissas with an exponent (`1.5`, `1e3`,
`1.5E-3`). Exponent signs are optional; a digit is required after the
sign, and digit runs retain the compiler's underscore conventions.
Numeric range checking remains the compiler's responsibility.

Strings expose literal `string_content`, `escape_sequence`, and
`interpolation` nodes. Each interpolation's `expression` field reuses
the ordinary expression grammar, so nested calls, strings, and numeric
literals receive their normal highlights. Quotes and literal segments
receive string captures rather than covering the entire expression
subtree with a string capture. Escaped dollars and bare dollars remain
literal content, and multiline text keeps its whitespace.

The editor parses interpolation expressions structurally; it does not
duplicate the compiler's restricted raw-text boundary scan described in
`GRAMMAR.md`. Some braced or nested expressions can therefore form an
editor syntax tree while still being rejected by the compiler. A hole
must contain one complete expression; this also rejects trailing tokens
that the compiler currently ignores (tracked in issue #170).

Handle declarations follow `HandleDecl`, including `pub` and an optional
trailing comma. `handle` introduces a declaration only at item start and
remains an identifier elsewhere, as does `on_drop` outside a declaration.
Cleanup signature and resource ownership checks remain the compiler's
responsibility; editor parsing does not create or release native handles.

Compile-time builtin calls follow `BuiltinCall ::= BuiltinName Arguments`.
The sigil and name form one token (`@embed_file`); arguments and postfix
chains use the ordinary expression rules. Unknown names, wrong arity,
and nonliteral paths are left to compiler validation. Editor parsing
does not read embedded files or store their contents in the syntax tree.

`select` arms follow `GRAMMAR.md`: `pattern = receiver => body`,
`else => body`, and `duration => body`. All keep the `select_arm` node
and `body` field; receive arms have `pattern` and `channel` fields,
and timeout arms have a `duration` field. Pattern/expression prefixes
are disambiguated by `=` versus `=>`. Semantic restrictions (such as
requiring a channel arm and forbidding `else` together with a timeout)
remain the compiler's responsibility.

Patterns accept a separate `-` token followed by an integer literal,
including inside alternatives, tuples, variants, and struct fields.
Whitespace and comments may separate the sign from the integer. A minus
before a float or binding is rejected in patterns; ordinary unary minus
expressions are unchanged.

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
