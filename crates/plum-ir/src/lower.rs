use crate::ir;
use plum_syntax::ast;

pub fn lower_expr(expr: &ast::Expr) -> Result<ir::Expr, String> {
    match expr {
        ast::Expr::Int(n, _) => Ok(ir::Expr::Int(*n)),
        ast::Expr::Ident(name, _) => Ok(ir::Expr::Var(name.clone())),
        // ast.rs currently covers the expression core only (see its
        // scope note) — binary/unary/call/etc. lowering isn't written
        // yet, so this stays an explicit error rather than a silent gap.
        _ => Err("lowering not yet implemented for this expression form".to_string()),
    }
}
