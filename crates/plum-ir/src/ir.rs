// Deliberately simpler than the AST, and deliberately span-free: this
// is the "simplified typed IR" DESIGN.md's sequencing plan calls for —
// every later pass (FBIP, eventually codegen) should only ever need to
// handle this small, uniform set of node kinds, not the full surface
// grammar. Error messages during lowering should reference the
// original AST node's span, not carry one through into the IR itself.
//
// Scope note: this currently covers literals, variables, let, unary/
// binary operators, if, and calls — enough for arithmetic and control
// flow. Deliberately NOT yet covered: anything heap-shaped (structs,
// enum variants, closures) or Match — FBIP has nothing to do until
// there's an actual allocation to reuse, so those come with the FBIP
// pass itself, not before it.

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
    RcAnnotated {
        op: RcOp,
        target: String,
        rest: Box<Expr>,
    },
}
