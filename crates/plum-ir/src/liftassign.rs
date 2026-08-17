//! Lifts an `Assign` out of value position into statement position.
//!
//! # Why
//!
//! Assignment is a STATEMENT in this backend: `codegen_expr`'s `Assign` arm
//! works by threading an updated `Env` into whatever follows, because a
//! `let mut` variable is an SSA register rather than a stack slot. That has
//! no analogue in `codegen_value`, which returns a register and a type and
//! cannot hand an `Env` back to its caller — so an `Assign` reached as a
//! VALUE was the one construct left hitting codegen's
//! "does not yet support this construct" catch-all:
//!
//! ```text
//! twice({ sum = sum + 1; sum })      // Assign as a call argument
//! let y = { sum = sum + 1; sum };    // ...as a Let's value
//! 1 + { sum = sum + 1; sum }         // ...as an operand
//! ```
//!
//! Rather than teach `codegen_value` to return an environment, this moves
//! the assignment to where the existing machinery already handles it:
//!
//! ```text
//! N(.., Assign { n, v, rest }, ..)  =>  Assign { n, v, N(.., rest, ..) }
//! ```
//!
//! # What makes it safe
//!
//! The rewrite moves the assignment EARLIER — ahead of everything in the
//! node that was evaluated before the slot it came from. So it only applies
//! when every preceding slot is order-independent with respect to it: a
//! literal, or a variable other than the one being assigned. Anything else
//! (a call, another assignment, a read of the assigned name) leaves the
//! `Assign` where it is, and the pre-existing clear error stands. Declining
//! costs nothing that worked before.
//!
//! Only slots evaluated UNCONDITIONALLY are considered. Lifting out of an
//! `If` branch, a `Match` arm, a loop body or a closure body would perform
//! an assignment the program does not, or perform it a different number of
//! times. `Let`'s value slot qualifies but its body does not — hoisting
//! past the `Let` would move the assignment ahead of the value it binds.

use crate::fbip::{for_each_child, map_children_owned};
use crate::ir::{Expr, MatchArm, Program};

/// Lifts value-position assignments throughout a program.
pub fn lift_value_assigns(program: Program) -> Program {
    let mut counter = 0usize;
    Program {
        functions: program
            .functions
            .into_iter()
            .map(|f| crate::ir::Function {
                name: f.name,
                params: f.params,
                body: lift(f.body, &mut counter),
            })
            .collect(),
        globals: program
            .globals
            .into_iter()
            .map(|g| crate::ir::Global {
                name: g.name,
                value: lift(g.value, &mut counter),
            })
            .collect(),
        externs: program.externs,
    }
}

/// Whether evaluating `expr` commutes with an assignment to `name`.
///
/// A literal reads and writes nothing. A variable other than `name` is
/// unaffected by the assignment, and cannot affect it. `Str`/`EmptyArray`
/// allocate, which is not observable in either order.
fn commutes_with_assign(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Var(n) => n != name,
        Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Unit | Expr::Str(_) | Expr::EmptyArray(_) => true,
        _ => false,
    }
}

/// Whether every child of `expr` is an unconditionally-evaluated value
/// slot, so the generic lift below may treat them uniformly.
///
/// `Let`, `Match`, `Assign`, `If`, `For`, `Closure`, `Spawn`, `Select` and
/// `RcAnnotated` are all absent: each has at least one child that is
/// deferred, conditional, or repeated. The three that carry a useful value
/// slot (`Let`'s value, `Match`'s scrutinee, `Assign`'s own value) are
/// handled explicitly instead.
fn all_children_are_value_slots(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Unary(..)
            | Expr::Binary(..)
            | Expr::Call { .. }
            | Expr::ExternCall { .. }
            | Expr::Ctor { .. }
            | Expr::CtorReuse { .. }
            | Expr::AsCStr(_)
            | Expr::AsString(_)
            | Expr::ToIntTrunc(_)
            | Expr::ToIntRound(_)
            | Expr::ToFloat(_)
            | Expr::Index { .. }
            | Expr::ArrayLen { .. }
            | Expr::ArrayPop { .. }
            | Expr::ArrayPush { .. }
            | Expr::ArraySet { .. }
            | Expr::ArrayRemove { .. }
            | Expr::ArrayPushReuse { .. }
            | Expr::ArraySetReuse { .. }
            | Expr::ArrayRemoveReuse { .. }
            | Expr::StrConcat { .. }
            | Expr::StrConcatReuse { .. }
            | Expr::StrRunes { .. }
            | Expr::StrTrim { .. }
            | Expr::StrSplit { .. }
            | Expr::StrToUpper { .. }
            | Expr::StrToLower { .. }
            | Expr::StrContains { .. }
            | Expr::StrStartsWith { .. }
            | Expr::StrEndsWith { .. }
            | Expr::StrReplace { .. }
            | Expr::StrReplaceReuse { .. }
            | Expr::ToString { .. }
            | Expr::StrHash { .. }
            | Expr::RefNew { .. }
            | Expr::RefGet { .. }
            | Expr::RefSet { .. }
            | Expr::TaskJoin { .. }
            | Expr::ChannelSend { .. }
            | Expr::ChannelRecv { .. }
            | Expr::ReadFileRaw { .. }
            | Expr::WriteFileRaw { .. }
            | Expr::EnvVarRaw { .. }
            | Expr::PanicRaw { .. }
    )
}

fn lift(expr: Expr, counter: &mut usize) -> Expr {
    // Children first, so an inner lift has already surfaced its assignment
    // into a slot this level can see.
    let expr = map_children_owned(expr, &mut |c| lift(c, counter));
    match try_lift(&expr, counter) {
        // The node this assignment came out of may still hold another one —
        // a second operand, a later argument — so the remainder is lifted
        // again. Each round removes one `Assign` from a value slot, so this
        // terminates.
        Some(Expr::Assign { name, value, rest }) => Expr::Assign {
            name,
            value,
            rest: Box::new(lift(*rest, counter)),
        },
        Some(other) => lift(other, counter),
        None => expr,
    }
}

/// One lift, or `None` if none applies here.
fn try_lift(expr: &Expr, counter: &mut usize) -> Option<Expr> {
    // `Let`'s VALUE, `Match`'s SCRUTINEE and `Assign`'s own VALUE are the
    // three useful value slots on nodes whose other children are not. Each
    // is the FIRST thing evaluated, so nothing precedes it to commute with.
    match expr {
        Expr::Let { name, value, body } => {
            if let Expr::Assign { name: an, value: av, rest } = value.as_ref() {
                return Some(Expr::Assign {
                    name: an.clone(),
                    value: av.clone(),
                    rest: Box::new(Expr::Let {
                        name: name.clone(),
                        value: rest.clone(),
                        body: body.clone(),
                    }),
                });
            }
            return None;
        }
        // An `If`'s CONDITION is unconditional and evaluated first, so the
        // same reasoning applies — the branches are not, and are left alone.
        Expr::If { cond, then_branch, else_branch } => {
            if let Expr::Assign { name: an, value: av, rest } = cond.as_ref() {
                return Some(Expr::Assign {
                    name: an.clone(),
                    value: av.clone(),
                    rest: Box::new(Expr::If {
                        cond: rest.clone(),
                        then_branch: then_branch.clone(),
                        else_branch: else_branch.clone(),
                    }),
                });
            }
            return None;
        }
        // A loop's BOUNDS are evaluated once, before it runs; its body is
        // not, and is left alone.
        Expr::For { var, start, end, body } => {
            if let Expr::Assign { name: an, value: av, rest } = start.as_ref() {
                return Some(Expr::Assign {
                    name: an.clone(),
                    value: av.clone(),
                    rest: Box::new(Expr::For {
                        var: var.clone(),
                        start: rest.clone(),
                        end: end.clone(),
                        body: body.clone(),
                    }),
                });
            }
            if let Expr::Assign { name: an, value: av, rest } = end.as_ref() {
                // `start` runs first, so it must commute.
                if commutes_with_assign(start, an) {
                    return Some(Expr::Assign {
                        name: an.clone(),
                        value: av.clone(),
                        rest: Box::new(Expr::For {
                            var: var.clone(),
                            start: start.clone(),
                            end: rest.clone(),
                            body: body.clone(),
                        }),
                    });
                }
            }
            return None;
        }
        Expr::Match { scrutinee, arms } => {
            if let Expr::Assign { name: an, value: av, rest } = scrutinee.as_ref() {
                return Some(Expr::Assign {
                    name: an.clone(),
                    value: av.clone(),
                    rest: Box::new(Expr::Match {
                        scrutinee: rest.clone(),
                        arms: arms.clone(),
                    }),
                });
            }
            return None;
        }
        Expr::Assign { name, value, rest } => {
            if let Expr::Assign { name: an, value: av, rest: inner } = value.as_ref() {
                return Some(Expr::Assign {
                    name: an.clone(),
                    value: av.clone(),
                    rest: Box::new(Expr::Assign {
                        name: name.clone(),
                        value: inner.clone(),
                        rest: rest.clone(),
                    }),
                });
            }
            return None;
        }
        _ => {}
    }
    if !all_children_are_value_slots(expr) {
        return None;
    }

    // Find the first child that is an `Assign`, and check that everything
    // evaluated before it commutes with that assignment.
    let mut slots: Vec<&Expr> = Vec::new();
    for_each_child(expr, &mut |c| slots.push(c));
    let target = slots.iter().position(|c| matches!(c, Expr::Assign { .. }))?;
    let Expr::Assign { name, value, rest } = slots[target] else {
        unreachable!("position just matched this shape");
    };
    // A preceding slot that does NOT commute is bound to a temporary first.
    // The `Let` evaluates it exactly where it was, so the order is
    // unchanged — and what remains in the slot is a fresh `Var`, which the
    // assignment may then move past. `sum + { sum = sum + 10; sum }` becomes
    // `let t = sum; { sum = sum + 10; t + sum }`, which is the same program.
    if !slots[..target].iter().all(|c| commutes_with_assign(c, name)) {
        let mut binds: Vec<(String, Expr)> = Vec::new();
        let mut replacements: Vec<Option<String>> = Vec::with_capacity(slots.len());
        for (i, c) in slots.iter().enumerate() {
            if i < target && !commutes_with_assign(c, name) {
                *counter += 1;
                // `$` is unavailable to Plum identifiers, so this cannot
                // collide with a user name.
                let t = format!("liftassign${counter}");
                binds.push((t.clone(), (*c).clone()));
                replacements.push(Some(t));
            } else {
                replacements.push(None);
            }
        }
        let mut i = 0usize;
        let bound = map_children_owned(expr.clone(), &mut |c| {
            let here = i;
            i += 1;
            match &replacements[here] {
                Some(t) => Expr::Var(t.clone()),
                None => c,
            }
        });
        let mut out = bound;
        for (t, v) in binds.into_iter().rev() {
            out = Expr::Let {
                name: t,
                value: Box::new(v),
                body: Box::new(out),
            };
        }
        return Some(out);
    }

    let rest = rest.clone();
    let mut i = 0usize;
    let rebuilt = map_children_owned(expr.clone(), &mut |c| {
        let here = i;
        i += 1;
        if here == target {
            (*rest).clone()
        } else {
            c
        }
    });
    Some(Expr::Assign {
        name: name.clone(),
        value: value.clone(),
        rest: Box::new(rebuilt),
    })
}

/// Unused, but kept so `MatchArm` stays imported for the `Match` clone
/// above without a bare `#[allow]`.
#[allow(dead_code)]
fn _arm_type_witness(a: MatchArm) -> MatchArm {
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BinOp;

    fn var(n: &str) -> Expr {
        Expr::Var(n.to_string())
    }

    fn lift(e: Expr) -> Expr {
        let mut c = 0usize;
        super::lift(e, &mut c)
    }

    /// `{ sum = sum + 1; sum }`
    fn bump() -> Expr {
        Expr::Assign {
            name: "sum".to_string(),
            value: Box::new(Expr::Binary(BinOp::Add, Box::new(var("sum")), Box::new(Expr::Int(1)))),
            rest: Box::new(var("sum")),
        }
    }

    fn call(f: &str, args: Vec<Expr>) -> Expr {
        Expr::Call { callee: Box::new(var(f)), args }
    }

    #[test]
    fn an_assign_as_a_call_argument_is_lifted_out() {
        // `twice({ sum = sum + 1; sum })` — the shape DESIGN.md listed as
        // the last construct reaching codegen's catch-all.
        let out = lift(call("twice", vec![bump()]));
        let Expr::Assign { name, rest, .. } = &out else {
            panic!("expected the Assign to be lifted, got {out:?}");
        };
        assert_eq!(name, "sum");
        assert_eq!(**rest, call("twice", vec![var("sum")]));
    }

    #[test]
    fn an_assign_as_a_lets_value_is_lifted_out() {
        let out = lift(Expr::Let {
            name: "y".to_string(),
            value: Box::new(bump()),
            body: Box::new(var("y")),
        });
        let Expr::Assign { rest, .. } = &out else {
            panic!("expected a lift, got {out:?}");
        };
        assert!(matches!(rest.as_ref(), Expr::Let { .. }), "got {rest:?}");
    }

    #[test]
    fn an_assign_as_a_binary_operand_is_lifted_out() {
        let out = lift(Expr::Binary(BinOp::Add, Box::new(Expr::Int(1)), Box::new(bump())));
        let Expr::Assign { rest, .. } = &out else {
            panic!("expected a lift, got {out:?}");
        };
        assert_eq!(
            **rest,
            Expr::Binary(BinOp::Add, Box::new(Expr::Int(1)), Box::new(var("sum")))
        );
    }

    #[test]
    fn a_preceding_read_of_the_assigned_name_is_bound_first() {
        // `sum + { sum = sum + 1; sum }`. Lifting directly would make the
        // left operand see the NEW value, so it is bound to a temporary
        // where it stood — same order, and the assignment can then move
        // past a fresh `Var`.
        let e = Expr::Binary(BinOp::Add, Box::new(var("sum")), Box::new(bump()));
        let out = lift(e);
        let Expr::Let { name: t, value, body } = &out else {
            panic!("expected the left operand bound first, got {out:?}");
        };
        assert!(t.starts_with("liftassign$"), "expected a fresh name, got {t}");
        assert_eq!(**value, var("sum"), "and bound to the OLD value");
        let Expr::Assign { name, rest, .. } = body.as_ref() else {
            panic!("then the assignment lifts: {body:?}");
        };
        assert_eq!(name, "sum");
        assert_eq!(
            **rest,
            Expr::Binary(BinOp::Add, Box::new(var(t)), Box::new(var("sum"))),
            "left operand reads the temporary, right reads the new value"
        );
    }

    #[test]
    fn a_preceding_call_is_bound_first_so_it_still_runs_first() {
        // The call may read or write `sum`, so it must not be reordered
        // across the assignment — binding it pins it in place.
        let e = Expr::Binary(BinOp::Add, Box::new(call("f", vec![])), Box::new(bump()));
        let out = lift(e);
        let Expr::Let { value, body, .. } = &out else {
            panic!("expected the call bound first, got {out:?}");
        };
        assert_eq!(**value, call("f", vec![]), "the call keeps its position");
        assert!(matches!(body.as_ref(), Expr::Assign { .. }), "got {body:?}");
    }

    #[test]
    fn a_preceding_unrelated_variable_does_not_block_the_lift() {
        let e = Expr::Binary(BinOp::Add, Box::new(var("other")), Box::new(bump()));
        let out = lift(e);
        assert!(matches!(out, Expr::Assign { .. }), "got {out:?}");
    }

    #[test]
    fn an_assign_inside_an_if_branch_is_left_alone() {
        // Only one branch runs; lifting would assign unconditionally.
        let e = Expr::If {
            cond: Box::new(Expr::Bool(true)),
            then_branch: Box::new(bump()),
            else_branch: Box::new(Expr::Int(0)),
        };
        assert_eq!(lift(e.clone()), e);
    }

    #[test]
    fn an_assign_inside_a_loop_body_is_left_alone() {
        // Lifting would run it once instead of per iteration.
        let e = Expr::For {
            var: "i".to_string(),
            start: Box::new(Expr::Int(0)),
            end: Box::new(Expr::Int(3)),
            body: Box::new(bump()),
        };
        assert_eq!(lift(e.clone()), e);
    }

    #[test]
    fn two_assigns_in_one_node_are_both_lifted_in_order() {
        // Evaluation order must survive: the left operand's assignment
        // still happens first.
        let second = Expr::Assign {
            name: "other".to_string(),
            value: Box::new(Expr::Int(9)),
            rest: Box::new(var("other")),
        };
        let out = lift(Expr::Binary(BinOp::Add, Box::new(bump()), Box::new(second)));
        let Expr::Assign { name: first, rest, .. } = &out else {
            panic!("expected a lift, got {out:?}");
        };
        assert_eq!(first, "sum", "the left operand's assignment must come first");
        let Expr::Assign { name: nested, .. } = rest.as_ref() else {
            panic!("expected both to lift, got {rest:?}");
        };
        assert_eq!(nested, "other");
    }

    #[test]
    fn an_expression_with_no_value_position_assign_is_unchanged() {
        let e = Expr::Binary(BinOp::Add, Box::new(var("a")), Box::new(Expr::Int(1)));
        assert_eq!(lift(e.clone()), e);
    }
}
