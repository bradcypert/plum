use crate::subst::Subst;
use crate::types::{Type, TypeVarId};
use crate::unify::unify;
use plum_syntax::ast;

/// A simple, monomorphic scope stack — same shadowing/scoping shape as
/// plum-interp's `env`, just mapping names to Types instead of Values.
/// Deliberately NOT polymorphic yet: a `let`-bound name gets exactly
/// one concrete (possibly still-a-variable) type, not a generalized
/// scheme that can be instantiated differently at each use. That's
/// `let`-polymorphism, a real and separate next step — see this
/// module's scope note.
#[derive(Clone)]
pub struct TypeEnv(Vec<(String, Type)>);

impl TypeEnv {
    pub fn new() -> Self {
        TypeEnv(Vec::new())
    }

    pub fn extend(&self, name: String, ty: Type) -> TypeEnv {
        let mut v = self.0.clone();
        v.push((name, ty));
        TypeEnv(v)
    }

    pub fn lookup(&self, name: &str) -> Option<&Type> {
        self.0.iter().rev().find(|(n, _)| n == name).map(|(_, t)| t)
    }
}

impl Default for TypeEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds the fresh-type-variable counter — the only piece of mutable
/// state inference needs (everything else flows through return values,
/// following the Subst-accumulator idiom described in this module's
/// top-level docs / the surrounding conversation).
pub struct Infer {
    next_var: TypeVarId,
}

impl Infer {
    pub fn new() -> Self {
        Infer { next_var: 0 }
    }

    /// Generates a never-before-used type variable. Not called by any
    /// `infer_expr` case yet (nothing in this pass's scope needs one —
    /// no unannotated parameters, no polymorphism to instantiate), but
    /// genuinely useful to expose now rather than build later: the
    /// next pass needs it, and it's a reasonable thing for a consumer
    /// of this crate to want directly regardless.
    pub fn fresh(&mut self) -> Type {
        let id = self.next_var;
        self.next_var += 1;
        Type::Var(id)
    }

    pub fn infer_expr(&mut self, expr: &ast::Expr, env: &TypeEnv) -> Result<(Type, Subst), String> {
        match expr {
            ast::Expr::Int(_, _) => Ok((Type::Int, Subst::empty())),
            ast::Expr::Float(_, _) => Ok((Type::Float, Subst::empty())),
            ast::Expr::Str(_, _) => Ok((Type::Str, Subst::empty())),
            ast::Expr::Bool(_, _) => Ok((Type::Bool, Subst::empty())),
            ast::Expr::Tuple(elems, _) if elems.is_empty() => Ok((Type::Unit, Subst::empty())),
            ast::Expr::Tuple(_, span) => Err(format!(
                "type inference not yet implemented for non-empty tuples at {span:?}"
            )),
            ast::Expr::Ident(name, span) => {
                let ty = env
                    .lookup(name)
                    .cloned()
                    .ok_or_else(|| format!("unbound variable: {name} at {span:?}"))?;
                Ok((ty, Subst::empty()))
            }
            ast::Expr::Unary { op, expr, .. } => self.infer_unary(op, expr, env),
            ast::Expr::Binary {
                op: ast::BinaryOp::Pipe,
                span,
                ..
            } => Err(format!(
                "type inference not yet implemented for `|>` (waits for call inference) at {span:?}"
            )),
            ast::Expr::Binary {
                op: ast::BinaryOp::Range,
                span,
                ..
            } => Err(format!("type inference not yet implemented for ranges at {span:?}")),
            ast::Expr::Binary { op, lhs, rhs, .. } => self.infer_binary(op, lhs, rhs, env),
            ast::Expr::Block(block, _) => self.infer_block(block, env),
            ast::Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => self.infer_if(cond, then_branch, else_branch, env),
            other => Err(format!(
                "type inference not yet implemented for this expression form at {:?}",
                other.span()
            )),
        }
    }

    fn infer_unary(&mut self, op: &ast::UnaryOp, expr: &ast::Expr, env: &TypeEnv) -> Result<(Type, Subst), String> {
        let (operand_ty, mut acc) = self.infer_expr(expr, env)?;
        match op {
            ast::UnaryOp::Neg => {
                let resolved = acc.apply(&operand_ty);
                require_numeric(&resolved)?;
                Ok((resolved, acc))
            }
            ast::UnaryOp::Not => {
                let s = unify(&acc.apply(&operand_ty), &Type::Bool).map_err(|e| format!("`!`: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Bool, acc))
            }
        }
    }

    fn infer_binary(
        &mut self,
        op: &ast::BinaryOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        env: &TypeEnv,
    ) -> Result<(Type, Subst), String> {
        use ast::BinaryOp::*;

        // `&&`/`||`: each operand independently must unify with Bool.
        if matches!(op, And | Or) {
            let (lty, s) = self.infer_expr(lhs, env)?;
            let mut acc = s;
            let s = unify(&acc.apply(&lty), &Type::Bool).map_err(|e| format!("logical operator: {e}"))?;
            acc = s.compose(&acc);
            let (rty, s) = self.infer_expr(rhs, env)?;
            acc = s.compose(&acc);
            let s = unify(&acc.apply(&rty), &Type::Bool).map_err(|e| format!("logical operator: {e}"))?;
            acc = s.compose(&acc);
            return Ok((Type::Bool, acc));
        }

        // Every other operator handled here needs both operands to
        // unify with each other first.
        let (lty, s) = self.infer_expr(lhs, env)?;
        let mut acc = s;
        let (rty, s) = self.infer_expr(rhs, env)?;
        acc = s.compose(&acc);
        let s = unify(&acc.apply(&lty), &acc.apply(&rty)).map_err(|e| format!("operator: {e}"))?;
        acc = s.compose(&acc);
        let operand_ty = acc.apply(&lty);

        match op {
            Eq | Ne => Ok((Type::Bool, acc)),
            Lt | Gt | Le | Ge => {
                require_numeric(&operand_ty)?;
                Ok((Type::Bool, acc))
            }
            Add | Sub | Mul | Div | Rem => {
                require_numeric(&operand_ty)?;
                Ok((operand_ty, acc))
            }
            And | Or | Pipe | Range => unreachable!("handled above or by the Binary match arm in infer_expr"),
        }
    }

    fn infer_if(
        &mut self,
        cond: &ast::Expr,
        then_branch: &ast::Block,
        else_branch: &Option<Box<ast::Expr>>,
        env: &TypeEnv,
    ) -> Result<(Type, Subst), String> {
        let (cond_ty, s) = self.infer_expr(cond, env)?;
        let mut acc = s;
        let s = unify(&acc.apply(&cond_ty), &Type::Bool).map_err(|e| format!("`if` condition: {e}"))?;
        acc = s.compose(&acc);

        let (then_ty, s) = self.infer_block(then_branch, env)?;
        acc = s.compose(&acc);

        match else_branch {
            Some(else_expr) => {
                let (else_ty, s) = self.infer_expr(else_expr, env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&then_ty), &acc.apply(&else_ty))
                    .map_err(|e| format!("`if`/`else` branches must match: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&then_ty), acc))
            }
            None => {
                let s = unify(&acc.apply(&then_ty), &Type::Unit)
                    .map_err(|e| format!("`if` without `else` must produce Unit: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Unit, acc))
            }
        }
    }

    fn infer_block(&mut self, block: &ast::Block, env: &TypeEnv) -> Result<(Type, Subst), String> {
        let mut acc = Subst::empty();
        let mut cur_env = env.clone();
        for stmt in &block.stmts {
            match stmt {
                ast::Stmt::Let { pattern, value, ty, .. } => {
                    let name = plain_ident(pattern)?;
                    let (val_ty, s) = self.infer_expr(value, &cur_env)?;
                    acc = s.compose(&acc);
                    let mut resolved = acc.apply(&val_ty);
                    if let Some(annotation) = ty {
                        let ann_ty = ast_type_to_type(annotation)?;
                        let s = unify(&resolved, &ann_ty)
                            .map_err(|e| format!("`let` annotation for {name:?}: {e}"))?;
                        acc = s.compose(&acc);
                        resolved = acc.apply(&resolved);
                    }
                    cur_env = cur_env.extend(name, resolved);
                }
                ast::Stmt::Expr(e) => {
                    let (_, s) = self.infer_expr(e, &cur_env)?;
                    acc = s.compose(&acc);
                }
                ast::Stmt::Assign { span, .. } => {
                    return Err(format!(
                        "type inference not yet implemented for assignment statements at {span:?}"
                    ));
                }
            }
        }
        match &block.tail {
            Some(tail) => {
                let (ty, s) = self.infer_expr(tail, &cur_env)?;
                acc = s.compose(&acc);
                Ok((acc.apply(&ty), acc))
            }
            None => Ok((Type::Unit, acc)),
        }
    }
}

fn require_numeric(ty: &Type) -> Result<(), String> {
    match ty {
        Type::Int | Type::Float => Ok(()),
        other => Err(format!("expected a numeric type (Int or Float), found {other:?}")),
    }
}

fn plain_ident(pattern: &ast::Pattern) -> Result<String, String> {
    match pattern {
        ast::Pattern::Ident(name, _) => Ok(name.clone()),
        other => Err(format!(
            "type inference not yet implemented for destructuring let-bindings at {:?}",
            other.span()
        )),
    }
}

fn ast_type_to_type(ty: &ast::Type) -> Result<Type, String> {
    match ty {
        ast::Type::Path(segments, span) => match segments.last().map(String::as_str) {
            Some("Int") => Ok(Type::Int),
            Some("Float") => Ok(Type::Float),
            Some("Bool") => Ok(Type::Bool),
            Some("String") => Ok(Type::Str),
            Some("Unit") => Ok(Type::Unit),
            _ => Err(format!(
                "type inference not yet implemented for this type annotation at {span:?}"
            )),
        },
        ast::Type::Generic { span, .. } => Err(format!(
            "type inference not yet implemented for generic type annotations at {span:?}"
        )),
    }
}

impl Default for Infer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    #[test]
    fn fresh_variables_are_distinct_and_increasing() {
        // Not exercised by any infer_expr case yet in this pass (no
        // unannotated parameters or polymorphism to instantiate) — the
        // next pass needs this, so it's built now and proven correct
        // in isolation rather than left unused until it's wired in.
        let mut infer = Infer::new();
        assert_eq!(infer.fresh(), Type::Var(0));
        assert_eq!(infer.fresh(), Type::Var(1));
        assert_eq!(infer.fresh(), Type::Var(2));
    }

    fn infer(src: &str) -> Type {
        infer_in(src, &TypeEnv::new())
    }

    fn infer_in(src: &str, env: &TypeEnv) -> Type {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let mut infer = Infer::new();
        let (ty, subst) = infer
            .infer_expr(&ast, env)
            .unwrap_or_else(|e| panic!("inference error for {src:?}: {e}"));
        subst.apply(&ty)
    }

    fn infer_err(src: &str) -> String {
        infer_err_in(src, &TypeEnv::new())
    }

    fn infer_err_in(src: &str, env: &TypeEnv) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let mut infer = Infer::new();
        infer
            .infer_expr(&ast, env)
            .expect_err(&format!("expected inference of {src:?} to fail"))
    }

    #[test]
    fn literals() {
        assert_eq!(infer("5"), Type::Int);
        assert_eq!(infer("3.14"), Type::Float);
        assert_eq!(infer("true"), Type::Bool);
        assert_eq!(infer("\"hi\""), Type::Str);
        assert_eq!(infer("()"), Type::Unit);
    }

    #[test]
    fn variable_looked_up_in_env() {
        let env = TypeEnv::new().extend("x".to_string(), Type::Int);
        assert_eq!(infer_in("x", &env), Type::Int);
    }

    #[test]
    fn unbound_variable_is_an_error() {
        infer_err("y");
    }

    #[test]
    fn unary_neg_on_numeric_types() {
        assert_eq!(infer("-5"), Type::Int);
        assert_eq!(infer("-3.14"), Type::Float);
    }

    #[test]
    fn unary_neg_on_non_numeric_is_an_error() {
        infer_err("-true");
    }

    #[test]
    fn unary_not_on_bool() {
        assert_eq!(infer("!true"), Type::Bool);
    }

    #[test]
    fn unary_not_on_non_bool_is_an_error() {
        infer_err("!5");
    }

    #[test]
    fn arithmetic_on_matching_numeric_types() {
        assert_eq!(infer("1 + 2"), Type::Int);
        assert_eq!(infer("1.5 + 2.5"), Type::Float);
        assert_eq!(infer("10 - 3"), Type::Int);
        assert_eq!(infer("4 * 5"), Type::Int);
    }

    #[test]
    fn arithmetic_on_mismatched_types_is_an_error() {
        infer_err("1 + 1.0");
    }

    #[test]
    fn arithmetic_on_non_numeric_types_is_an_error() {
        infer_err("true + false");
    }

    #[test]
    fn comparison_requires_numeric_and_produces_bool() {
        assert_eq!(infer("3 < 5"), Type::Bool);
        assert_eq!(infer("3.0 <= 5.0"), Type::Bool);
    }

    #[test]
    fn comparison_on_non_numeric_is_an_error() {
        // Would unify fine (Bool == Bool), but `<` specifically
        // requires numeric operands, unlike `==`.
        infer_err("true < false");
    }

    #[test]
    fn equality_works_on_any_matching_type_not_just_numeric() {
        assert_eq!(infer("3 == 3"), Type::Bool);
        assert_eq!(infer("true == false"), Type::Bool);
        assert_eq!(infer("\"a\" == \"a\""), Type::Bool);
    }

    #[test]
    fn equality_on_mismatched_types_is_an_error() {
        infer_err("3 == true");
    }

    #[test]
    fn logical_and_or_require_bool() {
        assert_eq!(infer("true && false"), Type::Bool);
        assert_eq!(infer("true || false"), Type::Bool);
    }

    #[test]
    fn logical_and_on_non_bool_is_an_error() {
        infer_err("1 && 2");
    }

    #[test]
    fn if_with_matching_branches() {
        assert_eq!(infer("if true { 1 } else { 2 }"), Type::Int);
    }

    #[test]
    fn if_with_mismatched_branches_is_an_error() {
        infer_err("if true { 1 } else { true }");
    }

    #[test]
    fn if_condition_must_be_bool() {
        infer_err("if 5 { 1 } else { 2 }");
    }

    #[test]
    fn if_without_else_must_be_unit() {
        infer_err("if true { 1 }");
        assert_eq!(infer("if true { () }"), Type::Unit);
    }

    #[test]
    fn block_let_and_use() {
        assert_eq!(infer("{ let x = 5; x + 1 }"), Type::Int);
    }

    #[test]
    fn block_let_shadowing_later_binding_wins() {
        assert_eq!(infer("{ let x = 1; let x = true; x }"), Type::Bool);
    }

    #[test]
    fn block_discarded_statement_does_not_constrain_result() {
        assert_eq!(infer("{ true; 5 }"), Type::Int);
    }

    #[test]
    fn empty_block_is_unit() {
        assert_eq!(infer("{}"), Type::Unit);
    }

    #[test]
    fn let_type_annotation_checked_against_inferred_type() {
        assert_eq!(infer("{ let x: Int = 5; x }"), Type::Int);
    }

    #[test]
    fn let_type_annotation_mismatch_is_an_error() {
        infer_err("{ let x: Bool = 5; x }");
    }

    #[test]
    fn combined_block_if_arithmetic_comparison() {
        let env = TypeEnv::new().extend("n".to_string(), Type::Int);
        assert_eq!(
            infer_in("{ let doubled = n * 2; if doubled > 10 { doubled } else { 0 } }", &env),
            Type::Int
        );
    }

    // --- Explicit, honest gaps — not yet supported ---

    #[test]
    fn match_is_not_yet_supported() {
        infer_err("match x { _ => 1 }");
    }

    #[test]
    fn struct_literal_is_not_yet_supported() {
        infer_err("Point { x: 1.0 }");
    }

    #[test]
    fn call_is_not_yet_supported() {
        infer_err("f(1)");
    }

    #[test]
    fn closure_is_not_yet_supported() {
        infer_err("|x| x");
    }

    #[test]
    fn pipe_is_not_yet_supported() {
        infer_err("x |> f");
    }

    #[test]
    fn assign_statement_is_not_yet_supported() {
        infer_err("{ let mut x = 5; x = 6; x }");
    }
}
