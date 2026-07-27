#[derive(Debug, Clone)]
pub enum Type {
    Int,
    Bool,
    Unit,
}

#[derive(Debug, Clone)]
pub enum RcOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64),
    Var(String),
    Let {
        name: String,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    RcAnnotated {
        op: RcOp,
        target: String,
        rest: Box<Expr>,
    },
}
