# Plum Grammar (EBNF)

This is the formal grammar to implement `plum-syntax`'s lexer and parser
against. It's derived directly from the decisions recorded in
`DESIGN.md` and exercised in `examples/overview.plum` — if this document
and `DESIGN.md` ever disagree, that's a bug in one of them, not a
license to pick either.

Notation: `::=` defines a rule, `|` is alternation, `[ x ]` is optional,
`{ x }` is zero-or-more, `( x )` is grouping, `"x"` is a literal token.

A short "Known ambiguities and implementation notes" section closes this
document — a few spots are deliberately left slightly loose because
they're genuinely parser-implementation decisions, not open language
design questions.

## Lexical grammar

```
letter        ::= "a".."z" | "A".."Z"
digit         ::= "0".."9"
ident_start   ::= letter | "_"
ident_continue::= letter | digit | "_"
Identifier    ::= ident_start { ident_continue }
```

`Identifier` is a single lexical category — the lexer does not split
identifiers into separate "type" and "value" token kinds. Capitalization
is consulted by the parser/resolver, not the lexer, to disambiguate:
`Shape.Circle` (path/variant access) from `p.x` (field access), and
`Thing[T]` (generic instantiation) from `arr[i]` (indexing). See
DESIGN.md's "Generics and type parameter syntax" and "Surface syntax"
sections.

```
IntLiteral    ::= digit { digit | "_" }
FloatLiteral  ::= digit { digit | "_" } "." digit { digit | "_" }
StringLiteral ::= '"' { StringChar } '"'
StringChar    ::= any character except '"' or newline, or an escape sequence
BoolLiteral   ::= "true" | "false"
Literal       ::= IntLiteral | FloatLiteral | StringLiteral | BoolLiteral
Comment       ::= "//" { any character except newline }
```

Comments and whitespace are insignificant except as token separators (no
significant indentation — see DESIGN.md, braces were chosen deliberately
over an offside rule).

Keywords (reserved, cannot be used as `Identifier`): `let`, `mut`, `fn`,
`struct`, `enum`, `match`, `if`, `else`, `for`, `in`, `pub`, `use`,
`mod`, `extern`, `unsafe`, `spawn`, `true`, `false`.

## Program structure

```
Program   ::= { Item }
Item      ::= [ "pub" ] ItemKind
ItemKind  ::= LetDef | StructDecl | EnumDecl | ExternBlock | UseDecl
```

There is no `mod` declaration in this grammar — module boundaries are
directory-shaped, not syntactic (see DESIGN.md's "Module system"). Every
`.plum` file in a directory parses as `Program` and contributes to the
same module.

### Let definitions (values and functions, unified)

```
LetDef        ::= "let" Identifier [ GenericParams ] { Param }
                   [ ":" Type ] "=" Expr
Param         ::= Identifier
                 | "(" Pattern [ ":" Type ] ")"
GenericParams ::= "[" GenericParam { "," GenericParam } "]"
GenericParam  ::= Identifier [ ":" Bound ]
Bound         ::= Identifier { "+" Identifier }
```

Zero `Param`s makes this a plain value binding (`let x = 5`); one or
more makes it a function definition (`let sum n acc = ...`). This is
deliberate, not a simplification for the grammar's sake — see DESIGN.md,
"functions are just values" is a load-bearing ML idea, not a decoration.

### Struct and enum declarations

```
StructDecl  ::= "struct" Identifier [ GenericParams ] "{" [ FieldList ] "}"
FieldList   ::= Field { "," Field } [ "," ]
Field       ::= [ "pub" ] Identifier ":" Type

EnumDecl    ::= "enum" Identifier [ GenericParams ] "{" [ VariantList ] "}"
VariantList ::= Variant { "," Variant } [ "," ]
Variant     ::= Identifier [ "(" Type { "," Type } ")" ]
```

### Extern blocks

```
ExternBlock     ::= "extern" StringLiteral "{" { ExternFn } "}"
ExternFn        ::= "fn" Identifier "(" [ ExternParamList ] ")"
                     [ "->" Type ] ";"
ExternParamList ::= ExternParam { "," ExternParam }
ExternParam     ::= Identifier ":" Type
```

`fn` and mandatory type annotations survive here on purpose — see
DESIGN.md, this is a foreign declaration with no body, so there's
nothing for inference to work from.

### Use declarations

```
UseDecl ::= "use" Path ";"
Path    ::= Identifier { "." Identifier }
```

One rule covers both `use shapes;` (qualify-by-default, the common
case) and `use shapes.Circle;` (the bare-import escape hatch) — the
grammar doesn't distinguish them, that's a semantic question of what the
final path segment resolves to, not a syntactic one.

## Types

```
Type        ::= PathType [ GenericArgs ]
              | TupleOrFunctionType

PathType    ::= Identifier { "." Identifier }
GenericArgs ::= "[" Type { "," Type } "]"

TupleOrFunctionType ::= "(" [ Type { "," Type } [ "," ] ] ")" [ "->" Type ]
```

`TupleOrFunctionType` covers three cases by what follows the closing
paren and what's inside: `(Int)` is just `Int` (grouping, not a tuple —
matches the value-level rule), `(Int,)` is a one-element tuple type,
`(Int, Float)` is a two-element tuple type, and any of those followed by
`"->" Type` is a function type (e.g. `(Int, Int) -> Int`), used for
explicitly annotating a parameter that itself takes a closure.

## Expressions

Precedence, loosest-binding first (matches DESIGN.md's "Operator
precedence and pipe semantics" exactly):

```
Expr        ::= PipeExpr

PipeExpr    ::= OrExpr { "|>" OrExpr }
OrExpr      ::= AndExpr { "||" AndExpr }
AndExpr     ::= CompareExpr { "&&" CompareExpr }
CompareExpr ::= RangeExpr [ CompareOp RangeExpr ]
CompareOp   ::= "==" | "!=" | "<" | ">" | "<=" | ">="
RangeExpr   ::= AddExpr [ ".." AddExpr ]
AddExpr     ::= MulExpr { ( "+" | "-" ) MulExpr }
MulExpr     ::= UnaryExpr { ( "*" | "/" | "%" ) UnaryExpr }
UnaryExpr   ::= [ "-" | "!" ] PostfixExpr
PostfixExpr ::= PrimaryExpr { Postfix }
Postfix     ::= "." Identifier
              | GenericArgs
              | Arguments
              | "[" Expr "]"
Arguments   ::= "(" [ Expr { "," Expr } ] ")"
```

`CompareExpr` and `RangeExpr` each allow **at most one** operator
application (non-associative) — `a < b < c` and `a..b..c` are both
grammar errors, not parsed with some implied associativity. See
DESIGN.md for why (chained comparisons silently meaning something other
than the mathematical reading is a documented footgun).

`Postfix` repeats to handle chains like `Ref.new(start)` (`.new` then
`(start)`, two postfix steps) and `channel[Int]()` (`[Int]` then `()`,
two postfix steps) uniformly — one rule, no special-casing based on
what's being chased.

```
PrimaryExpr ::= Literal
              | Identifier
              | "(" [ Expr { "," Expr } [ "," ] ] ")"
              | Block
              | IfExpr
              | MatchExpr
              | ForExpr
              | ClosureExpr
              | UnsafeExpr
              | SpawnExpr
              | StructLiteral
```

The parenthesized form follows the same tuple-vs-grouping rule as
types: `(x)` is `x`, `(x,)` is a one-element tuple, `(x, y)` is a
two-element tuple.

```
IfExpr        ::= "if" Expr Block [ "else" ( Block | IfExpr ) ]
ForExpr       ::= "for" Pattern "in" Expr Block
UnsafeExpr    ::= "unsafe" Block
SpawnExpr     ::= "spawn" Block

ClosureExpr   ::= "|" [ ClosureParam { "," ClosureParam } ] "|" Expr
ClosureParam  ::= Identifier [ ":" Type ]

MatchExpr     ::= "match" Expr "{" MatchArm { "," MatchArm } [ "," ] "}"
MatchArm      ::= Pattern [ "if" Expr ] "=>" Expr

StructLiteral ::= PathType "{" [ FieldInitList ] "}"
FieldInitList ::= FieldInit { "," FieldInit } [ "," ] [ ".." Expr ]
                | ".." Expr
FieldInit     ::= Identifier [ ":" Expr ]
```

`FieldInit`'s shorthand form (`Identifier` with no `: Expr`) means
`field: field` — binds a field from a same-named variable in scope, the
same shorthand Rust's struct literals allow.

### Blocks (the statement/expression rule)

```
Block      ::= "{" { BlockItem ";" } [ Expr ] "}"
BlockItem  ::= LetStmt | AssignStmt | Expr
LetStmt    ::= "let" [ "mut" ] Pattern [ ":" Type ] "=" Expr
AssignStmt ::= Identifier "=" Expr
```

Every `BlockItem` requires its trailing `;` — **no exemption** for
`if`/`match`/`for`/block-shaped expressions used as statements, unlike
Rust. The final, unterminated `Expr` (if present) is the block's value;
its absence (or a trailing `;` on the last item) makes the block's
value `Unit`. See DESIGN.md's "Block statement/expression rule" for the
reasoning.

`AssignStmt` was missing from an earlier draft of this document even
though DESIGN.md always discussed assignment (`total = total + i`) as a
statement, not a general expression (see "Operator precedence and pipe
semantics" — assignment is deliberately excluded from the expression
grammar). Its target is restricted to a plain `Identifier`, not a
general `Pattern` or arbitrary lvalue path — this matches `let mut`
only ever binding a plain identifier (see "Local mutability"), and it
means `Ref[T]` mutation stays exclusively through `.get()`/`.set()`/
`.update()`, never through assignment syntax. `p.x = 5` is not valid
Plum; there is no field-assignment form.

`let mut` is only meaningful with a plain `Identifier` pattern in
practice (a "mutable slot" doesn't make sense for a destructured
pattern) — the grammar doesn't forbid `let mut (a, b) = ...` outright,
but it should be rejected during type-checking/lowering, not parsing;
noted here so it isn't forgotten as an implementation detail.

## Patterns

```
Pattern        ::= OrPattern
OrPattern      ::= PrimaryPattern { "|" PrimaryPattern }
PrimaryPattern ::= Literal
                  | "_"
                  | Identifier
                  | PathType "(" [ Pattern { "," Pattern } ] ")"
                  | PathType "{" [ FieldPatternList ] "}"
                  | "(" [ Pattern { "," Pattern } [ "," ] ] ")"

FieldPatternList ::= FieldPattern { "," FieldPattern } [ "," [ ".." ] ]
                    | ".."
FieldPattern     ::= Identifier [ ":" Pattern ]
```

`FieldPattern`'s shorthand form (`Identifier` with no `: Pattern`) binds
a variable of the same name — `Point { x, y }` binds `x` and `y`
directly. The trailing `..` in a struct pattern means "ignore remaining
fields," the third distinct job `..` does in this grammar (struct
update, ranges, pattern-rest) — see DESIGN.md, each is unambiguous by
grammatical position.

`Option[T]`/`Result[T, E]` patterns (`Some(x)`, `None`, `Ok(v)`,
`Err(e)`) fall directly out of the enum-variant pattern production
(`PathType "(" ... ")"`) — nothing type-specific is needed for them.

## Known ambiguities and implementation notes

A few things are deliberately left to the parser implementation rather
than fully pinned in this document, because they're genuinely
implementation decisions, not open language-design questions:

- **Struct-literal vs. block ambiguity in `if`/`match` scrutinee
  position.** `if Point { x: 1.0, y: 2.0 } { ... }` is ambiguous between
  "a struct literal used as the condition" and "the condition is `Point`,
  followed by the `if`'s body block." Rust has this exact problem and
  solves it by disallowing bare struct literals in condition position
  (requiring parens: `if (Point { ... }) { ... }`) — Plum should adopt
  the same restriction, but it isn't spelled out as a separate
  restricted-expression grammar above for readability. Flagged here so
  it isn't lost.
- **`Param`'s parenthesization vs. `Pattern`'s own tuple-parenthesization**
  can overlap syntactically (a parenthesized destructuring param like
  `(Point { x, y })` vs. a tuple pattern `(a, b)` used directly as a
  param). The rules as written are consistent, but working through the
  exact recursive-descent handling (avoiding double-wrapping) is
  parser-implementation work, not a design question.
- **Generic-instantiation vs. indexing** (`Thing[T]` vs. `arr[i]`) and
  **generic instantiation vs. plain indexing-of-a-capitalized-value**
  are resolved by capitalization at parse/resolve time, per DESIGN.md —
  this grammar states both use the `Postfix ::= "[" Expr "]"` /
  `GenericArgs` productions without encoding the disambiguation rule
  itself, since it's a semantic (name-resolution-time) rule, not a
  syntactic one.
- **Array/list literal syntax is intentionally absent** from this
  grammar — not yet decided (see DESIGN.md's open questions). Adding it
  later should slot into `PrimaryExpr` without disturbing anything else
  here.
- **`f (a) (b)` parses successfully but is a semantic trap, not a
  two-argument call.** Because `Postfix` repeats (see "Expressions"
  above), a call immediately followed by another parenthesized group is
  parsed as two *chained* single-argument calls — `f(a)` first, then
  the parenthesized `(b)` applied again to *that result* — the same
  shape currying would produce, even though Plum doesn't have currying
  (see DESIGN.md's "Surface syntax," deliberately deferred). This isn't
  a parse error; it silently produces the wrong arity, and would only
  surface as a type error once type-checking exists. Found by parsing
  `examples/overview.plum`'s original `sum (n - 1) (acc + n)`, which
  looked like an OCaml-style two-argument call but actually meant
  something else entirely — the example was corrected to
  `sum(n - 1, acc + n)`. The only real fix here is discipline (always
  use one comma-separated argument list per call) until/unless currying
  is revisited; a parser-level warning for "call result immediately
  called again" is possible future tooling, not a grammar change.
