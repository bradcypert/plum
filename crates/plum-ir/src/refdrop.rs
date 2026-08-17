//! Release for `Ref[T]` cells — the borrow-aware counterpart to
//! `fbip`'s Perceus pass, run last in `optimize_program`.
//!
//! # Why this is a separate pass
//!
//! `fbip`'s `insert_refcount_ops` assumes **the last use of a value
//! CONSUMES it**, so it never emits a trailing decrement: ownership just
//! moves into whatever the last use was. That is true for a `Ctor`
//! field, a call argument, a return — and false for `.get()`/`.set()`,
//! which only BORROW their base. The reference is read and then dropped
//! on the floor, so a `ref(v)` cell was never freed at all: 63MB for
//! 2,000,000 `ref()` calls.
//!
//! Adding `RefNew` to `fbip::is_syntactically_heap` to fix that was
//! tried and reverted (see DESIGN.md): it produced increments with no
//! matching decrements, strictly worse than the leak. A `Ref` needs the
//! opposite treatment from everything `fbip` handles, so it gets its own
//! pass rather than a wider predicate in that one.
//!
//! The separation is airtight because a `Ref` binding never enters
//! `fbip`'s `known_heap` at all (`is_syntactically_heap` has no `RefNew`
//! arm, and `Var(n)` only qualifies if `n` is already in the set). So
//! `fbip` provably ignores exactly the bindings this pass owns, and this
//! pass ignores everything else. It also means a `Ref` can never become
//! a reuse-in-place candidate — `mark_reuse` is gated on the same set —
//! which matters: a `Ref` must ALWAYS mutate in place and ALWAYS stay
//! visible through every alias, and its cell has no tag word for a match
//! arm to overwrite as a `Ctor`.
//!
//! # The rule
//!
//! For a variable bound to a `Ref` cell:
//!
//! 1. Every CONSUMING use gets an `Inc`.
//! 2. Exactly one `Dec` at the end of the binding's scope.
//! 3. BORROWING uses get nothing.
//!
//! Borrowing uses need no increment because the binding holds the
//! original reference alive until (3) runs, so anything reading through
//! the pointer inside that scope is safe by construction.
//!
//! Rule 1 is what makes rule 2 unconditionally safe, including when the
//! cell escapes. `let make_cell (n: Int): Ref[Int] = { let r = ref(n); r }`
//! is legal Plum that works today: the body's own result IS the cell.
//! The bare `Var(r)` in return position is a consuming use, so it is
//! incremented, and the scope-end decrement then releases only the
//! binding's own reference — the caller receives a live cell. A naive
//! scope-end decrement without rule 1 would be a use-after-free here,
//! which is exactly what made the earlier one-line attempt unsalvageable.
//!
//! # Expressing "decrement at scope end"
//!
//! `RcAnnotated { op: Dec, target, rest }` decrements BEFORE evaluating
//! `rest`, and this IR has no "decrement after this expression produces
//! its value" node. So the scope-end decrement is expressed by binding
//! the body's result to a synthetic temporary first:
//!
//! ```text
//! Let { r, RefNew(v), body }
//!   =>
//! Let { r, RefNew(v), Let { tmp, body', RcAnnotated { Dec, r, Var(tmp) } } }
//! ```
//!
//! One consequence worth naming: `body` is no longer in tail position,
//! so a call at the end of it stops being a `musttail` candidate. That
//! is not a lost optimization but a correctness requirement — a function
//! holding a `Ref` cell has cleanup to run after the call returns, so it
//! genuinely cannot tail-call away its own frame.

use crate::ir::{Expr, MatchArm, RcOp, SelectArm};
use std::collections::HashSet;

/// Inserts `Ref` increments/decrements over a whole function body.
pub fn insert_ref_drops(expr: Expr) -> Expr {
    let mut counter = 0usize;
    rewrite(expr, &HashSet::new(), &mut counter)
}

/// Whether `value` binds a `Ref` cell: either a fresh `ref(v)`, or an
/// alias of a name already known to hold one (`let b = a`).
///
/// An alias counts so that `b` gets its own increment/decrement pair —
/// `let b = a` is a consuming use of `a`, so the cell's count goes to 2
/// and both scopes release one.
fn binds_ref(value: &Expr, refs: &HashSet<String>) -> bool {
    match value {
        Expr::RefNew { .. } => true,
        Expr::Var(n) => refs.contains(n),
        _ => false,
    }
}

/// Walks `expr`, rewriting every `Let` that binds a `Ref` cell.
///
/// `refs` is the set of in-scope names known to hold a `Ref`. Shadowing
/// is handled precisely (not over-approximated): a `Let`/`For`/match-arm
/// binding that reuses a name REMOVES it from the set for its own scope,
/// so an unrelated inner `x` never inherits an outer `Ref` `x`'s
/// treatment.
fn rewrite(expr: Expr, refs: &HashSet<String>, counter: &mut usize) -> Expr {
    match expr {
        Expr::Let { name, value, body } => {
            let is_ref = binds_ref(&value, refs);
            let value_t = rewrite(*value, refs, counter);
            let mut inner = refs.clone();
            if is_ref {
                inner.insert(name.clone());
            } else {
                inner.remove(&name);
            }
            let body_t = rewrite(*body, &inner, counter);
            if !is_ref {
                return Expr::Let {
                    name,
                    value: Box::new(value_t),
                    body: Box::new(body_t),
                };
            }
            let body_m = mark(body_t, &name);
            *counter += 1;
            // `$` can't appear in a Plum identifier, so this can never
            // collide with a user name — the same guarantee
            // `monomorphize`'s mangled names rely on.
            let tmp = format!("refdrop${counter}");
            Expr::Let {
                name: name.clone(),
                value: Box::new(value_t),
                body: Box::new(Expr::Let {
                    name: tmp.clone(),
                    value: Box::new(body_m),
                    body: Box::new(Expr::RcAnnotated {
                        op: RcOp::Dec,
                        target: name,
                        rest: Box::new(Expr::Var(tmp)),
                    }),
                }),
            }
        }
        // Every other binder removes its name from `refs` for the scope
        // it governs, for the same shadowing reason as `Let` above.
        Expr::For { var, start, end, body } => {
            let mut inner = refs.clone();
            inner.remove(&var);
            Expr::For {
                var,
                start: Box::new(rewrite(*start, refs, counter)),
                end: Box::new(rewrite(*end, refs, counter)),
                body: Box::new(rewrite(*body, &inner, counter)),
            }
        }
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(rewrite(*scrutinee, refs, counter)),
            arms: arms
                .into_iter()
                .map(|arm| {
                    let mut inner = refs.clone();
                    for b in &arm.bindings {
                        inner.remove(b);
                    }
                    MatchArm {
                        tag: arm.tag,
                        bindings: arm.bindings,
                        guard: arm.guard.map(|g| Box::new(rewrite(*g, &inner, counter))),
                        body: rewrite(arm.body, &inner, counter),
                    }
                })
                .collect(),
        },
        Expr::Closure { params, param_types, ret_type, body } => {
            let mut inner = refs.clone();
            for p in &params {
                inner.remove(p);
            }
            Expr::Closure {
                params,
                param_types,
                ret_type,
                body: Box::new(rewrite(*body, &inner, counter)),
            }
        }
        other => map_children(other, &mut |c| rewrite(c, refs, counter)),
    }
}

/// Annotates every CONSUMING use of `name` inside `expr` with an `Inc`,
/// leaving BORROWING uses untouched. See this module's doc comment for
/// the rule and why borrows need nothing.
fn mark(expr: Expr, name: &str) -> Expr {
    match expr {
        // A bare use in any position not special-cased below consumes.
        Expr::Var(n) if n == name => Expr::RcAnnotated {
            op: RcOp::Inc,
            target: name.to_string(),
            rest: Box::new(Expr::Var(n)),
        },
        // `.get()`/`.set()` read through the pointer and hand back
        // nothing that owns the cell — the defining borrow positions,
        // and the whole reason this pass exists.
        Expr::RefGet { base } => Expr::RefGet { base: Box::new(mark_borrowed(*base, name)) },
        Expr::RefSet { base, value } => Expr::RefSet {
            base: Box::new(mark_borrowed(*base, name)),
            value: Box::new(mark(*value, name)),
        },
        // `Ref` equality is a raw pointer comparison (identity, see
        // DESIGN.md's "Mutability and cycles"), so it borrows both
        // operands. Only ever reached with a `Ref`-typed `name`, so
        // treating `Binary` as borrowing here says nothing about how
        // arithmetic on other types is handled.
        Expr::Binary(op, l, r) => Expr::Binary(
            op,
            Box::new(mark_borrowed(*l, name)),
            Box::new(mark_borrowed(*r, name)),
        ),
        // NOT descended into: a free variable inside a closure body is a
        // CAPTURE, and codegen already balances captures itself —
        // `codegen_closure_literal` increments each heap-shaped capture
        // as it stores it, and the generated `closure_release$*` function
        // decrements it. Marking here too would double-increment.
        Expr::Closure { .. } => expr_identity(expr),
        // Same reasoning, and moot besides: a `Ref` is rejected from
        // crossing a `spawn` boundary at all.
        Expr::Spawn { .. } => expr_identity(expr),
        // Shadowing: once `name` is rebound, uses below refer to
        // something else. The `value`/bounds are still outside the new
        // binding, so they are marked; the governed scope is not.
        Expr::Let { name: n, value, body } if n == name => Expr::Let {
            name: n,
            value: Box::new(mark(*value, name)),
            body,
        },
        Expr::For { var, start, end, body } if var == name => Expr::For {
            var,
            start: Box::new(mark(*start, name)),
            end: Box::new(mark(*end, name)),
            body,
        },
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(mark(*scrutinee, name)),
            arms: arms
                .into_iter()
                .map(|arm| {
                    if arm.bindings.iter().any(|b| b == name) {
                        arm
                    } else {
                        MatchArm {
                            tag: arm.tag,
                            bindings: arm.bindings,
                            guard: arm.guard.map(|g| Box::new(mark(*g, name))),
                            body: mark(arm.body, name),
                        }
                    }
                })
                .collect(),
        },
        other => map_children(other, &mut |c| mark(c, name)),
    }
}

/// A borrow position: a direct `Var(name)` here needs no increment.
/// Anything else is an ordinary subexpression and is marked normally
/// (the borrow applies to the base slot itself, not to whatever
/// computes it).
fn mark_borrowed(expr: Expr, name: &str) -> Expr {
    if matches!(&expr, Expr::Var(n) if n == name) {
        expr
    } else {
        mark(expr, name)
    }
}

/// Returns `expr` unchanged — a named helper purely so the `Closure`/
/// `Spawn` arms above read as a deliberate decision rather than a
/// forgotten recursion.
fn expr_identity(expr: Expr) -> Expr {
    expr
}

/// Applies `f` to every DIRECT child subexpression of `expr`, rebuilding
/// it. Exhaustive over `Expr` with no `_` arm, deliberately: a new
/// variant carrying a subexpression must fail to compile here rather
/// than silently having its children skipped by both walks above.
///
/// Binder-introducing variants (`Let`/`For`/`Match`/`Closure`) are
/// handled explicitly by each caller BEFORE reaching this function,
/// since each needs its own scope handling; they are still mapped
/// structurally here for the cases where a caller does not intercept
/// them.
fn map_children(expr: Expr, f: &mut dyn FnMut(Expr) -> Expr) -> Expr {
    let mut b = |e: Box<Expr>| Box::new(f(*e));
    match expr {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Unit
        | Expr::Var(_)
        | Expr::EmptyArray(_)
        | Expr::Channel { .. }
        | Expr::ArgsRaw
        | Expr::RandomRaw
        | Expr::ArrayPopReuse { .. }
        | Expr::StrTrimReuse { .. }
        | Expr::StrToUpperReuse { .. }
        | Expr::StrToLowerReuse { .. } => expr,

        Expr::Unary(op, e) => Expr::Unary(op, b(e)),
        Expr::AsCStr(e) => Expr::AsCStr(b(e)),
        Expr::AsString(e) => Expr::AsString(b(e)),
        Expr::ToIntTrunc(e) => Expr::ToIntTrunc(b(e)),
        Expr::ToIntRound(e) => Expr::ToIntRound(b(e)),
        Expr::ToFloat(e) => Expr::ToFloat(b(e)),
        Expr::Binary(op, l, r) => Expr::Binary(op, b(l), b(r)),

        Expr::Let { name, value, body } => Expr::Let { name, value: b(value), body: b(body) },
        Expr::If { cond, then_branch, else_branch } => Expr::If {
            cond: b(cond),
            then_branch: b(then_branch),
            else_branch: b(else_branch),
        },
        Expr::Call { callee, args } => Expr::Call {
            callee: b(callee),
            args: args.into_iter().map(&mut *f).collect(),
        },
        Expr::ExternCall { name, args } => Expr::ExternCall {
            name,
            args: args.into_iter().map(&mut *f).collect(),
        },
        Expr::Ctor { tag, fields } => Expr::Ctor {
            tag,
            fields: fields.into_iter().map(&mut *f).collect(),
        },
        Expr::CtorReuse { reuse_of, tag, fields } => Expr::CtorReuse {
            reuse_of,
            tag,
            fields: fields.into_iter().map(&mut *f).collect(),
        },
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: b(scrutinee),
            arms: arms
                .into_iter()
                .map(|arm| MatchArm {
                    tag: arm.tag,
                    bindings: arm.bindings,
                    guard: arm.guard.map(|g| Box::new(f(*g))),
                    body: f(arm.body),
                })
                .collect(),
        },
        Expr::RcAnnotated { op, target, rest } => Expr::RcAnnotated { op, target, rest: b(rest) },
        Expr::For { var, start, end, body } => Expr::For {
            var,
            start: b(start),
            end: b(end),
            body: b(body),
        },
        Expr::Closure { params, param_types, ret_type, body } => Expr::Closure {
            params,
            param_types,
            ret_type,
            body: b(body),
        },
        Expr::Assign { name, value, rest } => Expr::Assign { name, value: b(value), rest: b(rest) },
        Expr::Spawn { block } => Expr::Spawn { block: b(block) },
        Expr::TaskJoin { task } => Expr::TaskJoin { task: b(task) },
        Expr::ChannelSend { sender, value } => Expr::ChannelSend { sender: b(sender), value: b(value) },
        Expr::ChannelRecv { receiver } => Expr::ChannelRecv { receiver: b(receiver) },
        Expr::Select { arms } => Expr::Select {
            arms: arms
                .into_iter()
                .map(|arm| SelectArm {
                    receiver: f(arm.receiver),
                    body: f(arm.body),
                })
                .collect(),
        },

        Expr::Index { base, index } => Expr::Index { base: b(base), index: b(index) },
        Expr::ArrayLen { array } => Expr::ArrayLen { array: b(array) },
        Expr::ArrayPush { array, value } => Expr::ArrayPush { array: b(array), value: b(value) },
        Expr::ArrayPop { array } => Expr::ArrayPop { array: b(array) },
        Expr::ArraySet { array, index, value } => Expr::ArraySet {
            array: b(array),
            index: b(index),
            value: b(value),
        },
        Expr::ArrayRemove { array, index } => Expr::ArrayRemove { array: b(array), index: b(index) },
        Expr::ArrayPushReuse { reuse_of, value } => Expr::ArrayPushReuse { reuse_of, value: b(value) },
        Expr::ArraySetReuse { reuse_of, index, value } => Expr::ArraySetReuse {
            reuse_of,
            index: b(index),
            value: b(value),
        },
        Expr::ArrayRemoveReuse { reuse_of, index } => Expr::ArrayRemoveReuse { reuse_of, index: b(index) },

        Expr::StrConcat { base, other } => Expr::StrConcat { base: b(base), other: b(other) },
        Expr::StrConcatReuse { reuse_of, other } => Expr::StrConcatReuse { reuse_of, other: b(other) },
        Expr::StrRunes { base } => Expr::StrRunes { base: b(base) },
        Expr::StrTrim { base } => Expr::StrTrim { base: b(base) },
        Expr::StrSplit { base, sep } => Expr::StrSplit { base: b(base), sep: b(sep) },
        Expr::StrToUpper { base } => Expr::StrToUpper { base: b(base) },
        Expr::StrToLower { base } => Expr::StrToLower { base: b(base) },
        Expr::StrContains { base, needle } => Expr::StrContains { base: b(base), needle: b(needle) },
        Expr::StrStartsWith { base, prefix } => Expr::StrStartsWith { base: b(base), prefix: b(prefix) },
        Expr::StrEndsWith { base, suffix } => Expr::StrEndsWith { base: b(base), suffix: b(suffix) },
        Expr::StrReplace { base, from, to } => Expr::StrReplace { base: b(base), from: b(from), to: b(to) },
        Expr::StrReplaceReuse { reuse_of, from, to } => Expr::StrReplaceReuse {
            reuse_of,
            from: b(from),
            to: b(to),
        },
        Expr::ToString { base } => Expr::ToString { base: b(base) },
        Expr::StrHash { base } => Expr::StrHash { base: b(base) },

        Expr::RefNew { value } => Expr::RefNew { value: b(value) },
        Expr::RefGet { base } => Expr::RefGet { base: b(base) },
        Expr::RefSet { base, value } => Expr::RefSet { base: b(base), value: b(value) },

        Expr::ReadFileRaw { path } => Expr::ReadFileRaw { path: b(path) },
        Expr::WriteFileRaw { path, contents } => Expr::WriteFileRaw { path: b(path), contents: b(contents) },
        Expr::EnvVarRaw { name } => Expr::EnvVarRaw { name: b(name) },
        Expr::PanicRaw { message } => Expr::PanicRaw { message: b(message) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BinOp;

    fn var(n: &str) -> Expr {
        Expr::Var(n.to_string())
    }

    fn let_(name: &str, value: Expr, body: Expr) -> Expr {
        Expr::Let { name: name.to_string(), value: Box::new(value), body: Box::new(body) }
    }

    fn ref_new(v: Expr) -> Expr {
        Expr::RefNew { value: Box::new(v) }
    }

    fn get(base: Expr) -> Expr {
        Expr::RefGet { base: Box::new(base) }
    }

    /// Every `Inc`/`Dec` the pass inserted, as `("Inc"|"Dec", target)`
    /// pairs in structural order — enough to assert the counting without
    /// pinning the exact tree shape.
    fn rc_ops(expr: &Expr) -> Vec<(String, String)> {
        let mut out = Vec::new();
        collect(expr, &mut out);
        out
    }

    fn collect(expr: &Expr, out: &mut Vec<(String, String)>) {
        if let Expr::RcAnnotated { op, target, rest } = expr {
            let name = match op {
                RcOp::Inc => "Inc",
                RcOp::Dec => "Dec",
            };
            out.push((name.to_string(), target.clone()));
            collect(rest, out);
            return;
        }
        let mut kids: Vec<&Expr> = Vec::new();
        push_children(expr, &mut kids);
        for k in kids {
            collect(k, out);
        }
    }

    fn push_children<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::Let { value, body, .. } => {
                out.push(value);
                out.push(body);
            }
            Expr::RefNew { value } => out.push(value),
            Expr::RefGet { base } => out.push(base),
            Expr::RefSet { base, value } => {
                out.push(base);
                out.push(value);
            }
            Expr::Binary(_, l, r) => {
                out.push(l);
                out.push(r);
            }
            Expr::Ctor { fields, .. } => out.extend(fields.iter()),
            Expr::Call { callee, args } => {
                out.push(callee);
                out.extend(args.iter());
            }
            Expr::For { start, end, body, .. } => {
                out.push(start);
                out.push(end);
                out.push(body);
            }
            Expr::Closure { body, .. } => out.push(body),
            Expr::Match { scrutinee, arms } => {
                out.push(scrutinee);
                for a in arms {
                    out.push(&a.body);
                }
            }
            Expr::RcAnnotated { rest, .. } => out.push(rest),
            _ => {}
        }
    }

    #[test]
    fn a_ref_bound_and_only_borrowed_gets_exactly_one_dec_and_no_incs() {
        // `let r = ref(0); r.get()` — the shape that leaked entirely
        // before this pass. `.get()` borrows, so no increment; the
        // binding's own reference is released at scope end.
        let e = let_("r", ref_new(Expr::Int(0)), get(var("r")));
        assert_eq!(rc_ops(&insert_ref_drops(e)), vec![("Dec".to_string(), "r".to_string())]);
    }

    #[test]
    fn an_escaping_ref_is_incremented_so_the_scope_end_dec_cannot_free_it() {
        // `let r = ref(0); r` — legal Plum (`make_cell`) that a naive
        // scope-end release turns into a use-after-free. The bare `Var`
        // in return position is a CONSUMING use, so it is incremented
        // first and the caller receives a live cell.
        let e = let_("r", ref_new(Expr::Int(0)), var("r"));
        let ops = rc_ops(&insert_ref_drops(e));
        assert_eq!(
            ops,
            vec![("Inc".to_string(), "r".to_string()), ("Dec".to_string(), "r".to_string())],
            "an escaping Ref must be Inc'd before the scope-end Dec"
        );
    }

    #[test]
    fn borrows_never_increment_however_many_there_are() {
        // Three borrows, still one Dec and zero Incs — the binding holds
        // the cell alive across all of them by construction.
        let body = Expr::Binary(
            BinOp::Add,
            Box::new(get(var("r"))),
            Box::new(Expr::Binary(BinOp::Add, Box::new(get(var("r"))), Box::new(get(var("r"))))),
        );
        let e = let_("r", ref_new(Expr::Int(0)), body);
        assert_eq!(rc_ops(&insert_ref_drops(e)), vec![("Dec".to_string(), "r".to_string())]);
    }

    #[test]
    fn ref_equality_borrows_both_operands() {
        // `Ref` `==` is identity, a raw pointer compare — it takes no
        // ownership, so neither side is incremented.
        let e = let_(
            "r",
            ref_new(Expr::Int(0)),
            Expr::Binary(BinOp::Eq, Box::new(var("r")), Box::new(var("r"))),
        );
        assert_eq!(rc_ops(&insert_ref_drops(e)), vec![("Dec".to_string(), "r".to_string())]);
    }

    #[test]
    fn an_alias_binding_gets_its_own_dec_and_consumes_the_original() {
        // `let a = ref(0); let b = a; b.get()` — `let b = a` consumes
        // `a` (count goes to 2) and `b` becomes a tracked Ref binding of
        // its own, so both scopes release one reference.
        let e = let_("a", ref_new(Expr::Int(0)), let_("b", var("a"), get(var("b"))));
        let ops = rc_ops(&insert_ref_drops(e));
        assert_eq!(
            ops,
            vec![
                ("Inc".to_string(), "a".to_string()),
                ("Dec".to_string(), "b".to_string()),
                ("Dec".to_string(), "a".to_string()),
            ]
        );
    }

    #[test]
    fn storing_a_ref_in_a_ctor_is_a_consuming_use() {
        // The cell escapes into a struct field, so it must be
        // incremented — otherwise the scope-end Dec would free a cell
        // the struct still points at.
        let e = let_(
            "r",
            ref_new(Expr::Int(0)),
            Expr::Ctor { tag: "Holder".to_string(), fields: vec![var("r")] },
        );
        let ops = rc_ops(&insert_ref_drops(e));
        assert_eq!(
            ops,
            vec![("Inc".to_string(), "r".to_string()), ("Dec".to_string(), "r".to_string())]
        );
    }

    #[test]
    fn a_closure_capture_is_left_entirely_alone() {
        // Codegen balances captures itself (`codegen_closure_literal`
        // increments each heap-shaped capture, `closure_release$*`
        // decrements it), so marking here would double-increment.
        let closure = Expr::Closure {
            params: vec!["n".to_string()],
            param_types: None,
            ret_type: None,
            body: Box::new(get(var("r"))),
        };
        let e = let_("r", ref_new(Expr::Int(0)), closure);
        assert_eq!(rc_ops(&insert_ref_drops(e)), vec![("Dec".to_string(), "r".to_string())]);
    }

    #[test]
    fn a_shadowing_binding_does_not_inherit_the_outer_refs_treatment() {
        // The inner `r` is an ordinary Int, not a Ref — it must not pick
        // up the outer `r`'s increments, and the outer's marking must
        // stop at the rebinding.
        let inner = let_("r", Expr::Int(1), var("r"));
        let e = let_("r", ref_new(Expr::Int(0)), inner);
        assert_eq!(rc_ops(&insert_ref_drops(e)), vec![("Dec".to_string(), "r".to_string())]);
    }

    #[test]
    fn a_ref_bound_inside_a_loop_body_is_released_each_iteration() {
        // The unbounded-growth case: without a per-iteration release
        // this leaked one cell per turn of the loop.
        let body = let_("r", ref_new(var("i")), get(var("r")));
        let e = Expr::For {
            var: "i".to_string(),
            start: Box::new(Expr::Int(0)),
            end: Box::new(Expr::Int(10)),
            body: Box::new(body),
        };
        assert_eq!(rc_ops(&insert_ref_drops(e)), vec![("Dec".to_string(), "r".to_string())]);
    }

    #[test]
    fn a_program_with_no_refs_is_returned_structurally_unchanged() {
        // The pass must be provably inert for everything else — verified
        // end to end too (a non-`ref` program's emitted IR is
        // byte-identical before and after this pass existed).
        let e = let_(
            "x",
            Expr::Int(1),
            Expr::Binary(BinOp::Add, Box::new(var("x")), Box::new(Expr::Int(2))),
        );
        assert_eq!(insert_ref_drops(e.clone()), e);
    }
}
