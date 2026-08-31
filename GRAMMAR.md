# Plum Grammar (EBNF)

This is the formal grammar the compiler's own lexer and parser
(`bootstrap/self_host/lexer/` and `bootstrap/self_host/parser/`) are
implemented against, and it is checked by
`bootstrap/bootstrap-check` — 102 fixtures in `bootstrap/corpus/`, each
with a recorded token stream and AST. It's derived directly from the decisions recorded in
`DESIGN.md` and exercised by `TUTORIAL.md`, every snippet of which is
compiled and run by `bootstrap/doc-check` — if this document and
`DESIGN.md` ever disagree, that's a bug in one of them, not a license to
pick either.

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
StringLiteral ::= '"' { StringChar | Interpolation } '"'
StringChar    ::= any character except '"', '$', or newline, or an escape sequence
Interpolation ::= "${" InterpExprSource "}"
BoolLiteral   ::= "true" | "false"
Literal       ::= IntLiteral | FloatLiteral | StringLiteral | BoolLiteral
Comment       ::= "//" { any character except newline }
```

**String interpolation** (`"hello, ${name}!"`) is pure syntax sugar,
fully resolved by the lexer+parser — `plum-types`/`plum-ir`/both
backends never see it, only the ordinary `.concat()`/`.to_string()`
calls it desugars into (`"hello, ".concat(name.to_string()).concat("!"
)`), which already exist and already work generically over every type.
No `f"..."` prefix — every double-quoted string supports `${...}`. A
bare `$` not immediately followed by `{` is always a literal `$`; `\$`
escapes a literal `$` immediately before a `{` that would otherwise
start an interpolation.

`InterpExprSource` is an ordinary `Expr`, parsed by re-lexing the raw
text between `${` and its matching `}` — but finding that matching `}`
is DELIBERATELY RESTRICTED (see DESIGN.md's "String interpolation"
entry for the fuller "why"): only `(`/`[` nesting depth is tracked (so
`${f(a, g(b))}` works), and a nested double-quoted string's content is
skipped wholesale so an embedded `}` inside it (`${f("a}b")}`) doesn't
end the interpolation early — but `{`/`}` themselves are NOT
depth-tracked. A block expression, closure with a block body, struct
literal, `if`/`match`, or a nested string containing ITS OWN `${...}`
therefore isn't supported directly inside `${...}` — pull it into a
variable first. Getting this wrong produces a real, visible parse error
(the truncated text fails to parse as a valid expression), never
silently wrong behavior.

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
                   [ ":" Type ] { RequireClause } { EnsureClause } "=" Expr
Param         ::= Identifier
                 | "(" Pattern [ ":" Type ] ")"
GenericParams ::= "[" GenericParam { "," GenericParam } "]"
GenericParam  ::= Identifier [ ":" Bound ]
Bound         ::= Identifier { "+" Identifier }
RequireClause ::= "require" Expr [ ":" StringLiteral ]
EnsureClause  ::= "ensure" Expr [ ":" StringLiteral ]
```

Zero `Param`s makes this a plain value binding (`let x = 5`); one or
more makes it a function definition (`let sum n acc = ...`). This is
deliberate, not a simplification for the grammar's sake — see DESIGN.md,
"functions are just values" is a load-bearing ML idea, not a decoration.

**Contracts** (`require`/`ensure`, DESIGN.md's "Contracts" section) —
`require` states a precondition, checked on entry; `ensure` states a
postcondition, checked just before returning, with `result` bound to
the function's own return value inside `ensure` clauses only. Every
`require` must come before any `ensure`; interleaving them is a parse
error. `require`/`ensure` are **contextual** keywords, not reserved
words — recognized only in this one grammar slot (the only other legal
token there is `=`), so `let require = 5` elsewhere is still ordinary,
valid Plum. A function with an `ensure` clause can't declare a
parameter literally named `result` (rejected at parse time — the
postcondition needs that name for the return value). Both clause kinds
desugar entirely at parse time into ordinary `assert`-shaped calls
(`plum-types`/`plum-ir` never see a contract as such) — see DESIGN.md
for the exact rewrite and its one real trade-off: `ensure` clauses cost
that function's own tail-call-optimization, since a postcondition has
to intercept the return value before returning it; `require` alone does
not.

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

`TupleOrFunctionType` covers all three of its possibilities, decided by
the arrow and the element count:

- **Function type** — any parenthesized list followed by `"-> Type"`
  (e.g. `(Int, Int) -> Int`, `() -> Int`). Used to annotate a parameter
  that takes a closure, and to declare `extern "C"` callback parameters
  (see DESIGN.md's "FFI and C interop" section).
- **Grouping** — `(Int)` with no arrow is just `Int`, matching the
  value-level rule exactly.
- **Unit** — `()` with no arrow is `Unit`, the unit VALUE's type. Not an
  empty tuple, which would be a distinct and useless type.
- **Tuple type** — two or more types with no arrow: `(Int, Float)`.

The tuple case was added in 2026-08. Plum had tuple VALUES from the
start but no way to write their type, which meant a tuple could only
ever be INFERRED — fine for the real type checker, which infers
everything, and fatal for the self-hosted one, which requires every
top-level signature to be annotated. `bootstrap/exec_corpus/tuples/`
was unrepresentable there for exactly that reason.

Note that neither BACKEND compiles tuple values yet: `plum build`
rejects a signature involving one, and so does the self-hosted backend.
Tuples remain an interpreter-only value type; this change is the syntax
half.

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
Arguments   ::= "(" [ Argument { "," Argument } ] ")"
Argument    ::= PlaceholderChain | Expr

PlaceholderChain ::= "_" { Postfix }
```

`CompareExpr` and `RangeExpr` each allow **at most one** operator
application (non-associative) — `a < b < c` and `a..b..c` are both
grammar errors, not parsed with some implied associativity. See
DESIGN.md for why (chained comparisons silently meaning something other
than the mathematical reading is a documented footgun).

**Currying (partial application)** — DESIGN.md's "Currying" section.
No grammar change: `PostfixExpr`'s repeated `Arguments` already allowed
`f(a)(b)` syntactically. What's new is purely semantic — a `Call`
supplying FEWER arguments than its callee's own declared arity is now a
valid PARTIAL APPLICATION (producing a function value over the
remaining parameters) rather than an arity-mismatch type error, PROVIDED
at least one argument is supplied (`f()` on a multi-param `f` is
unaffected — still either the ordinary zero-arg-Unit sugar or a hard
error, never a vacuous "give me `f` back") and the callee's type is
already resolved at that call site (a still-fully-generic, unconstrained
callee doesn't get this — falls back to the ordinary arity error).
`f(a)(b)` and `f(a, b)` are provably equivalent under this rule — the
well-known ML property that also means Plum's own documented `sum (n -
1) (acc + n)` footgun (missing comma between two single-arg calls) is
no longer silently different from `sum(n - 1, acc + n)`, just a
different, equally-valid way of writing the same call. Function
DEFINITION syntax (`LetDef`, above) is completely unaffected by this —
`let divide (a: Int) (b: Int) = ...` still declares one ordinary
2-param function, never real curried nesting.

An `Argument` that begins with `_` is placeholder-lambda sugar: `_`
followed by zero or more `Postfix` steps desugars to an implicit
single-param closure over that chain, e.g. `Array.find(xs, _.toString())`
means `Array.find(xs, |n| n.toString())`, and `Array.map(xs, _)` (the
zero-`Postfix` case) means `Array.map(xs, |x| x)`. This is deliberately
narrow — see DESIGN.md — `_` is recognized only as the receiver of a
postfix chain that is the *entire* argument, not a general
Scala-style placeholder usable anywhere in an expression (`_ + 1` is
a plain parse error, not sugar for `|x| x + 1`).

**Pipe desugaring** (`x |> rhs`, see DESIGN.md's "Operator precedence
and pipe semantics" for the full rationale): `x |> f(a, b)` normally
means `f(a, b, x)` — `x` inserted as the LAST argument. If one of
`rhs`'s arguments is a bare `_` (the zero-`Postfix` case of
`PlaceholderChain` above — the same shape `Array.map(xs, _)` already
uses for the identity closure), `x` is spliced in AT that position
instead: `x |> f(a, _, b)` means `f(a, x, b)`. This matters because most
stdlib associated functions take their "subject" value FIRST, not
last — `xs |> Array.map(_, f)` is how to pipe into `Array.map(arr, f)`,
since plain `xs |> Array.map(f)` would (wrongly) mean `Array.map(f, xs)`.
More than one `_` in the same call is a compile error, not a silent
pick of one.

`Postfix` repeats to handle chains like `Ref.new(start)` (`.new` then
`(start)`, two postfix steps) and `channel[Int]()` (`[Int]` then `()`,
two postfix steps) uniformly — one rule, no special-casing based on
what's being chased.

```
PrimaryExpr ::= Literal
              | Identifier
              | "(" [ Expr { "," Expr } [ "," ] ] ")"
              | ArrayLiteral
              | Block
              | IfExpr
              | MatchExpr
              | ForExpr
              | ClosureExpr
              | UnsafeExpr
              | SpawnExpr
              | StructLiteral
              | BuiltinCall

ArrayLiteral ::= "[" [ Expr { "," Expr } [ "," ] ] "]"
BuiltinCall  ::= BuiltinName Arguments
```

`BuiltinName` is a single TOKEN, not `"@"` followed by an identifier:
`@` and the name are lexed together, so there is no bare `@` in the
language and `@ foo` is a lex error rather than something the grammar
has to reject. The name is matched against a closed set — one entry,
`@embed_file` — and anything else is an error naming what exists.

A builtin runs at COMPILE time and its call never reaches the type
checker: `@embed_file("x")` is replaced during parsing by a string
literal holding that file's contents, so the tree that leaves the
parser contains no trace of it. Its argument must therefore be a
literal, which the grammar cannot express and the parser checks —
an interpolated string is a `concat` chain by then, not a literal.

`[e1, e2, ...]` is an `Array[T]` literal — every element must unify to
the same type `T` (checked downstream, not by the parser); `[]` is
valid, its element type resolved from context (a still-unresolved fresh
var if nothing ever constrains it). This was previously, incorrectly,
documented below as "intentionally absent, not yet decided" — stale by
the time that note was written; the real parser has supported this
since early on (`ast::Expr::ArrayLiteral`). Fixed here rather than left
to keep drifting further from the real implementation.

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

```
SelectExpr    ::= "select" "{" [ SelectArm { "," SelectArm } [ "," ] ] "}"
SelectArm     ::= Pattern "=" Expr "=>" Expr
```

A `SelectArm`'s middle `Expr` is the `Receiver[T]` to wait on, and the
`Pattern` binds the value received from it — so the `=` reads as the
binding it is, not as an equality test. Unlike `MatchArm` there is no
guard: an arm that could decline after winning would have to put the
value back, and the queue has no un-pop.

`select` blocks until one arm's receiver has a value. Arms are swept in
written order, so an earlier arm wins a tie. An empty `select {}` is
rejected — see DESIGN.md, "`select`, and the primitive it was missing".

StructLiteral ::= PathType "{" [ FieldInitList ] "}"
FieldInitList ::= FieldInit { "," FieldInit } [ "," ] [ ".." Expr ]
                | ".." Expr
FieldInit     ::= Identifier { "." Identifier } [ ":" Expr ]
```

`FieldInit`'s shorthand form (`Identifier` with no `: Expr`, and no `.`
segments) means `field: field` — binds a field from a same-named
variable in scope, the same shorthand Rust's struct literals allow.

**Nested field-update path sugar**: further `.segment` steps after the
first identifier are a DIFFERENT sugar (`plumc::nested_struct_update`,
a pre-inference AST-rewrite pass — no grammar ambiguity with anything
else, since a plain field name never contains a `.`) for deep struct
updates without hand-reconstructing every intermediate level:

```
Game { ship.position.x: nx, ship.position.y: ny, ..g }
```

expands to

```
Game {
    ship: Ship { position: Vec2 { x: nx, y: ny, ..g.ship.position }, ..g.ship },
    ..g
}
```

before type inference ever runs — paths sharing a prefix merge into ONE
nested literal per level, not independent reconstructions. Requires the
literal to also carry a `..` spread (there's nothing else to read the
intermediate values from); no shorthand form (`ship.position` alone,
no `: expr`, is a parse error — there's no local named `ship.position`
for it to mean). Every intermediate segment's declared field type must
be a concrete struct (not a still-generic type parameter) — see
`nested_struct_update`'s own doc comment for the full "why" and this
v1 scope limit.

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
                  | PathType
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

A bare `PathType` is a nullary variant only when it has more than one
segment (`Shade.Light`); a single capitalized `Identifier` is
ambiguous between a nullary variant and a binding, and is resolved as
one or the other by name, not by grammar.

`PathType`'s multi-segment form is how a variant says which enum it
belongs to (`Shade.Light`, `Result.Ok(n)`). Which enum an UNQUALIFIED
tag means is a resolution question, not a grammatical one: see
DESIGN.md, "A variant tag stops being a global name".

In a PATTERN the path must begin with a capitalized segment, since that
is what tells a path-shaped pattern from an identifier binding — so the
enum may be named (`Shade.Light`) but the module may not
(`inner.Shade.Light` does not parse, though it does in an expression).
The enum name alone resolves through the declaring module, the root and
then the prelude, so the module qualifier is only missed when two
enums of the same bare NAME are visible at once.

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
- **`f (a) (b)` parses as two chained single-argument calls, not one
  two-argument call — and, as of currying (see "Expressions" above),
  that's no longer a trap.** Because `Postfix` repeats, a call
  immediately followed by another parenthesized group is `f(a)` first,
  then the parenthesized `(b)` applied again to *that result*. Before
  currying existed, `f`'s own multi-param arity made `f(a)` alone an
  arity-mismatch type error — a loud failure, at least, even though the
  syntax LOOKED like a valid two-argument call (found by parsing
  an early overview sketch's `sum (n - 1) (acc + n)`, which
  looked like an OCaml-style two-argument call but actually meant
  something else entirely — the example was corrected to
  `sum(n - 1, acc + n)`, and the "always use one comma-separated
  argument list" discipline note that used to live here is now
  historical, not live advice). Now `f(a)` alone is a valid partial
  application, and `f(a)(b)` is PROVABLY equivalent to `f(a, b)` — see
  DESIGN.md's "Currying" section for the exact reasoning. The two forms
  are genuinely interchangeable today, not a footgun to route around.
