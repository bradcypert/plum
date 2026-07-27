use crate::ir::{Expr, MatchArm, RcOp};
use std::collections::HashSet;

/// Inserts explicit refcount inc/dec operations via last-use analysis
/// (the first half of Perceus-style FBIP — see DESIGN.md's memory
/// model section). Reuse-in-place (the second half: recognizing a
/// deconstruct-then-construct-same-shape pattern and skipping the
/// allocation when the scrutinee's refcount is 1) is a later pass on
/// top of this one, not implemented yet.
///
/// Scoping note: only names PROVABLY heap-shaped without a type
/// checker — a direct `Ctor` construction, or a variable aliased from
/// one — get refcount treatment. Call results and match results are
/// conservatively left untouched, since we don't yet know their type.
pub fn insert_refcount_ops(expr: Expr) -> Expr {
    transform(expr, &HashSet::new())
}

/// Walks the whole tree, and at every `Let`, decides whether the bound
/// name is heap-shaped and — if so — runs `mark_last_uses` over its
/// body to insert the actual Inc/Dec ops for that one name.
fn transform(expr: Expr, known_heap: &HashSet<String>) -> Expr {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::Var(_) => expr,
        Expr::Unary(op, e) => Expr::Unary(op, Box::new(transform(*e, known_heap))),
        Expr::Binary(op, l, r) => Expr::Binary(
            op,
            Box::new(transform(*l, known_heap)),
            Box::new(transform(*r, known_heap)),
        ),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => Expr::If {
            cond: Box::new(transform(*cond, known_heap)),
            then_branch: Box::new(transform(*then_branch, known_heap)),
            else_branch: Box::new(transform(*else_branch, known_heap)),
        },
        Expr::Call { callee, args } => Expr::Call {
            callee: Box::new(transform(*callee, known_heap)),
            args: args.into_iter().map(|a| transform(a, known_heap)).collect(),
        },
        Expr::Ctor { tag, fields } => Expr::Ctor {
            tag,
            fields: fields.into_iter().map(|f| transform(f, known_heap)).collect(),
        },
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(transform(*scrutinee, known_heap)),
            arms: arms
                .into_iter()
                .map(|arm| MatchArm {
                    tag: arm.tag,
                    // Arm bindings aren't added to `known_heap` — we
                    // don't know a constructor's field types without a
                    // type checker, same conservative limitation as
                    // call/match results. A future type checker closes
                    // this the same way it closes the others.
                    bindings: arm.bindings,
                    body: transform(arm.body, known_heap),
                })
                .collect(),
        },
        Expr::RcAnnotated { op, target, rest } => Expr::RcAnnotated {
            op,
            target,
            rest: Box::new(transform(*rest, known_heap)),
        },
        Expr::Let { name, value, body } => {
            let is_heap_value = is_syntactically_heap(&value, known_heap);
            let value_t = transform(*value, known_heap);

            let mut inner_heap = known_heap.clone();
            if is_heap_value {
                inner_heap.insert(name.clone());
            }
            let body_t = transform(*body, &inner_heap);

            let final_body = if is_heap_value {
                let (marked, used) = mark_last_uses(body_t, &name, false);
                if used {
                    marked
                } else {
                    // Never referenced at all — dead the moment it's
                    // bound, so drop it immediately rather than never.
                    Expr::RcAnnotated {
                        op: RcOp::Dec,
                        target: name.clone(),
                        rest: Box::new(marked),
                    }
                }
            } else {
                body_t
            };

            Expr::Let {
                name,
                value: Box::new(value_t),
                body: Box::new(final_body),
            }
        }
    }
}

/// A name is provably heap-shaped if it's a direct `Ctor` construction,
/// or a plain alias of an already-known-heap variable. Everything else
/// (call results, match results, literals) is left untracked — see
/// this module's scope note.
fn is_syntactically_heap(expr: &Expr, known_heap: &HashSet<String>) -> bool {
    match expr {
        Expr::Ctor { .. } => true,
        Expr::Var(name) => known_heap.contains(name),
        _ => false,
    }
}

/// The core last-use analysis: walks `expr` and, for every occurrence
/// of `Var(name)`, decides whether it needs a `dup` (Inc) first based
/// on `live_after` — whether `name` is known to be needed again in
/// whatever comes after `expr` in the surrounding context. Processing
/// happens in reverse evaluation order (right-to-left / last-to-first)
/// so that "is this the last use" can be answered by threading
/// liveness backward through the tree, rather than counting occurrences
/// in a separate pass — the same reason real liveness analyses are
/// backward analyses.
///
/// Returns the transformed expression and whether `name` was used
/// anywhere within it (which becomes `live_after` for whatever's
/// processed next, going backward).
fn mark_last_uses(expr: Expr, name: &str, live_after: bool) -> (Expr, bool) {
    match expr {
        Expr::Var(n) if n == name => {
            if live_after {
                (
                    Expr::RcAnnotated {
                        op: RcOp::Inc,
                        target: name.to_string(),
                        rest: Box::new(Expr::Var(n)),
                    },
                    true,
                )
            } else {
                // This IS the last use — ownership just moves here, no
                // annotation needed.
                (Expr::Var(n), true)
            }
        }
        Expr::Var(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit => {
            (expr, live_after)
        }
        Expr::Unary(op, e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::Unary(op, Box::new(e_t)), used)
        }
        Expr::Binary(op, l, r) => {
            // Evaluation order is l-then-r, so process backward: r
            // first (closest to `live_after`), then l fed with
            // whatever r turned out to need.
            let (r_t, used_r) = mark_last_uses(*r, name, live_after);
            let (l_t, used_l) = mark_last_uses(*l, name, live_after || used_r);
            (Expr::Binary(op, Box::new(l_t), Box::new(r_t)), used_l || used_r)
        }
        Expr::Call { callee, args } => {
            let mut acc_used = live_after;
            let mut new_args = Vec::with_capacity(args.len());
            for a in args.into_iter().rev() {
                let (a_t, used) = mark_last_uses(a, name, acc_used);
                acc_used = acc_used || used;
                new_args.push(a_t);
            }
            new_args.reverse();
            let (callee_t, used_callee) = mark_last_uses(*callee, name, acc_used);
            (
                Expr::Call {
                    callee: Box::new(callee_t),
                    args: new_args,
                },
                used_callee || acc_used,
            )
        }
        Expr::Ctor { tag, fields } => {
            let mut acc_used = live_after;
            let mut new_fields = Vec::with_capacity(fields.len());
            for f in fields.into_iter().rev() {
                let (f_t, used) = mark_last_uses(f, name, acc_used);
                acc_used = acc_used || used;
                new_fields.push(f_t);
            }
            new_fields.reverse();
            (
                Expr::Ctor {
                    tag,
                    fields: new_fields,
                },
                acc_used,
            )
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            // then/else are ALTERNATIVES, not a sequence — only one
            // ever runs, so both are processed independently with the
            // SAME live_after, not an accumulated one. This is what
            // stops "used once per branch" from being mistaken for
            // "used twice."
            let (then_t, used_then) = mark_last_uses(*then_branch, name, live_after);
            let (else_t, used_else) = mark_last_uses(*else_branch, name, live_after);
            let (cond_t, used_cond) =
                mark_last_uses(*cond, name, live_after || used_then || used_else);
            (
                Expr::If {
                    cond: Box::new(cond_t),
                    then_branch: Box::new(then_t),
                    else_branch: Box::new(else_t),
                },
                used_cond || used_then || used_else,
            )
        }
        Expr::Match { scrutinee, arms } => {
            // Same "alternatives" treatment as If's branches.
            let mut used_any_arm = false;
            let new_arms: Vec<MatchArm> = arms
                .into_iter()
                .map(|arm| {
                    if arm.bindings.iter().any(|b| b == name) {
                        // This arm shadows `name` via its own bindings
                        // — its body can't refer to the outer name.
                        arm
                    } else {
                        let (body_t, used) = mark_last_uses(arm.body, name, live_after);
                        used_any_arm = used_any_arm || used;
                        MatchArm {
                            tag: arm.tag,
                            bindings: arm.bindings,
                            body: body_t,
                        }
                    }
                })
                .collect();
            let (scrutinee_t, used_scrutinee) =
                mark_last_uses(*scrutinee, name, live_after || used_any_arm);
            (
                Expr::Match {
                    scrutinee: Box::new(scrutinee_t),
                    arms: new_arms,
                },
                used_scrutinee || used_any_arm,
            )
        }
        Expr::RcAnnotated { op, target, rest } => {
            let (rest_t, used) = mark_last_uses(*rest, name, live_after);
            (
                Expr::RcAnnotated {
                    op,
                    target,
                    rest: Box::new(rest_t),
                },
                used,
            )
        }
        Expr::Let {
            name: bound,
            value,
            body,
        } => {
            if bound == name {
                // Shadowed: `body` can only refer to the NEW binding,
                // never the outer one being analyzed here — only
                // `value` (evaluated before the shadow takes effect)
                // can still reference the outer name.
                let (value_t, used_in_value) = mark_last_uses(*value, name, live_after);
                (
                    Expr::Let {
                        name: bound,
                        value: Box::new(value_t),
                        body,
                    },
                    used_in_value,
                )
            } else {
                let (body_t, used_in_body) = mark_last_uses(*body, name, live_after);
                let (value_t, used_in_value) = mark_last_uses(*value, name, used_in_body);
                (
                    Expr::Let {
                        name: bound,
                        value: Box::new(value_t),
                        body: Box::new(body_t),
                    },
                    used_in_value || used_in_body,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BinOp, RcOp};

    // Small constructors so tests read as trees, not boilerplate.
    fn var(name: &str) -> Expr {
        Expr::Var(name.to_string())
    }
    fn int(n: i64) -> Expr {
        Expr::Int(n)
    }
    fn ctor(tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::Ctor {
            tag: tag.to_string(),
            fields,
        }
    }
    fn let_(name: &str, value: Expr, body: Expr) -> Expr {
        Expr::Let {
            name: name.to_string(),
            value: Box::new(value),
            body: Box::new(body),
        }
    }
    fn if_(cond: Expr, then_branch: Expr, else_branch: Expr) -> Expr {
        Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        }
    }
    fn inc(name: &str, rest: Expr) -> Expr {
        Expr::RcAnnotated {
            op: RcOp::Inc,
            target: name.to_string(),
            rest: Box::new(rest),
        }
    }
    fn dec(name: &str, rest: Expr) -> Expr {
        Expr::RcAnnotated {
            op: RcOp::Dec,
            target: name.to_string(),
            rest: Box::new(rest),
        }
    }

    #[test]
    fn single_use_needs_no_rc_ops_at_all() {
        // The value is constructed and immediately returned — its one
        // use is trivially its last use. Ownership just moves.
        let input = let_("p", ctor("Point", vec![int(1), int(2)]), var("p"));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn primitives_never_get_rc_ops_even_with_multiple_uses() {
        // `n` is Int, not a Ctor — unboxed, no header, no refcount
        // traffic, per DESIGN.md, regardless of how many times it's used.
        let input = let_("n", int(5), Expr::Binary(BinOp::Add, Box::new(var("n")), Box::new(var("n"))));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn two_uses_dups_before_the_first_not_the_last() {
        let input = let_(
            "p",
            ctor("Point", vec![]),
            ctor("Pair", vec![var("p"), var("p")]),
        );
        let expected = let_(
            "p",
            ctor("Point", vec![]),
            ctor("Pair", vec![inc("p", var("p")), var("p")]),
        );
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn zero_uses_drops_immediately() {
        let input = let_("p", ctor("Point", vec![]), int(5));
        let expected = let_("p", ctor("Point", vec![]), dec("p", int(5)));
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn aliasing_chain_needs_no_rc_ops_when_each_hop_is_a_last_use() {
        // p -> q -> return q. Each step genuinely only touches the
        // value once, so the whole chain should come out untouched —
        // proving the pass doesn't insert anything unnecessary.
        let input = let_("p", ctor("Point", vec![]), let_("q", var("p"), var("q")));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn branches_are_alternatives_not_a_sequence() {
        // `p` is used once in each branch of an if. Only one branch
        // ever runs, so this must NOT be treated as two uses needing a
        // dup — a naive "sum occurrences" analysis would get this
        // wrong and insert an unnecessary Inc in the `then` branch.
        let input = let_(
            "p",
            ctor("Point", vec![]),
            if_(var("cond"), var("p"), var("p")),
        );
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn shadowing_does_not_confuse_outer_and_inner_bindings() {
        // The outer `p` is shadowed immediately and never actually
        // used — it should get an immediate drop. The inner `p`'s own
        // single use should be left alone, untouched by the outer
        // analysis.
        let input = let_(
            "p",
            ctor("Outer", vec![]),
            let_("p", ctor("Inner", vec![]), var("p")),
        );
        let expected = let_(
            "p",
            ctor("Outer", vec![]),
            dec("p", let_("p", ctor("Inner", vec![]), var("p"))),
        );
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn call_results_are_conservatively_not_tracked() {
        // Without a type checker we don't know what `f(x)` returns, so
        // this pass must not guess it's heap-shaped — even if it's
        // used twice, nothing should be inserted (a real type checker
        // closes this gap later; see this module's scope note).
        let call = Expr::Call {
            callee: Box::new(var("f")),
            args: vec![var("x")],
        };
        let input = let_("r", call, ctor("Pair", vec![var("r"), var("r")]));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }
}
