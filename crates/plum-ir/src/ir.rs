// Deliberately simpler than the AST, and deliberately span-free: this
// is the "simplified typed IR" DESIGN.md's sequencing plan calls for —
// every later pass (FBIP, eventually codegen) should only ever need to
// handle this small, uniform set of node kinds, not the full surface
// grammar. Error messages during lowering should reference the
// original AST node's span, not carry one through into the IR itself.
//
// Scope note: this currently covers literals, variables, let, unary/
// binary operators, if, calls, and now a minimal heap-shaped value
// (`Ctor`/`Match`) — just enough for the FBIP pass to have something
// real to refcount. `Ctor`/`Match` are deliberately NOT the full
// struct/enum surface grammar: fields are positional, not named
// (matching Perceus's own minimal core calculus), and `Match` only
// deconstructs by tag, with no nested patterns, guards, or or-patterns.
// Lowering the real surface syntax down to this is separate, later,
// comparatively mechanical work — this is scoped to give the algorithm
// something to work on, not to be a complete compilation target yet.

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Bool,
    Unit,
}

// What the (still-stub) FBIP pass will insert: a `dup` (increment) at
// any point a value is used more than once, a `drop` (decrement) at a
// value's last use. See fbip.rs.
#[derive(Debug, Clone, PartialEq)]
pub enum RcOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    // `()`, Unit's only value — see DESIGN.md's "Tuples and closures".
    // Non-empty tuples aren't lowered yet (they're heap-allocated, so
    // they wait for the same pass as structs/enums).
    Unit,
    Var(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Let {
        name: String,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    // A heap-allocated, tagged value with positional fields — the
    // minimal representation of "a struct or an enum variant" this IR
    // needs. E.g. `Ctor("Point", [x, y])` or `Ctor("Cons", [head, tail])`.
    Ctor {
        tag: String,
        fields: Vec<Expr>,
    },
    // Deconstructs `scrutinee` by tag; the matching arm's `bindings`
    // name the fields positionally. No nested patterns, guards, or
    // or-patterns — see this file's scope note.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    // A reuse-in-place candidate, inserted by fbip.rs's reuse analysis
    // — the second half of FBIP, on top of refcount insertion. Means:
    // "construct (tag, fields), but if `reuse_of`'s refcount is 1 at
    // this point, overwrite its memory in place instead of allocating
    // fresh." Codegen would turn this into a single `if rc == 1`
    // branch (see DESIGN.md's memory model section) — this IR node
    // just marks WHERE that's safe, it doesn't simulate the memory
    // itself.
    CtorReuse {
        reuse_of: String,
        tag: String,
        fields: Vec<Expr>,
    },
    RcAnnotated {
        op: RcOp,
        target: String,
        rest: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub tag: String,
    pub bindings: Vec<String>,
    pub body: Expr,
}

// A named, top-level, non-closure function — see lower.rs's
// `lower_program` scope note for exactly what does and doesn't lower
// into one of these yet (only plain-identifier params; no zero-param
// "globals" yet; generics are ignored entirely, since a type parameter
// has no runtime effect without a type checker).
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub functions: Vec<Function>,
}
