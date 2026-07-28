use crate::ir;
use plum_syntax::ast;
use std::collections::HashMap;

/// A minimal symbol table, built from a program's `struct` declarations
/// before lowering any expressions that use them. Needed because
/// struct literals are named-field (`Point { y: 2.0, x: 1.0 }`, any
/// order) but the IR's `Ctor` is positional (matching Perceus's own
/// minimal core calculus) — resolving "what position does field `y`
/// go in" requires knowing the struct's DECLARED field order, which a
/// single expression can't know about in isolation. This is the first
/// place lowering needs to be program-aware rather than purely
/// per-expression.
pub struct LoweringContext {
    struct_fields: HashMap<String, Vec<String>>,
}

impl LoweringContext {
    pub fn new() -> Self {
        LoweringContext {
            struct_fields: HashMap::new(),
        }
    }

    pub fn from_items(items: &[ast::Item]) -> Self {
        let mut ctx = Self::new();
        for item in items {
            if let ast::ItemKind::Struct(decl) = &item.kind {
                let fields = decl.fields.iter().map(|f| f.name.clone()).collect();
                ctx.struct_fields.insert(decl.name.clone(), fields);
            }
        }
        ctx
    }
}

impl Default for LoweringContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Lowers a whole program's `let`-defined functions into `ir::Function`s.
/// Only `let` items with 1+ parameters become functions. Deliberately
/// out of scope, both with a clear error rather than a silent skip:
/// zero-param top-level `let`s (a "global" needs its own design — is it
/// referenced bare or called? — not conflated with this), and
/// destructuring params (same restriction as block-level `let`).
/// Generics are simply IGNORED, not rejected — see ir.rs's `Function`
/// doc comment: a type parameter has no runtime effect without a type
/// checker, so this is deliberate erasure.
pub fn lower_program(program: &ast::Program, ctx: &LoweringContext) -> Result<ir::Program, String> {
    let mut functions = Vec::new();
    for item in &program.items {
        if let ast::ItemKind::Let(def) = &item.kind {
            if def.params.is_empty() {
                return Err(format!(
                    "lowering not yet implemented for zero-parameter top-level `let` \
                     (a \"global\" needs its own design) at {:?}",
                    def.span
                ));
            }
            let params = def
                .params
                .iter()
                .map(lower_param_name)
                .collect::<Result<Vec<_>, _>>()?;
            let body = lower_expr(&def.body, ctx)?;
            functions.push(ir::Function {
                name: def.name.clone(),
                params,
                body,
            });
        }
        // struct/enum/extern/use declarations don't produce runtime
        // functions — they're consumed elsewhere (LoweringContext) or
        // not consumed at all yet (extern, use).
    }
    Ok(ir::Program { functions })
}

fn lower_param_name(param: &ast::Param) -> Result<String, String> {
    match &param.kind {
        ast::ParamKind::Ident(name) => Ok(name.clone()),
        ast::ParamKind::Pattern(ast::Pattern::Ident(name, _), _) => Ok(name.clone()),
        _ => Err(format!(
            "lowering not yet implemented for destructuring function parameters at {:?}",
            param.span
        )),
    }
}

pub fn lower_expr(expr: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, String> {
    match expr {
        ast::Expr::Int(n, _) => Ok(ir::Expr::Int(*n)),
        ast::Expr::Float(f, _) => Ok(ir::Expr::Float(*f)),
        ast::Expr::Str(s, _) => Ok(ir::Expr::Str(s.clone())),
        ast::Expr::Bool(b, _) => Ok(ir::Expr::Bool(*b)),
        ast::Expr::Ident(name, _) => Ok(ir::Expr::Var(name.clone())),
        ast::Expr::Tuple(elems, _) if elems.is_empty() => Ok(ir::Expr::Unit),
        ast::Expr::Tuple(_, span) => Err(format!(
            "lowering not yet implemented for non-empty tuples (heap-allocated \
             — waits for its own pass) at {span:?}"
        )),
        ast::Expr::Unary { op, expr, .. } => {
            let ir_op = match op {
                ast::UnaryOp::Neg => ir::UnOp::Neg,
                ast::UnaryOp::Not => ir::UnOp::Not,
            };
            Ok(ir::Expr::Unary(ir_op, Box::new(lower_expr(expr, ctx)?)))
        }
        ast::Expr::Binary {
            op: ast::BinaryOp::Pipe,
            lhs,
            rhs,
            ..
        } => lower_pipe(lhs, rhs, ctx),
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
            Box::new(lower_expr(lhs, ctx)?),
            Box::new(lower_expr(rhs, ctx)?),
        )),
        ast::Expr::Block(block, _) => lower_block(block, ctx),
        ast::Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let ir_cond = lower_expr(cond, ctx)?;
            let ir_then = lower_block(then_branch, ctx)?;
            let ir_else = match else_branch {
                Some(e) => lower_expr(e, ctx)?,
                None => ir::Expr::Unit,
            };
            Ok(ir::Expr::If {
                cond: Box::new(ir_cond),
                then_branch: Box::new(ir_then),
                else_branch: Box::new(ir_else),
            })
        }
        ast::Expr::Call { callee, args, .. } => {
            let ir_callee = lower_expr(callee, ctx)?;
            let ir_args = args.iter().map(|a| lower_expr(a, ctx)).collect::<Result<_, _>>()?;
            Ok(ir::Expr::Call {
                callee: Box::new(ir_callee),
                args: ir_args,
            })
        }
        ast::Expr::StructLiteral {
            path,
            fields,
            spread,
            span,
        } => lower_struct_literal(path, fields, spread, *span, ctx),
        ast::Expr::Match { scrutinee, arms, .. } => lower_match(scrutinee, arms, ctx),
        ast::Expr::For { pattern, iter, body, span } => lower_for(pattern, iter, body, *span, ctx),
        // `unsafe` has nothing to mark yet — no IR operation is
        // unsafe-only (no raw pointers, no unchecked ops), so the block
        // lowers exactly as if the keyword weren't there. When the
        // language grows something `unsafe` actually gates, THAT'S
        // what needs an IR-level marker, not the block itself.
        ast::Expr::Unsafe(block, _) => lower_block(block, ctx),
        // The concurrency MODEL itself is Decided (see DESIGN.md) —
        // what's actually blocking this is unresolved: a `Value::
        // HeapRef` is only meaningful within the single `Heap` that
        // allocated it, so a value sent across a channel to a task
        // running on another thread's `Interpreter` couldn't resolve.
        // See DESIGN.md's "Implementation blocker: heap ownership
        // across tasks" for the real options under consideration.
        ast::Expr::Spawn(_, span) => Err(format!(
            "lowering not yet implemented for `spawn` (blocked on heap ownership across \
             tasks, not the concurrency model itself — see DESIGN.md) at {span:?}"
        )),
        // Unlike function params, a closure param is ALWAYS a plain
        // identifier at the AST level (`ClosureParam` has no Pattern
        // case) — no destructuring restriction to enforce here.
        // Annotations are ignored, same as everywhere else lowering
        // erases them.
        ast::Expr::Closure { params, body, .. } => Ok(ir::Expr::Closure {
            params: params.iter().map(|p| p.name.clone()).collect(),
            body: Box::new(lower_expr(body, ctx)?),
        }),
        // Field access, generic instantiation, and indexing are all
        // still deferred — none of them are needed to validate
        // struct/match lowering.
        other => Err(format!(
            "lowering not yet implemented for this expression form at {:?}",
            other.span()
        )),
    }
}

fn lower_struct_literal(
    path: &[String],
    fields: &[ast::FieldInit],
    spread: &Option<Box<ast::Expr>>,
    span: plum_syntax::span::Span,
    ctx: &LoweringContext,
) -> Result<ir::Expr, String> {
    if spread.is_some() {
        return Err(format!(
            "lowering not yet implemented for struct update/spread syntax (`..expr`) at {span:?}"
        ));
    }
    let tag = path.last().cloned().expect("a path always has at least one segment");
    let Some(declared_fields) = ctx.struct_fields.get(&tag) else {
        return Err(format!(
            "unknown struct type {tag:?} at {span:?} (no declaration found in this lowering context)"
        ));
    };

    let mut by_name: HashMap<&str, &ast::Expr> = HashMap::new();
    for f in fields {
        if by_name.insert(f.name.as_str(), &f.value).is_some() {
            return Err(format!("field {:?} specified more than once at {:?}", f.name, f.span));
        }
    }

    let mut ir_fields = Vec::with_capacity(declared_fields.len());
    for declared_name in declared_fields {
        let Some(value_expr) = by_name.remove(declared_name.as_str()) else {
            return Err(format!("missing field {declared_name:?} for struct {tag:?} at {span:?}"));
        };
        ir_fields.push(lower_expr(value_expr, ctx)?);
    }
    if let Some((extra_name, _)) = by_name.into_iter().next() {
        return Err(format!("struct {tag:?} has no field named {extra_name:?} (at {span:?})"));
    }

    Ok(ir::Expr::Ctor { tag, fields: ir_fields })
}

fn lower_match(scrutinee: &ast::Expr, arms: &[ast::MatchArm], ctx: &LoweringContext) -> Result<ir::Expr, String> {
    let ir_scrutinee = lower_expr(scrutinee, ctx)?;
    let mut ir_arms = Vec::with_capacity(arms.len());
    for arm in arms {
        if arm.guard.is_some() {
            return Err(format!(
                "lowering not yet implemented for match guards at {:?}",
                arm.span
            ));
        }
        let (tag, bindings) = lower_variant_pattern(&arm.pattern)?;
        let body = lower_expr(&arm.body, ctx)?;
        ir_arms.push(ir::MatchArm { tag, bindings, body });
    }
    Ok(ir::Expr::Match {
        scrutinee: Box::new(ir_scrutinee),
        arms: ir_arms,
    })
}

// `for pattern in iter { body }`. Only two things are supported so far,
// both erroring loudly rather than silently otherwise:
//   - `pattern` must be a plain identifier — same restriction as
//     function/let-binding patterns elsewhere in this file, for the
//     same reason (destructuring needs its own pass).
//   - `iter` must be a literal Range (`start..end`) written directly —
//     no array/list/collection type exists yet at the IR level to
//     iterate over otherwise, so anything else (a variable, a call
//     result, even a variable that HOLDS a range) can't be lowered
//     until one does.
fn lower_for(
    pattern: &ast::Pattern,
    iter: &ast::Expr,
    body: &ast::Block,
    span: plum_syntax::span::Span,
    ctx: &LoweringContext,
) -> Result<ir::Expr, String> {
    let var = match pattern {
        ast::Pattern::Ident(name, _) => name.clone(),
        other => {
            return Err(format!(
                "lowering not yet implemented for destructuring `for` patterns at {:?}",
                other.span()
            ));
        }
    };
    let ast::Expr::Binary {
        op: ast::BinaryOp::Range,
        lhs,
        rhs,
        ..
    } = iter
    else {
        return Err(format!(
            "lowering not yet implemented for `for` over anything but a literal range \
             (`start..end`) — no array/list/collection type exists yet at {span:?}"
        ));
    };
    Ok(ir::Expr::For {
        var,
        start: Box::new(lower_expr(lhs, ctx)?),
        end: Box::new(lower_expr(rhs, ctx)?),
        body: Box::new(lower_block(body, ctx)?),
    })
}

// Only the simple `Path(bindings...)` / bare `Path` shape our minimal
// IR Match can represent. Notably NOT supported yet: a bare wildcard
// `_` as a whole arm (no "default arm" concept exists in the IR —
// Match dispatches strictly by tag; see ir.rs's scope note), or-
// patterns, literal patterns, struct patterns, tuple patterns, and
// nested patterns inside a variant's args (only `Ident`/`Wildcard`
// args are supported, since those map directly onto a positional
// binding name).
fn lower_variant_pattern(pattern: &ast::Pattern) -> Result<(String, Vec<String>), String> {
    match pattern {
        ast::Pattern::Variant { path, args, .. } => {
            let tag = path.last().cloned().expect("a path always has at least one segment");
            let mut bindings = Vec::with_capacity(args.len());
            for arg in args {
                match arg {
                    ast::Pattern::Ident(name, _) => bindings.push(name.clone()),
                    // `_` can never collide with a real user binding —
                    // the lexer treats it as a distinct Underscore
                    // token, not a valid Ident, so no genuine Plum
                    // variable can ever be named "_".
                    ast::Pattern::Wildcard(_) => bindings.push("_".to_string()),
                    other => {
                        return Err(format!(
                            "lowering not yet implemented for nested patterns inside a \
                             variant arm's arguments at {:?}",
                            other.span()
                        ));
                    }
                }
            }
            Ok((tag, bindings))
        }
        other => Err(format!(
            "lowering not yet implemented for this pattern shape as a match arm at {:?}",
            other.span()
        )),
    }
}

// `x |> rhs` inserts `x` as the LAST argument of the call `rhs`
// denotes; a bare identifier with no parens is treated as a
// zero-argument call before insertion. This is DESIGN.md's pipe
// desugaring rule, and it's a compile-time rewrite, not a runtime
// capability — it doesn't need currying to work, see DESIGN.md.
fn lower_pipe(lhs: &ast::Expr, rhs: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, String> {
    let ir_lhs = lower_expr(lhs, ctx)?;
    match rhs {
        ast::Expr::Call { callee, args, .. } => {
            let mut ir_args: Vec<ir::Expr> =
                args.iter().map(|a| lower_expr(a, ctx)).collect::<Result<_, _>>()?;
            ir_args.push(ir_lhs);
            Ok(ir::Expr::Call {
                callee: Box::new(lower_expr(callee, ctx)?),
                args: ir_args,
            })
        }
        other => Ok(ir::Expr::Call {
            callee: Box::new(lower_expr(other, ctx)?),
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
fn lower_block(block: &ast::Block, ctx: &LoweringContext) -> Result<ir::Expr, String> {
    let mut result = match &block.tail {
        Some(t) => lower_expr(t, ctx)?,
        None => ir::Expr::Unit,
    };
    for stmt in block.stmts.iter().rev() {
        result = match stmt {
            ast::Stmt::Let { pattern, value, .. } => {
                let name = plain_ident(pattern)?;
                ir::Expr::Let {
                    name,
                    value: Box::new(lower_expr(value, ctx)?),
                    body: Box::new(result),
                }
            }
            ast::Stmt::Expr(e) => ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(lower_expr(e, ctx)?),
                body: Box::new(result),
            },
            // Nothing checks `name` was actually declared `let mut` —
            // that's a static mutability check that doesn't exist yet
            // at this layer (or any layer); see ir.rs's `Assign` doc
            // comment.
            ast::Stmt::Assign { name, value, .. } => ir::Expr::Assign {
                name: name.clone(),
                value: Box::new(lower_expr(value, ctx)?),
                rest: Box::new(result),
            },
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
        lower_with(src, &LoweringContext::new())
    }

    fn lower_err(src: &str) -> String {
        lower_with_err(src, &LoweringContext::new())
    }

    fn lower_with(src: &str, ctx: &LoweringContext) -> ir::Expr {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        lower_expr(&ast, ctx).unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"))
    }

    fn lower_with_err(src: &str, ctx: &LoweringContext) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        lower_expr(&ast, ctx).expect_err(&format!("expected lowering of {src:?} to fail"))
    }

    fn context_from_program(src: &str) -> LoweringContext {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        LoweringContext::from_items(&program.items)
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
    fn block_let_mut_lowers_like_plain_let() {
        // `let mut` itself still just introduces an ordinary binding —
        // `Assign` (below) is the new node, not a different flavor of
        // `Let`. Nothing at this layer distinguishes a `let mut`
        // binding from a plain one; see ir.rs's `Assign` doc comment
        // for why that's a deliberate, documented gap for now.
        assert_eq!(
            lower("{ let mut x = 5; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("x".to_string())),
            }
        );
    }

    #[test]
    fn block_assign_lowers_to_ir_assign() {
        assert_eq!(
            lower("{ let mut x = 5; x = 6; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Assign {
                    name: "x".to_string(),
                    value: Box::new(ir::Expr::Int(6)),
                    rest: Box::new(ir::Expr::Var("x".to_string())),
                }),
            }
        );
    }

    #[test]
    fn block_multiple_assigns_nest_in_order() {
        assert_eq!(
            lower("{ let mut x = 0; x = 1; x = 2; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(0)),
                body: Box::new(ir::Expr::Assign {
                    name: "x".to_string(),
                    value: Box::new(ir::Expr::Int(1)),
                    rest: Box::new(ir::Expr::Assign {
                        name: "x".to_string(),
                        value: Box::new(ir::Expr::Int(2)),
                        rest: Box::new(ir::Expr::Var("x".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn assign_value_can_reference_the_current_binding() {
        // The classic accumulator shape: `sum = sum + i`.
        assert_eq!(
            lower("{ let mut sum = 0; sum = sum + 1; sum }"),
            ir::Expr::Let {
                name: "sum".to_string(),
                value: Box::new(ir::Expr::Int(0)),
                body: Box::new(ir::Expr::Assign {
                    name: "sum".to_string(),
                    value: Box::new(ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("sum".to_string())),
                        Box::new(ir::Expr::Int(1)),
                    )),
                    rest: Box::new(ir::Expr::Var("sum".to_string())),
                }),
            }
        );
    }

    // --- Struct literals: need a LoweringContext to resolve declared
    // field order, since a literal can specify fields in any order but
    // the IR's Ctor is positional.

    #[test]
    fn struct_literal_basic() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        assert_eq!(
            lower_with("Point { x: 1.0, y: 2.0 }", &ctx),
            ir::Expr::Ctor {
                tag: "Point".to_string(),
                fields: vec![ir::Expr::Float(1.0), ir::Expr::Float(2.0)],
            }
        );
    }

    #[test]
    fn struct_literal_field_order_is_independent_of_declared_order() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        // Fields written in the OPPOSITE order from the declaration —
        // the resulting Ctor must still put x before y, since that's
        // Ctor's positional slot 0 and 1 by declaration, not by
        // whatever order the programmer happened to write them in.
        assert_eq!(
            lower_with("Point { y: 2.0, x: 1.0 }", &ctx),
            ir::Expr::Ctor {
                tag: "Point".to_string(),
                fields: vec![ir::Expr::Float(1.0), ir::Expr::Float(2.0)],
            }
        );
    }

    #[test]
    fn struct_literal_unknown_type_is_an_error() {
        lower_with_err("Foo { x: 1.0 }", &LoweringContext::new());
    }

    #[test]
    fn struct_literal_missing_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0 }", &ctx);
    }

    #[test]
    fn struct_literal_unknown_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0, y: 2.0, z: 3.0 }", &ctx);
    }

    #[test]
    fn struct_literal_duplicate_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0, x: 2.0 }", &ctx);
    }

    #[test]
    fn struct_literal_spread_is_not_yet_supported() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0, ..other }", &ctx);
    }

    // --- Match: variant patterns lower to tag + positional bindings.

    #[test]
    fn match_variant_arms() {
        assert_eq!(
            lower("match shape { Shape.Circle(r) => r, Shape.Rectangle(w, h) => w * h }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "Circle".to_string(),
                        bindings: vec!["r".to_string()],
                        body: ir::Expr::Var("r".to_string()),
                    },
                    ir::MatchArm {
                        tag: "Rectangle".to_string(),
                        bindings: vec!["w".to_string(), "h".to_string()],
                        body: ir::Expr::Binary(
                            ir::BinOp::Mul,
                            Box::new(ir::Expr::Var("w".to_string())),
                            Box::new(ir::Expr::Var("h".to_string())),
                        ),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_zero_arg_variant() {
        assert_eq!(
            lower("match x { None => 0, Some(v) => v }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "None".to_string(),
                        bindings: vec![],
                        body: ir::Expr::Int(0),
                    },
                    ir::MatchArm {
                        tag: "Some".to_string(),
                        bindings: vec!["v".to_string()],
                        body: ir::Expr::Var("v".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_variant_with_wildcard_args() {
        assert_eq!(
            lower("match shape { Shape.Rectangle(_, _) => true }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![ir::MatchArm {
                    tag: "Rectangle".to_string(),
                    bindings: vec!["_".to_string(), "_".to_string()],
                    body: ir::Expr::Bool(true),
                }],
            }
        );
    }

    #[test]
    fn match_bare_wildcard_arm_is_not_yet_supported() {
        // No "default arm" concept exists in the IR's Match yet — it
        // dispatches strictly by tag. A bare `_` as a WHOLE arm (as
        // opposed to `_` used inside a variant's args, which works
        // fine — see match_variant_with_wildcard_args) needs a real
        // IR extension, deliberately deferred.
        lower_err("match x { _ => 1 }");
    }

    #[test]
    fn match_or_pattern_is_not_yet_supported() {
        lower_err("match x { A(v) | B(v) => v }");
    }

    #[test]
    fn match_guard_is_not_yet_supported() {
        lower_err("match x { A(v) if v > 0 => v, B(v) => v }");
    }

    // --- End to end: real parsed program source, not a synthetic
    // one-off expression — proves a struct declaration and its use
    // genuinely connect through a full parse_program() call.

    #[test]
    fn end_to_end_struct_from_real_program_source() {
        let src = "struct Point { x: Float, y: Float }\nlet origin = Point { x: 0.0, y: 0.0 }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);

        let ast::ItemKind::Let(def) = &program.items[1].kind else {
            panic!("expected the second item to be a let definition");
        };
        assert_eq!(
            lower_expr(&def.body, &ctx).unwrap(),
            ir::Expr::Ctor {
                tag: "Point".to_string(),
                fields: vec![ir::Expr::Float(0.0), ir::Expr::Float(0.0)],
            }
        );
    }

    // --- Explicit, honest gaps — not yet supported ---

    #[test]
    fn non_empty_tuple_is_not_yet_supported() {
        lower_err("(1, 2)");
    }

    #[test]
    fn range_is_not_yet_supported() {
        lower_err("0..5");
    }

    // --- Closures ---

    #[test]
    fn closure_lowers_to_ir_closure() {
        assert_eq!(
            lower("|x| x + 1"),
            ir::Expr::Closure {
                params: vec!["x".to_string()],
                body: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("x".to_string())),
                    Box::new(ir::Expr::Int(1)),
                )),
            }
        );
    }

    #[test]
    fn closure_multiple_params() {
        assert_eq!(
            lower("|a, b| a + b"),
            ir::Expr::Closure {
                params: vec!["a".to_string(), "b".to_string()],
                body: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("a".to_string())),
                    Box::new(ir::Expr::Var("b".to_string())),
                )),
            }
        );
    }

    #[test]
    fn closure_zero_params() {
        assert_eq!(
            lower("|| 5"),
            ir::Expr::Closure {
                params: vec![],
                body: Box::new(ir::Expr::Int(5)),
            }
        );
    }

    #[test]
    fn closure_param_annotations_do_not_affect_lowering() {
        assert_eq!(lower("|x: Int| x"), lower("|x| x"));
    }

    #[test]
    fn closure_body_can_be_a_block() {
        assert_eq!(
            lower("|x| { let y = x + 1; y }"),
            ir::Expr::Closure {
                params: vec!["x".to_string()],
                body: Box::new(ir::Expr::Let {
                    name: "y".to_string(),
                    value: Box::new(ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Int(1)),
                    )),
                    body: Box::new(ir::Expr::Var("y".to_string())),
                }),
            }
        );
    }

    // --- `for`/`unsafe`/`spawn` ---

    #[test]
    fn for_over_a_literal_range_lowers_to_ir_for() {
        assert_eq!(
            lower("for i in 0..5 { i }"),
            ir::Expr::For {
                var: "i".to_string(),
                start: Box::new(ir::Expr::Int(0)),
                end: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("i".to_string())),
            }
        );
    }

    #[test]
    fn for_range_bounds_can_be_arbitrary_expressions() {
        assert_eq!(
            lower("for i in a..(b + 1) { i }"),
            ir::Expr::For {
                var: "i".to_string(),
                start: Box::new(ir::Expr::Var("a".to_string())),
                end: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("b".to_string())),
                    Box::new(ir::Expr::Int(1)),
                )),
                body: Box::new(ir::Expr::Var("i".to_string())),
            }
        );
    }

    #[test]
    fn for_body_is_a_real_block_with_statements() {
        assert_eq!(
            lower("for i in 0..5 { let x = i; x }"),
            ir::Expr::For {
                var: "i".to_string(),
                start: Box::new(ir::Expr::Int(0)),
                end: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Let {
                    name: "x".to_string(),
                    value: Box::new(ir::Expr::Var("i".to_string())),
                    body: Box::new(ir::Expr::Var("x".to_string())),
                }),
            }
        );
    }

    #[test]
    fn for_over_anything_but_a_literal_range_is_not_yet_supported() {
        // No array/list/collection type exists yet at the IR level —
        // not even a variable that HAPPENS to hold a range works, since
        // there's no Range value, only the literal syntax.
        lower_err("for i in xs { i }");
    }

    #[test]
    fn for_destructuring_pattern_is_not_yet_supported() {
        lower_err("for (a, b) in 0..5 { a }");
    }

    #[test]
    fn unsafe_block_lowers_transparently() {
        assert_eq!(lower("unsafe { 1 + 2 }"), lower("{ 1 + 2 }"));
    }

    #[test]
    fn spawn_is_not_yet_supported() {
        // The concurrency MODEL is Decided now — this is blocked on a
        // real open implementation question (heap ownership across
        // tasks), not an undecided design. See DESIGN.md.
        lower_err("spawn { 1 }");
    }

    // --- Item-level lowering: `let`-defined functions -> ir::Function

    fn lower_program(src: &str) -> ir::Program {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        super::lower_program(&program, &ctx).unwrap_or_else(|e| panic!("program lowering error for {src:?}: {e}"))
    }

    fn lower_program_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        super::lower_program(&program, &ctx).expect_err(&format!("expected lowering of {src:?} to fail"))
    }

    #[test]
    fn single_param_function() {
        let program = lower_program("let double n = n * 2");
        assert_eq!(
            program.functions,
            vec![ir::Function {
                name: "double".to_string(),
                params: vec!["n".to_string()],
                body: ir::Expr::Binary(
                    ir::BinOp::Mul,
                    Box::new(ir::Expr::Var("n".to_string())),
                    Box::new(ir::Expr::Int(2)),
                ),
            }]
        );
    }

    #[test]
    fn annotations_do_not_affect_lowering() {
        let annotated = lower_program("let double (n: Int): Int = n * 2");
        let bare = lower_program("let double n = n * 2");
        assert_eq!(annotated.functions, bare.functions);
    }

    #[test]
    fn multi_param_function() {
        let program = lower_program("let add a b = a + b");
        assert_eq!(program.functions[0].params, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn generics_are_ignored_not_rejected() {
        // No type checker exists yet, so a type parameter has no
        // runtime effect — this is deliberate erasure, not a missing
        // feature. Proven by lowering succeeding at all here.
        let program = lower_program("let identity[T] (x: T): T = x");
        assert_eq!(program.functions[0].params, vec!["x".to_string()]);
    }

    #[test]
    fn struct_and_enum_and_use_items_produce_no_functions() {
        let program = lower_program(
            "struct Point { x: Float, y: Float }\n\
             enum Shape { Circle(Float) }\n\
             use shapes;\n\
             let double n = n * 2",
        );
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].name, "double");
    }

    #[test]
    fn multiple_functions_lower_in_order() {
        let program = lower_program("let square x = x * x\nlet cube x = x * x * x");
        let names: Vec<&str> = program.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["square", "cube"]);
    }

    #[test]
    fn zero_param_let_is_not_yet_supported() {
        // Deliberately deferred: a zero-param top-level `let` should
        // be referenced bare (`x`), not called (`x()`) — supporting
        // that needs "evaluate globals eagerly into the environment"
        // machinery this pass doesn't build. Loud error, not a silent
        // skip, so this isn't mistaken for "just doesn't show up."
        lower_program_err("let x = 5");
    }

    #[test]
    fn destructuring_param_is_not_yet_supported() {
        lower_program_err("let swap (a, b) = (b, a)");
    }
}
