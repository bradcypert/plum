use crate::ir;
use plum_syntax::ast;

pub fn lower_expr(expr: &ast::Expr) -> Result<ir::Expr, String> {
    match expr {
        ast::Expr::Int(n, _) => Ok(ir::Expr::Int(*n)),
        ast::Expr::Float(f, _) => Ok(ir::Expr::Float(*f)),
        ast::Expr::Str(s, _) => Ok(ir::Expr::Str(s.clone())),
        ast::Expr::Bool(b, _) => Ok(ir::Expr::Bool(*b)),
        ast::Expr::Ident(name, _) => Ok(ir::Expr::Var(name.clone())),
        ast::Expr::Tuple(elems, _) if elems.is_empty() => Ok(ir::Expr::Unit),
        ast::Expr::Tuple(_, span) => Err(format!(
            "lowering not yet implemented for non-empty tuples (heap-allocated \
             — waits for the same pass as structs/enums) at {span:?}"
        )),
        ast::Expr::Unary { op, expr, .. } => {
            let ir_op = match op {
                ast::UnaryOp::Neg => ir::UnOp::Neg,
                ast::UnaryOp::Not => ir::UnOp::Not,
            };
            Ok(ir::Expr::Unary(ir_op, Box::new(lower_expr(expr)?)))
        }
        ast::Expr::Binary {
            op: ast::BinaryOp::Pipe,
            lhs,
            rhs,
            ..
        } => lower_pipe(lhs, rhs),
        ast::Expr::Binary {
            op: ast::BinaryOp::Range,
            span,
            ..
        } => Err(format!(
            "lowering not yet implemented for ranges (waits for `for`-loop \
             lowering) at {span:?}"
        )),
        ast::Expr::Binary { op, lhs, rhs, .. } => Ok(ir::Expr::Binary(
            lower_binop(op),
            Box::new(lower_expr(lhs)?),
            Box::new(lower_expr(rhs)?),
        )),
        ast::Expr::Block(block, _) => lower_block(block),
        ast::Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let ir_cond = lower_expr(cond)?;
            let ir_then = lower_block(then_branch)?;
            let ir_else = match else_branch {
                Some(e) => lower_expr(e)?,
                None => ir::Expr::Unit,
            };
            Ok(ir::Expr::If {
                cond: Box::new(ir_cond),
                then_branch: Box::new(ir_then),
                else_branch: Box::new(ir_else),
            })
        }
        ast::Expr::Call { callee, args, .. } => {
            let ir_callee = lower_expr(callee)?;
            let ir_args = args.iter().map(lower_expr).collect::<Result<_, _>>()?;
            Ok(ir::Expr::Call {
                callee: Box::new(ir_callee),
                args: ir_args,
            })
        }
        // Everything heap-shaped (structs, enum variants, closures) or
        // requiring pattern-matching machinery (match) waits for the
        // FBIP pass itself — see this module's and ir.rs's scope notes.
        other => Err(format!(
            "lowering not yet implemented for this expression form at {:?}",
            other.span()
        )),
    }
}

// `x |> rhs` inserts `x` as the LAST argument of the call `rhs`
// denotes; a bare identifier with no parens is treated as a
// zero-argument call before insertion. This is DESIGN.md's pipe
// desugaring rule, and it's a compile-time rewrite, not a runtime
// capability — it doesn't need currying to work, see DESIGN.md.
fn lower_pipe(lhs: &ast::Expr, rhs: &ast::Expr) -> Result<ir::Expr, String> {
    let ir_lhs = lower_expr(lhs)?;
    match rhs {
        ast::Expr::Call { callee, args, .. } => {
            let mut ir_args: Vec<ir::Expr> = args.iter().map(lower_expr).collect::<Result<_, _>>()?;
            ir_args.push(ir_lhs);
            Ok(ir::Expr::Call {
                callee: Box::new(lower_expr(callee)?),
                args: ir_args,
            })
        }
        other => Ok(ir::Expr::Call {
            callee: Box::new(lower_expr(other)?),
            args: vec![ir_lhs],
        }),
    }
}

fn lower_binop(op: &ast::BinaryOp) -> ir::BinOp {
    match op {
        ast::BinaryOp::Add => ir::BinOp::Add,
        ast::BinaryOp::Sub => ir::BinOp::Sub,
        ast::BinaryOp::Mul => ir::BinOp::Mul,
        ast::BinaryOp::Div => ir::BinOp::Div,
        ast::BinaryOp::Rem => ir::BinOp::Rem,
        ast::BinaryOp::Eq => ir::BinOp::Eq,
        ast::BinaryOp::Ne => ir::BinOp::Ne,
        ast::BinaryOp::Lt => ir::BinOp::Lt,
        ast::BinaryOp::Gt => ir::BinOp::Gt,
        ast::BinaryOp::Le => ir::BinOp::Le,
        ast::BinaryOp::Ge => ir::BinOp::Ge,
        ast::BinaryOp::And => ir::BinOp::And,
        ast::BinaryOp::Or => ir::BinOp::Or,
        ast::BinaryOp::Range | ast::BinaryOp::Pipe => {
            unreachable!("Range and Pipe are handled before lower_binop is called")
        }
    }
}

// Folds a block's statement list into nested `let`s, right to left — a
// discarded expression-statement becomes `let _ = expr in rest`, the
// standard way to represent sequencing without a dedicated IR node.
fn lower_block(block: &ast::Block) -> Result<ir::Expr, String> {
    let mut result = match &block.tail {
        Some(t) => lower_expr(t)?,
        None => ir::Expr::Unit,
    };
    for stmt in block.stmts.iter().rev() {
        result = match stmt {
            ast::Stmt::Let { pattern, value, .. } => {
                let name = plain_ident(pattern)?;
                ir::Expr::Let {
                    name,
                    value: Box::new(lower_expr(value)?),
                    body: Box::new(result),
                }
            }
            ast::Stmt::Expr(e) => ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(lower_expr(e)?),
                body: Box::new(result),
            },
            ast::Stmt::Assign { span, .. } => {
                return Err(format!(
                    "lowering not yet implemented for assignment statements \
                     (mutable-slot IR representation not yet designed) at {span:?}"
                ));
            }
        };
    }
    Ok(result)
}

fn plain_ident(pattern: &ast::Pattern) -> Result<String, String> {
    match pattern {
        ast::Pattern::Ident(name, _) => Ok(name.clone()),
        other => Err(format!(
            "lowering not yet implemented for destructuring let-bindings \
             (only plain identifiers so far) at {:?}",
            other.span()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    fn lower(src: &str) -> ir::Expr {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        lower_expr(&ast).unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"))
    }

    fn lower_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        lower_expr(&ast).expect_err(&format!("expected lowering of {src:?} to fail"))
    }

    #[test]
    fn literals() {
        assert_eq!(lower("5"), ir::Expr::Int(5));
        assert_eq!(lower("3.14"), ir::Expr::Float(3.14));
        assert_eq!(lower("\"hi\""), ir::Expr::Str("hi".to_string()));
        assert_eq!(lower("true"), ir::Expr::Bool(true));
        assert_eq!(lower("false"), ir::Expr::Bool(false));
    }

    #[test]
    fn empty_tuple_is_unit() {
        assert_eq!(lower("()"), ir::Expr::Unit);
    }

    #[test]
    fn variable() {
        assert_eq!(lower("x"), ir::Expr::Var("x".to_string()));
    }

    #[test]
    fn unary_ops() {
        assert_eq!(
            lower("-5"),
            ir::Expr::Unary(ir::UnOp::Neg, Box::new(ir::Expr::Int(5)))
        );
        assert_eq!(
            lower("!flag"),
            ir::Expr::Unary(ir::UnOp::Not, Box::new(ir::Expr::Var("flag".to_string())))
        );
    }

    #[test]
    fn binary_arithmetic() {
        assert_eq!(
            lower("1 + 2"),
            ir::Expr::Binary(
                ir::BinOp::Add,
                Box::new(ir::Expr::Int(1)),
                Box::new(ir::Expr::Int(2))
            )
        );
    }

    #[test]
    fn binary_all_operators_map_correctly() {
        let cases = [
            ("a - b", ir::BinOp::Sub),
            ("a * b", ir::BinOp::Mul),
            ("a / b", ir::BinOp::Div),
            ("a % b", ir::BinOp::Rem),
            ("a == b", ir::BinOp::Eq),
            ("a != b", ir::BinOp::Ne),
            ("a < b", ir::BinOp::Lt),
            ("a > b", ir::BinOp::Gt),
            ("a <= b", ir::BinOp::Le),
            ("a >= b", ir::BinOp::Ge),
            ("a && b", ir::BinOp::And),
            ("a || b", ir::BinOp::Or),
        ];
        for (src, expected_op) in cases {
            let expected = ir::Expr::Binary(
                expected_op,
                Box::new(ir::Expr::Var("a".to_string())),
                Box::new(ir::Expr::Var("b".to_string())),
            );
            assert_eq!(lower(src), expected, "mismatch lowering {src:?}");
        }
    }

    #[test]
    fn if_with_else() {
        assert_eq!(
            lower("if true { 1 } else { 2 }"),
            ir::Expr::If {
                cond: Box::new(ir::Expr::Bool(true)),
                then_branch: Box::new(ir::Expr::Int(1)),
                else_branch: Box::new(ir::Expr::Int(2)),
            }
        );
    }

    #[test]
    fn if_without_else_defaults_to_unit() {
        assert_eq!(
            lower("if true { 1 }"),
            ir::Expr::If {
                cond: Box::new(ir::Expr::Bool(true)),
                then_branch: Box::new(ir::Expr::Int(1)),
                else_branch: Box::new(ir::Expr::Unit),
            }
        );
    }

    #[test]
    fn call() {
        assert_eq!(
            lower("f(1, 2)"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![ir::Expr::Int(1), ir::Expr::Int(2)],
            }
        );
    }

    #[test]
    fn call_no_args() {
        assert_eq!(
            lower("f()"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![],
            }
        );
    }

    // --- Pipe desugaring: DESIGN.md's "insert as last argument" rule,
    // implemented for the first time here — the parser deliberately
    // never bakes this in (see ast.rs's BinaryOp::Pipe comment).

    #[test]
    fn pipe_into_bare_identifier() {
        assert_eq!(
            lower("x |> f"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![ir::Expr::Var("x".to_string())],
            }
        );
    }

    #[test]
    fn pipe_into_explicit_call_appends_last() {
        assert_eq!(
            lower("x |> f(a, b)"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![
                    ir::Expr::Var("a".to_string()),
                    ir::Expr::Var("b".to_string()),
                    ir::Expr::Var("x".to_string()),
                ],
            }
        );
    }

    #[test]
    fn pipe_chain_is_nested_calls() {
        assert_eq!(
            lower("x |> f |> g"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("g".to_string())),
                args: vec![ir::Expr::Call {
                    callee: Box::new(ir::Expr::Var("f".to_string())),
                    args: vec![ir::Expr::Var("x".to_string())],
                }],
            }
        );
    }

    #[test]
    fn pipe_lhs_can_be_a_compound_expression() {
        assert_eq!(
            lower("a + b |> f"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("a".to_string())),
                    Box::new(ir::Expr::Var("b".to_string())),
                )],
            }
        );
    }

    // --- Blocks fold into nested lets; a discarded expression-
    // statement is `let _ = expr in rest`, the standard trick for
    // representing sequencing without a dedicated IR node.

    #[test]
    fn empty_block_is_unit() {
        assert_eq!(lower("{}"), ir::Expr::Unit);
    }

    #[test]
    fn block_with_only_tail_has_no_extra_wrapping() {
        assert_eq!(lower("{ 5 }"), ir::Expr::Int(5));
    }

    #[test]
    fn block_let_statement() {
        assert_eq!(
            lower("{ let x = 5; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("x".to_string())),
            }
        );
    }

    #[test]
    fn block_discarded_expression_statement() {
        assert_eq!(
            lower("{ 5; 6 }"),
            ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Int(6)),
            }
        );
    }

    #[test]
    fn block_multiple_lets_nest_in_order() {
        assert_eq!(
            lower("{ let x = 1; let y = 2; x + y }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(1)),
                body: Box::new(ir::Expr::Let {
                    name: "y".to_string(),
                    value: Box::new(ir::Expr::Int(2)),
                    body: Box::new(ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Var("y".to_string())),
                    )),
                }),
            }
        );
    }

    #[test]
    fn block_let_mut_lowers_like_plain_let_for_now() {
        // Mutation itself isn't representable in the IR yet (see
        // block_assign_is_not_yet_supported below) — `let mut` just
        // introduces a binding, same as `let`, until the mutable-slot
        // story is designed alongside FBIP.
        assert_eq!(
            lower("{ let mut x = 5; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("x".to_string())),
            }
        );
    }

    // --- Explicit, honest gaps — not yet supported ---

    #[test]
    fn block_assign_is_not_yet_supported() {
        lower_err("{ let mut x = 5; x = 6; x }");
    }

    #[test]
    fn non_empty_tuple_is_not_yet_supported() {
        lower_err("(1, 2)");
    }

    #[test]
    fn range_is_not_yet_supported() {
        lower_err("0..5");
    }

    #[test]
    fn match_is_not_yet_supported() {
        lower_err("match x { _ => 1 }");
    }

    #[test]
    fn struct_literal_is_not_yet_supported() {
        lower_err("Point { x: 1.0 }");
    }

    #[test]
    fn closure_is_not_yet_supported() {
        lower_err("|x| x");
    }
}
