// Plum's tree-sitter grammar — transcribed from `GRAMMAR.md` (the
// project's own formal EBNF, kept as the single source of truth both
// this grammar and `plum-syntax`'s hand-written recursive-descent
// parser answer to) plus two real syntax forms `GRAMMAR.md` doesn't
// document yet (found by reading `plum-syntax` directly, not assumed):
// array literals (`[e1, e2, ...]`) and `select { pattern = expr => body,
// ... }` for reading from multiple channels. See this file's own
// section comments for where a rule maps onto a specific `GRAMMAR.md`
// production, and where it deliberately doesn't.
//
// **Scope, deliberate**: this is a SEPARATE grammar from `plum-syntax`'s
// real one, existing purely to drive tree-sitter-based syntax
// highlighting/indentation in editors — not a second implementation of
// the language's actual syntax rules for any correctness-bearing
// purpose. It is kept deliberately dumb: no semantic disambiguation
// (that needs identifier capitalization + name resolution, which a
// context-free grammar can't do — see `GRAMMAR.md`'s "Known
// ambiguities" section), and one documented simplification where the
// real parser tracks parser STATE (`no_struct_literal`) that a
// context-free grammar can't cheaply mirror — see `struct_literal`'s
// own comment below.

const PREC = {
  PIPE: 1,
  OR: 2,
  AND: 3,
  COMPARE: 4,
  RANGE: 5,
  ADD: 6,
  MUL: 7,
  UNARY: 8,
  POSTFIX: 9,
};

module.exports = grammar({
  name: "plum",

  extras: ($) => [/\s/, $.comment],

  word: ($) => $.identifier,

  conflicts: ($) => [
    // `x.y` (a plain field-access postfix chain) and `x.y { ... }`
    // (a struct literal whose path happens to be dotted) share an
    // unbounded common prefix — genuinely needs GLR to explore both
    // and let `struct_literal`'s own `prec.dynamic(-1, ...)` (see its
    // comment) discard the wrong one once `{` either does or doesn't
    // show up.
    [$.path, $._primary_expr],
  ],

  rules: {
    // ---- Program structure (GRAMMAR.md "Program structure") ----
    source_file: ($) => repeat($.item),

    item: ($) => seq(optional("pub"), $._item_kind),
    _item_kind: ($) =>
      choice($.let_def, $.struct_decl, $.enum_decl, $.extern_block, $.use_decl),

    let_def: ($) =>
      seq(
        "let",
        // `let Type.method (...) = ...` — a real per-type ASSOCIATED
        // function declaration (`Point.add`, `Option.map`), not
        // documented in `GRAMMAR.md`'s `LetDef` production at all
        // (found by reading `parser.rs::parse_let_def` directly, which
        // special-cases exactly one optional `"." Identifier` suffix
        // here — not a general dotted `path`, so this mirrors that
        // precisely rather than being more permissive than the real
        // parser).
        field("name", $.identifier),
        optional(seq(".", field("assoc_name", $.identifier))),
        optional($.generic_params),
        repeat(field("param", $.param)),
        optional(seq(":", field("return_type", $._type))),
        "=",
        field("value", $._expr),
      ),

    // `parser.rs::parse_param`'s parenthesized form is really three
    // shapes, confirmed by reading it directly (not from GRAMMAR.md
    // alone, which just says `"(" Pattern [":" Type] ")"` — that
    // EBNF alone can't produce `()`, the Unit param): `()` (the Unit
    // pattern, special-cased), `(a, b)` (tuple-destructure, single-
    // paren form, no `: Type` suffix), or `(pat: Type)` (singleton
    // with an optional type). Collapsed into one permissive rule here
    // rather than splitting into three matching alternatives — a
    // touch more permissive than the real parser (e.g. it doesn't
    // reject a stray trailing comma the same way), which is an
    // acceptable trade for a highlighting-only grammar.
    param: ($) =>
      choice(
        $.identifier,
        seq(
          "(",
          optional(seq(commaSep1($.pattern), optional(","))),
          optional(seq(":", $._type)),
          ")",
        ),
      ),

    generic_params: ($) => seq("[", commaSep1($.generic_param), "]"),
    generic_param: ($) => seq($.identifier, optional(seq(":", $.bound))),
    bound: ($) => sep1($.identifier, "+"),

    struct_decl: ($) =>
      seq(
        "struct",
        field("name", $.identifier),
        optional($.generic_params),
        "{",
        optional($.field_list),
        "}",
      ),
    field_list: ($) => seq(commaSep1($.field), optional(",")),
    field: ($) =>
      seq(optional("pub"), field("name", $.identifier), ":", field("type", $._type)),

    enum_decl: ($) =>
      seq(
        "enum",
        field("name", $.identifier),
        optional($.generic_params),
        "{",
        optional($.variant_list),
        "}",
      ),
    variant_list: ($) => seq(commaSep1($.variant), optional(",")),
    variant: ($) =>
      seq(field("name", $.identifier), optional(seq("(", commaSep1($._type), ")"))),

    extern_block: ($) => seq("extern", $.string_literal, "{", repeat($.extern_fn), "}"),
    extern_fn: ($) =>
      seq(
        "fn",
        field("name", $.identifier),
        "(",
        optional($.extern_param_list),
        ")",
        optional(seq("->", field("return_type", $._type))),
        ";",
      ),
    extern_param_list: ($) => commaSep1($.extern_param),
    extern_param: ($) => seq(field("name", $.identifier), ":", field("type", $._type)),

    use_decl: ($) => seq("use", $.path, ";"),
    path: ($) => sep1($.identifier, "."),

    // ---- Types (GRAMMAR.md "Types") ----
    _type: ($) => choice($.path_type, $.tuple_or_function_type),
    path_type: ($) => seq(sep1($.identifier, "."), optional($.generic_args)),
    generic_args: ($) => seq("[", commaSep1($._type), "]"),
    tuple_or_function_type: ($) =>
      prec.right(
        seq(
          "(",
          optional(seq(commaSep1($._type), optional(","))),
          ")",
          optional(seq("->", $._type)),
        ),
      ),

    // ---- Expressions (GRAMMAR.md "Expressions") ----
    // Mirrors GRAMMAR.md's precedence chain one rule per production —
    // rather than collapsing it into a single generic `binary_expr`
    // with a numeric precedence table — so that `compare_expr`/
    // `range_expr` can enforce GRAMMAR.md's "at most one operator
    // application (non-associative)" rule structurally (a plain
    // `optional(...)`, not a `repeat(...)`) instead of needing a
    // runtime check a highlighter-only grammar has no way to perform.
    _expr: ($) => $.pipe_expr,

    pipe_expr: ($) =>
      prec.left(PREC.PIPE, seq($.or_expr, repeat(seq("|>", $.or_expr)))),
    or_expr: ($) => prec.left(PREC.OR, seq($.and_expr, repeat(seq("||", $.and_expr)))),
    and_expr: ($) =>
      prec.left(PREC.AND, seq($.compare_expr, repeat(seq("&&", $.compare_expr)))),
    compare_expr: ($) =>
      prec.left(
        PREC.COMPARE,
        seq($.range_expr, optional(seq($.compare_op, $.range_expr))),
      ),
    compare_op: () => choice("==", "!=", "<", ">", "<=", ">="),
    range_expr: ($) =>
      prec.left(PREC.RANGE, seq($.add_expr, optional(seq("..", $.add_expr)))),
    add_expr: ($) =>
      prec.left(PREC.ADD, seq($.mul_expr, repeat(seq(choice("+", "-"), $.mul_expr)))),
    mul_expr: ($) =>
      prec.left(
        PREC.MUL,
        seq($.unary_expr, repeat(seq(choice("*", "/", "%"), $.unary_expr))),
      ),
    unary_expr: ($) =>
      choice(prec(PREC.UNARY, seq(choice("-", "!"), $.unary_expr)), $.postfix_expr),

    postfix_expr: ($) => prec.left(PREC.POSTFIX, seq($._primary_expr, repeat($._postfix))),
    _postfix: ($) =>
      choice(
        seq(".", field("field", $.identifier)),
        field("arguments", $.arguments),
        $.bracket_postfix,
      ),
    // Covers BOTH `arr[i]` (indexing) and `Thing[T]`/`channel[Int, Str]`
    // (generic instantiation) as one production — `GRAMMAR.md`'s own
    // "Known ambiguities" section names this pair as resolved by
    // IDENTIFIER CAPITALIZATION at parse/resolve time in the real
    // parser (`Parser::next_bracket_is_generic`), which a context-free
    // grammar has no way to check. Splitting them into two competing
    // bracket productions here is a genuine, provable LR conflict (not
    // just a GLR-resolvable local ambiguity, confirmed by hand: `tree-
    // sitter generate` rejects it outright) — so, per this file's own
    // "deliberately dumb" scope, both collapse into one neutral
    // bracket-postfix node instead. A generic identifier like `Int`
    // parses fine as a plain `_expr` atom, so `channel[Int]` still
    // parses correctly here — just labeled uniformly, not
    // distinguished from real indexing the way the real compiler does.
    bracket_postfix: ($) => seq("[", commaSep1($._expr), "]"),
    arguments: ($) => seq("(", optional(seq(commaSep1($.argument), optional(","))), ")"),
    argument: ($) => choice($.placeholder_chain, $._expr),
    // `_.method()` sugar (see the "sugar _.method" entry in this
    // repo's own git log) — `_` followed by zero or more `Postfix`
    // steps, recognized only as an entire argument on its own, not a
    // general Scala-style placeholder (see GRAMMAR.md).
    placeholder_chain: ($) => prec.left(seq("_", repeat($._postfix))),

    _primary_expr: ($) =>
      choice(
        $._literal,
        $.identifier,
        $.paren_or_tuple_expr,
        $.array_literal,
        $.block,
        $.if_expr,
        $.match_expr,
        $.select_expr,
        $.for_expr,
        $.closure_expr,
        $.unsafe_expr,
        $.spawn_expr,
        $.struct_literal,
      ),

    paren_or_tuple_expr: ($) =>
      seq("(", optional(seq(commaSep1($._expr), optional(","))), ")"),

    array_literal: ($) => seq("[", optional(seq(commaSep1($._expr), optional(","))), "]"),

    if_expr: ($) =>
      prec.right(
        seq(
          "if",
          field("condition", $._expr),
          field("consequence", $.block),
          optional(seq("else", field("alternative", choice($.block, $.if_expr)))),
        ),
      ),

    for_expr: ($) =>
      seq("for", field("pattern", $.pattern), "in", field("iterable", $._expr), $.block),

    unsafe_expr: ($) => seq("unsafe", $.block),
    spawn_expr: ($) => seq("spawn", $.block),

    closure_expr: ($) =>
      choice(
        // `||` (an empty closure's param list) lexes as ONE token under
        // tree-sitter's own longest-match rule, exactly the same
        // maximal-munch interaction `plum-syntax`'s own lexer has to
        // work around (see `parser.rs`'s comment on `TokenKind::
        // OrOr` and its `TokenKind::Pipe | TokenKind::OrOr` dispatch)
        // — this alternative is that same fix at the grammar level.
        seq("||", field("body", $._expr)),
        seq("|", optional(commaSep1($.closure_param)), "|", field("body", $._expr)),
      ),
    closure_param: ($) => seq($.identifier, optional(seq(":", $._type))),

    match_expr: ($) =>
      seq("match", field("scrutinee", $._expr), "{", optional($.match_arm_list), "}"),
    match_arm_list: ($) => seq(commaSep1($.match_arm), optional(",")),
    match_arm: ($) =>
      seq(
        field("pattern", $.pattern),
        optional(seq("if", field("guard", $._expr))),
        "=>",
        field("body", $._expr),
      ),

    select_expr: ($) =>
      seq("select", "{", optional($.select_arm_list), "}"),
    select_arm_list: ($) => seq(commaSep1($.select_arm), optional(",")),
    select_arm: ($) =>
      seq(
        field("pattern", $.pattern),
        "=",
        field("channel", $._expr),
        "=>",
        field("body", $._expr),
      ),

    // Struct literals — deliberately given LOWER dynamic precedence
    // than the rest of `_primary_expr`, so tree-sitter's GLR parser
    // resolves `if Point { x: 1.0 } { ... }`'s local ambiguity (is the
    // brace a struct literal's body, or the `if`'s own consequence
    // block?) toward "it's the `if`'s block" — the far more common
    // real-world shape, and the SAME restriction `plum-syntax`'s own
    // parser enforces via its `no_struct_literal` flag (see
    // `parser.rs`), just reached by a different mechanism here since a
    // context-free grammar can't thread that flag through the way a
    // recursive-descent parser can. `GRAMMAR.md`'s own "Known
    // ambiguities" section names this exact case as implementation-
    // defined, not a language design question.
    //
    // Uses `path` (plain dotted identifiers), NOT `path_type` —
    // `GRAMMAR.md`'s formal `StructLiteral ::= PathType "{" ...`
    // production technically allows a `GenericArgs` suffix here, but
    // `parser.rs::parse_path_shaped_expr` never actually parses one at
    // a struct-literal head (confirmed by reading it directly: it
    // walks a plain `.`-joined identifier chain, checks the last
    // segment's capitalization, then looks for `{` — no bracket
    // handling in between). Matching the real parser's AST
    // (`Expr::StructLiteral`'s `path: Vec<String>`) here also
    // sidesteps a genuine grammar ambiguity a bracket suffix would
    // create against postfix indexing (`Thing[i]`).
    struct_literal: ($) =>
      prec.dynamic(-1, seq(field("path", $.path), "{", optional($.field_init_list), "}")),
    field_init_list: ($) =>
      choice(
        seq(commaSep1($.field_init), optional(","), optional(seq("..", $._expr))),
        seq("..", $._expr),
      ),
    field_init: ($) => seq(field("name", $.identifier), optional(seq(":", field("value", $._expr)))),

    // ---- Blocks (GRAMMAR.md "Blocks (the statement/expression rule)") ----
    block: ($) => seq("{", repeat($._block_stmt), optional($._expr), "}"),
    _block_stmt: ($) => seq(choice($.let_stmt, $.assign_stmt, $._expr), ";"),
    let_stmt: ($) =>
      seq(
        "let",
        optional("mut"),
        field("pattern", $.pattern),
        optional(seq(":", field("type", $._type))),
        "=",
        field("value", $._expr),
      ),
    assign_stmt: ($) => seq(field("target", $.identifier), "=", field("value", $._expr)),

    // ---- Patterns (GRAMMAR.md "Patterns") ----
    pattern: ($) => sep1($._primary_pattern, "|"),
    _primary_pattern: ($) =>
      choice(
        $._literal,
        "_",
        $.identifier,
        $.variant_pattern,
        $.struct_pattern,
        $.tuple_pattern,
      ),
    variant_pattern: ($) =>
      seq(field("path", $.path), "(", optional(commaSep1($.pattern)), ")"),
    struct_pattern: ($) =>
      seq(field("path", $.path), "{", optional($.field_pattern_list), "}"),
    field_pattern_list: ($) =>
      choice(seq(commaSep1($.field_pattern), optional(seq(",", optional("..")))), ".."),
    field_pattern: ($) => seq(field("name", $.identifier), optional(seq(":", $.pattern))),
    tuple_pattern: ($) => seq("(", optional(seq(commaSep1($.pattern), optional(","))), ")"),

    // ---- Lexical grammar (GRAMMAR.md "Lexical grammar") ----
    identifier: () => /[A-Za-z_][A-Za-z0-9_]*/,

    _literal: ($) => choice($.int_literal, $.float_literal, $.string_literal, $.bool_literal),
    int_literal: () => /[0-9][0-9_]*/,
    float_literal: () => /[0-9][0-9_]*\.[0-9][0-9_]*/,
    bool_literal: () => choice("true", "false"),

    string_literal: ($) =>
      seq('"', repeat(choice($.escape_sequence, /[^"\\\n]+/)), '"'),
    escape_sequence: () => /\\./,

    comment: () => token(seq("//", /.*/)),
  },
});

function commaSep1(rule) {
  return sep1(rule, ",");
}

function sep1(rule, separator) {
  return seq(rule, repeat(seq(separator, rule)));
}
