use crate::ir::Expr;

/// Placeholder for the functional-but-in-place pass: inserts explicit
/// refcount inc/dec operations, and will later decide when a mutation
/// can reuse a uniquely-owned allocation instead of copying.
pub fn insert_refcount_ops(expr: Expr) -> Expr {
    expr
}
