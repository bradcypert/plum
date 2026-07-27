use crate::span::Span;

// Scope note: this AST now covers the "expression core," the Pattern
// grammar, and the block-level expression forms (if/match/for/
// closures/unsafe/spawn/struct-literals/blocks) from GRAMMAR.md.
// Deliberately not yet covered: the Item grammar (let defs, struct/enum
// decls, extern blocks, use decls). See DESIGN.md/GRAMMAR.md for the
// full grammar this is being built out against incrementally.

#[derive(Debug, Clone, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinaryOp {
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
    Range,
    // Pipe is kept as an ordinary binary operator at the parse-tree
    // level, matching GRAMMAR.md's PipeExpr production. The "insert as
    // last argument" desugaring rule from DESIGN.md is a semantic
    // rewrite, applied during IR lowering, not something the parser
    // bakes in — the parser's job is to produce a faithful parse tree.
    Pipe,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Path(Vec<String>, Span),
    Generic {
        base: Vec<String>,
        args: Vec<Type>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Bool(bool, Span),
    Ident(String, Span),
    Tuple(Vec<Expr>, Span),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Field {
        base: Box<Expr>,
        name: String,
        span: Span,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    GenericInst {
        callee: Box<Expr>,
        args: Vec<Type>,
        span: Span,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    Block(Block, Span),
    If {
        cond: Box<Expr>,
        then_branch: Block,
        // `Block | IfExpr` per GRAMMAR.md — both are Exprs
        // (Expr::Block / Expr::If), so this just holds either directly
        // rather than needing a separate sum type.
        else_branch: Option<Box<Expr>>,
        span: Span,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
    For {
        pattern: Pattern,
        iter: Box<Expr>,
        body: Block,
        span: Span,
    },
    Closure {
        params: Vec<ClosureParam>,
        body: Box<Expr>,
        span: Span,
    },
    Unsafe(Block, Span),
    Spawn(Block, Span),
    StructLiteral {
        path: Vec<String>,
        fields: Vec<FieldInit>,
        spread: Option<Box<Expr>>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Let {
        is_mut: bool,
        pattern: Pattern,
        ty: Option<Type>,
        value: Expr,
        span: Span,
    },
    // Target is always a plain identifier, never a general Pattern or
    // lvalue path — see GRAMMAR.md's AssignStmt note. `Ref[T]` mutation
    // goes exclusively through .get()/.set()/.update(), not assignment.
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClosureParam {
    pub name: String,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldInit {
    pub name: String,
    // Shorthand (`x`, meaning `x: x`) is resolved at parse time into
    // `Expr::Ident(name, _)`, same reasoning as FieldPattern's
    // shorthand below.
    pub value: Expr,
    pub span: Span,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Str(_, s)
            | Expr::Bool(_, s)
            | Expr::Ident(_, s)
            | Expr::Tuple(_, s)
            | Expr::Unary { span: s, .. }
            | Expr::Binary { span: s, .. }
            | Expr::Field { span: s, .. }
            | Expr::Call { span: s, .. }
            | Expr::GenericInst { span: s, .. }
            | Expr::Index { span: s, .. }
            | Expr::Block(_, s)
            | Expr::If { span: s, .. }
            | Expr::Match { span: s, .. }
            | Expr::For { span: s, .. }
            | Expr::Closure { span: s, .. }
            | Expr::Unsafe(_, s)
            | Expr::Spawn(_, s)
            | Expr::StructLiteral { span: s, .. } => *s,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldPattern {
    pub name: String,
    // Shorthand (`x`, meaning "bind field x to a variable named x") is
    // resolved at parse time into `Pattern::Ident(name, _)` rather than
    // carried as an Option — one less case for every downstream
    // consumer to handle, and it's exactly equivalent semantically.
    pub pattern: Pattern,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Bool(bool, Span),
    Wildcard(Span),
    Ident(String, Span),
    Tuple(Vec<Pattern>, Span),
    // Covers both `Shape.Circle(r)` (payload) and a bare `None`
    // (zero-arg variant reference, args empty) — see DESIGN.md/
    // GRAMMAR.md, disambiguated from a plain binding by capitalization,
    // the same convention used for `.` and `[T]` elsewhere.
    Variant {
        path: Vec<String>,
        args: Vec<Pattern>,
        span: Span,
    },
    Struct {
        path: Vec<String>,
        fields: Vec<FieldPattern>,
        has_rest: bool,
        span: Span,
    },
    Or(Vec<Pattern>, Span),
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Int(_, s)
            | Pattern::Float(_, s)
            | Pattern::Str(_, s)
            | Pattern::Bool(_, s)
            | Pattern::Wildcard(s)
            | Pattern::Ident(_, s)
            | Pattern::Tuple(_, s)
            | Pattern::Variant { span: s, .. }
            | Pattern::Struct { span: s, .. }
            | Pattern::Or(_, s) => *s,
        }
    }
}
