; Plum syntax highlighting queries — capture names follow the
; conventions nvim-treesitter and Neovim's own base `queries/highlights
; .scm`-consuming themes expect (`@keyword`, `@function`, `@type`, …),
; so these work with an existing theme/colorscheme with no extra setup.

; ---- Keywords ----
; `mod` is deliberately absent — GRAMMAR.md lists it as LEXICALLY
; reserved (can't be used as an `Identifier`) but it never appears as
; an actual token in any production ("There is no `mod` declaration in
; this grammar" — module boundaries are directory-shaped, not
; syntactic). `grammar.js` never uses the literal string "mod" either,
; so tree-sitter's own query compiler rejects it here as an unknown
; node type — confirmed by hand, not assumed.
[
  "let"
  "mut"
  "fn"
  "struct"
  "enum"
  "match"
  "if"
  "else"
  "for"
  "in"
  "pub"
  "use"
  "extern"
  "unsafe"
  "spawn"
  "select"
] @keyword

; ---- Literals ----
(int_literal) @number
(float_literal) @number.float
(bool_literal) @boolean
(string_literal) @string
(escape_sequence) @string.escape
(comment) @comment

; ---- Operators and punctuation ----
[
  "="
  "=="
  "!="
  "<"
  ">"
  "<="
  ">="
  "+"
  "-"
  "*"
  "/"
  "%"
  "&&"
  "||"
  "!"
  "|>"
  ".."
  "->"
  "=>"
] @operator

[ "." "," ":" ";" ] @punctuation.delimiter
[ "(" ")" "{" "}" "[" "]" ] @punctuation.bracket
"_" @variable.builtin

; ---- Items ----
(let_def name: (identifier) @function)
(let_def assoc_name: (identifier) @function.method)
(struct_decl name: (identifier) @type)
(enum_decl name: (identifier) @type)
(variant name: (identifier) @constructor)
(field name: (identifier) @property)
(extern_fn name: (identifier) @function)
(extern_param name: (identifier) @variable.parameter)

; A `let` with zero `param`s (and no `.assoc_name`/generics) is a
; plain value binding, not a function — `name` is immediately
; (anchored, no intervening `param` sibling) followed by `:` or `=` —
; re-tagged `@variable` so e.g. `let PI = 3.14159` doesn't highlight
; like a call. Listed AFTER the general `@function` pattern above and
; matching the SAME node: relies on "later pattern wins for the same
; capture," the standard technique nvim-treesitter's own stock
; `highlights.scm` files use everywhere for this exact kind of
; structural refinement.
(let_def name: (identifier) @variable . [":" "="])

; ---- Types ----
(path_type (identifier) @type)
(generic_param (identifier) @type.parameter)
(bound (identifier) @type)

; ---- Expressions ----
(param (identifier) @variable.parameter)
(closure_param (identifier) @variable.parameter)
; `let_stmt`'s `pattern` field is a `pattern` NODE (a real named rule,
; not inlined even for the common single-identifier case — unlike
; `_primary_pattern`, whose leading underscore DOES inline it), so the
; identifier itself is one level further down.
(let_stmt pattern: (pattern (identifier) @variable))
(assign_stmt target: (identifier) @variable)
(field_init name: (identifier) @property)
(field_pattern name: (identifier) @property)

; `a.b` field access vs. `a.b()` method call — distinguished by
; whether the SAME postfix chain continues into an `arguments` node
; right after, same "last matching pattern wins" trick used above for
; value vs. function `let`s.
(postfix_expr field: (identifier) @property)
((postfix_expr field: (identifier) @function.method) . (arguments))

; A capitalized identifier used as a bare expression atom almost
; always names a type/variant/module (`Point`, `Shape.Circle`, `Ref`,
; `Some`) — Plum's own capitalization convention (see GRAMMAR.md's
; lexical-grammar section), reused here the same way the real parser
; and resolver use it.
((identifier) @constructor
  (#match? @constructor "^[A-Z]"))
(identifier) @variable

; ---- Patterns ----
(variant_pattern path: (path) @constructor)
(struct_pattern path: (path) @type)
(struct_literal path: (path) @type)

; ---- Built-in type names ----
; Not compiler-recognized keywords (ordinary identifiers, per
; GRAMMAR.md's lexical grammar), but conventionally capitalized
; built-ins worth distinguishing from an arbitrary user type.
((identifier) @type.builtin
  (#any-of? @type.builtin "Int" "Float" "Bool" "String" "Unit" "Array" "Ref" "Option" "Result"))
