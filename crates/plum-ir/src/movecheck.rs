// Enforces DESIGN.md's "channel send is a move": `tx.send(v)` transfers
// ownership of `v`, so using `v` again afterward is a compile error, not
// a silent re-use of the (already deep-copied-away) old value.
//
// Runs on the AST, not the lowered IR — deliberately. `ir::Expr` is
// span-free by design (see ir.rs's own doc comment), so an IR-level
// pass could never point at a real source location; the AST's `Block`
// (`stmts` + `tail`) is also a much more natural "straight-line
// sequence" to walk than IR's nested `Let`/`Assign` chains. Placed in
// `plum-ir` (not `plum-syntax`) anyway, since it's conceptually paired
// with lowering — it detects the exact SAME `expr.send(value)` call
// shape `lower.rs`'s `Call` arm already special-cases, by the same
// "match the callee's shape" convention established there.
//
// Scope is deliberately narrow — see the module-level doc on
// `check_expr` for exactly what is and isn't tracked. The risk
// direction that shaped every scoping call here: a FALSE POSITIVE
// (wrongly rejecting valid code) is worse than a false negative (a
// missed violation just runs against the already-deep-copied old
// value, IDENTICAL to this language's fully-permissive behavior before
// this pass existed) — so every case this pass can't analyze with
// confidence is left unchecked, not conservatively rejected.

use plum_syntax::ast;
use std::collections::HashSet;

/// Checks every top-level function/global body in `program` for a
/// value used after being sent on a channel. Independent per top-level
/// `let` — nothing about one function's sends affects another's
/// checking, matching how `plum-interp::call` gives each invocation a
/// completely fresh environment.
pub fn check_moves(program: &ast::Program) -> Result<(), String> {
    for item in &program.items {
        if let ast::ItemKind::Let(def) = &item.kind {
            let mut moved = HashSet::new();
            check_expr(&def.body, &mut moved)?;
        }
    }
    Ok(())
}

/// Every name a pattern binds, recursively (including through nested
/// tuple/struct/variant/or-patterns) — used ONLY to know which names a
/// new binding shadows, so shadowing a moved outer name with a fresh
/// one correctly clears its moved status rather than false-positiving
/// on the (unrelated) new binding.
fn pattern_names(pattern: &ast::Pattern, out: &mut Vec<String>) {
    match pattern {
        ast::Pattern::Ident(name, _) => out.push(name.clone()),
        ast::Pattern::Tuple(elems, _) => elems.iter().for_each(|p| pattern_names(p, out)),
        ast::Pattern::Variant { args, .. } => args.iter().for_each(|p| pattern_names(p, out)),
        ast::Pattern::Struct { fields, .. } => fields.iter().for_each(|f| pattern_names(&f.pattern, out)),
        ast::Pattern::Or(alts, _) => alts.iter().for_each(|p| pattern_names(p, out)),
        ast::Pattern::Int(..) | ast::Pattern::Float(..) | ast::Pattern::Str(..) | ast::Pattern::Bool(..) | ast::Pattern::Wildcard(_) => {}
    }
}

/// Walks `expr`, erroring the moment a name already in `moved` is
/// referenced, and adding a name to `moved` the moment it's sent
/// (`tx.send(name)`) — evaluation-order sequential recursion for
/// everything that's genuinely straight-line.
///
/// Three shapes are intentionally NOT propagated through, matching
/// this module's documented "false positives are worse than false
/// negatives" bias:
///   - `If`/`Match`: each branch/arm is checked from a CLONE of the
///     current `moved` set (so a violation already established BEFORE
///     the branch still correctly fires inside it), but nothing newly
///     moved INSIDE a branch is merged back into the outer set
///     afterward — computing this precisely would need Rust-borrow-
///     checker-style intersection-of-branches logic, real complexity
///     this v1 deliberately doesn't take on.
///   - `For`/`Closure`/`Spawn` bodies: checked in complete ISOLATION
///     (a fresh, empty `moved` set) — both because they may run zero,
///     one, or many times, or later/elsewhere entirely (the exact same
///     "escapes this expression" reasoning `fbip.rs` already applies
///     to these three constructs for refcounting), and because a
///     `Spawn`/`Closure` body's own captured names aren't governed by
///     this pass's simple sequential model at all.
fn check_expr(expr: &ast::Expr, moved: &mut HashSet<String>) -> Result<(), String> {
    match expr {
        ast::Expr::Ident(name, span) => {
            if moved.contains(name) {
                return Err(format!(
                    "{name:?} used at {span:?}, but it was already sent on a channel (its last use)"
                ));
            }
            Ok(())
        }
        ast::Expr::Int(..) | ast::Expr::Float(..) | ast::Expr::Str(..) | ast::Expr::Bool(..) => Ok(()),
        ast::Expr::Unary { expr, .. } => check_expr(expr, moved),
        ast::Expr::Binary { lhs, rhs, .. } => {
            check_expr(lhs, moved)?;
            check_expr(rhs, moved)
        }
        ast::Expr::Tuple(elems, _) => elems.iter().try_for_each(|e| check_expr(e, moved)),
        ast::Expr::ArrayLiteral(elems, _) => elems.iter().try_for_each(|e| check_expr(e, moved)),
        ast::Expr::Field { base, .. } => check_expr(base, moved),
        // `expr.send(value)` — checked BEFORE generic `Call` handling,
        // the same shape `lower.rs`'s `Call` arm already special-cases
        // for the identical reason: no type information is available
        // here either, so this goes by callee SHAPE alone. `base` and
        // `value` are both checked for EXISTING violations first (in
        // evaluation order), and only THEN — if `value` is a plain
        // `Ident` — does that name become moved going forward.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "send") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            check_expr(base, moved)?;
            check_expr(&args[0], moved)?;
            if let ast::Expr::Ident(sent_name, _) = &args[0] {
                moved.insert(sent_name.clone());
            }
            Ok(())
        }
        ast::Expr::Call { callee, args, .. } => {
            check_expr(callee, moved)?;
            args.iter().try_for_each(|a| check_expr(a, moved))
        }
        ast::Expr::GenericInst { callee, .. } => check_expr(callee, moved),
        ast::Expr::Index { base, index, .. } => {
            check_expr(base, moved)?;
            check_expr(index, moved)
        }
        ast::Expr::Block(block, _) => check_block(block, moved),
        ast::Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            check_expr(cond, moved)?;
            let mut then_moved = moved.clone();
            check_block(then_branch, &mut then_moved)?;
            if let Some(else_expr) = else_branch {
                let mut else_moved = moved.clone();
                check_expr(else_expr, &mut else_moved)?;
            }
            Ok(())
        }
        ast::Expr::Match { scrutinee, arms, .. } => {
            check_expr(scrutinee, moved)?;
            for arm in arms {
                let mut arm_moved = moved.clone();
                let mut bound = Vec::new();
                pattern_names(&arm.pattern, &mut bound);
                for name in &bound {
                    arm_moved.remove(name);
                }
                if let Some(guard) = &arm.guard {
                    check_expr(guard, &mut arm_moved)?;
                }
                check_expr(&arm.body, &mut arm_moved)?;
            }
            Ok(())
        }
        // `select { pattern = expr => body, ... }` — every arm's
        // `expr` (the channel being received from) is evaluated
        // unconditionally, so those are checked SEQUENTIALLY against
        // the SAME threaded `moved` set, same as `Call`'s args. Only
        // ONE arm's `body` actually runs (whichever channel becomes
        // ready first, decided at runtime) — same "alternatives"
        // treatment as `Match`'s arms just above: each `body` is
        // checked from a CLONE of `moved` (so a violation already
        // established before the `select` still fires inside it), the
        // arm's own pattern-bound name is cleared first (shadowing),
        // and nothing newly moved inside a body propagates back out.
        ast::Expr::Select { arms, .. } => {
            for arm in arms {
                check_expr(&arm.expr, moved)?;
            }
            for arm in arms {
                let mut arm_moved = moved.clone();
                let mut bound = Vec::new();
                pattern_names(&arm.pattern, &mut bound);
                for name in &bound {
                    arm_moved.remove(name);
                }
                check_expr(&arm.body, &mut arm_moved)?;
            }
            Ok(())
        }
        ast::Expr::For { iter, body, .. } => {
            check_expr(iter, moved)?;
            let mut body_moved = HashSet::new();
            check_block(body, &mut body_moved)
        }
        ast::Expr::Closure { body, .. } => {
            let mut body_moved = HashSet::new();
            check_expr(body, &mut body_moved)
        }
        ast::Expr::Unsafe(block, _) => check_block(block, moved),
        ast::Expr::Spawn(block, _) => {
            let mut body_moved = HashSet::new();
            check_block(block, &mut body_moved)
        }
        ast::Expr::StructLiteral { fields, spread, .. } => {
            for f in fields {
                check_expr(&f.value, moved)?;
            }
            if let Some(spread_expr) = spread {
                check_expr(spread_expr, moved)?;
            }
            Ok(())
        }
    }
}

/// A `Block`'s `stmts` then `tail`, in order — the actual "straight-
/// line sequence" this whole pass is built around.
fn check_block(block: &ast::Block, moved: &mut HashSet<String>) -> Result<(), String> {
    for stmt in &block.stmts {
        match stmt {
            ast::Stmt::Let { pattern, value, .. } => {
                check_expr(value, moved)?;
                let mut bound = Vec::new();
                pattern_names(pattern, &mut bound);
                for name in &bound {
                    moved.remove(name);
                }
            }
            ast::Stmt::Assign { name, value, .. } => {
                check_expr(value, moved)?;
                moved.remove(name);
            }
            ast::Stmt::Expr(e) => check_expr(e, moved)?,
        }
    }
    if let Some(tail) = &block.tail {
        check_expr(tail, moved)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    fn check(src: &str) {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        check_moves(&program).unwrap_or_else(|e| panic!("expected {src:?} to pass move-checking, got: {e}"));
    }

    fn check_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        check_moves(&program).expect_err(&format!("expected {src:?} to fail move-checking"))
    }

    #[test]
    fn a_plain_send_with_no_reuse_is_fine() {
        check("let use_it tx p = tx.send(p)");
    }

    #[test]
    fn using_a_value_before_sending_it_is_fine() {
        check("let use_it tx p = { let n = p; tx.send(p) }");
    }

    #[test]
    fn reusing_a_value_after_sending_it_is_an_error() {
        let err = check_err("let use_it tx p = { tx.send(p); p }");
        assert!(err.contains("\"p\""), "expected the error to name p, got: {err}");
    }

    #[test]
    fn reusing_a_value_after_sending_it_as_a_binary_operand_is_an_error() {
        check_err("let use_it tx p = { tx.send(p); p + 1 }");
    }

    #[test]
    fn reusing_a_value_after_sending_it_in_a_later_statement_is_an_error() {
        check_err("let use_it tx p = { tx.send(p); let x = p; x }");
    }

    #[test]
    fn sending_a_value_that_was_already_sent_is_an_error() {
        check_err("let use_it tx p = { tx.send(p); tx.send(p) }");
    }

    #[test]
    fn shadowing_a_sent_name_with_a_fresh_binding_clears_its_moved_status() {
        check("let use_it tx p = { tx.send(p); let p = 5; p }");
    }

    #[test]
    fn reassigning_a_sent_name_clears_its_moved_status() {
        check("let use_it tx p = { tx.send(p); p = 5; p }");
    }

    #[test]
    fn reuse_after_an_if_that_conditionally_sent_is_not_flagged() {
        // `p` is only ACTUALLY sent if `b` is true — deliberately
        // permissive about reuse after the `if` either way, since
        // computing this precisely needs real intersection-of-branches
        // logic this v1 doesn't take on. See `check_expr`'s doc
        // comment.
        check("let use_it tx p b = { if b { tx.send(p) } else { 0 }; p }");
    }

    #[test]
    fn a_violation_that_predates_an_if_still_fires_inside_it() {
        check_err("let use_it tx p b = { tx.send(p); if b { p + 1 } else { 0 } }");
    }

    #[test]
    fn reuse_after_a_match_that_conditionally_sent_is_not_flagged() {
        check("let use_it tx p n = { match n { 0 => tx.send(p), other => 0 }; p }");
    }

    #[test]
    fn reuse_inside_a_for_loop_body_after_an_outer_send_is_not_flagged() {
        // Isolated on purpose — see `check_expr`'s doc comment.
        check("let use_it tx p = { tx.send(p); for i in 0..3 { p } }");
    }

    #[test]
    fn a_send_inside_a_for_loop_body_is_still_checked_within_that_body() {
        check_err("let use_it tx p = for i in 0..3 { tx.send(p); p }");
    }

    #[test]
    fn reuse_inside_a_closure_after_an_outer_send_is_not_flagged() {
        check("let use_it tx p = { tx.send(p); |x| p }");
    }

    #[test]
    fn a_send_inside_a_closure_is_still_checked_within_that_closure() {
        check_err("let use_it tx p = || { tx.send(p); p }");
    }

    #[test]
    fn reuse_inside_a_spawn_block_after_an_outer_send_is_not_flagged() {
        check("let use_it tx p = { tx.send(p); spawn { p } }");
    }

    #[test]
    fn a_send_inside_a_spawn_block_is_still_checked_within_that_block() {
        check_err("let use_it tx p = spawn { tx.send(p); p }");
    }

    #[test]
    fn sending_a_field_access_result_does_not_move_the_base() {
        // `p.x` isn't a bare `Ident`, so nothing gets marked moved.
        check("let use_it tx p = { tx.send(p.x); p }");
    }

    #[test]
    fn a_clean_function_does_not_mask_a_violation_in_another() {
        // `f` sends its OWN `p` with no violation; `g` (a totally
        // separate function, also with a param named `p`) DOES violate
        // — the overall check still correctly fails, not short-
        // circuited to success by `f`'s clean pass.
        check_err("let f tx p = tx.send(p)\nlet g tx p = { tx.send(p); p }");
    }

    #[test]
    fn a_violation_in_one_function_does_not_poison_another() {
        // The reverse: `f`'s own send of `p` doesn't somehow leak into
        // `g`'s checking of an UNRELATED `q`.
        check("let f tx p = tx.send(p)\nlet g tx q = q");
    }

    #[test]
    fn a_struct_literal_field_value_is_checked() {
        check_err("struct Pair { a: Int, b: Int }\nlet use_it tx p = { tx.send(p); Pair { a: p, b: 1 } }");
    }
}
