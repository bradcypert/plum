//! Binds unnamed heap-allocating intermediates to `Let` temporaries, so
//! `fbip`'s scope-end release can reach them.
//!
//! # Why
//!
//! `fbip::all_uses_are_borrows` releases a heap value whose every use is
//! a read — but it can only do that for a value with a NAME, since the
//! release is attached to a `Let`'s scope. An intermediate has no name,
//! so it leaked regardless. Measured, per 1M iterations of a loop:
//!
//! | shape | leaked |
//! | --- | --- |
//! | `"abcdefgh".concat("ijklmnop").len()` | 139.2 MB |
//! | `Point { x: i, y: i }.x` | 47.5 MB |
//! | `i.to_string().len()` | 34.0 MB |
//!
//! Every one is ordinary code; `i.to_string()` especially. Giving each
//! intermediate a name routes them all through machinery that already
//! works, rather than adding a second release mechanism.
//!
//! # What is hoisted, and why the rule is this narrow
//!
//! Only an expression that is BOTH a syntactically fresh allocation
//! (`is_fresh_alloc`) AND has nothing but atoms for children
//! (`hoistable`).
//!
//! The second condition is a soundness requirement, not tidiness.
//! Hoisting moves evaluation EARLIER — before any sibling to its left —
//! so it is only safe for an expression whose sole effect is allocating.
//! A fresh allocation over atoms qualifies: it allocates and stores
//! already-computed words. One over a `Call` does not, because hoisting
//! it would reorder that call against its siblings:
//!
//! ```text
//! f() + Point { x: g() }.x     // hoisting the Ctor runs g() before f()
//! ```
//!
//! Children are processed first, so a nested fresh allocation becomes an
//! atom (a `Var`) before its parent is considered — which is what lets
//! `"a".concat("b")` hoist all three of its allocations rather than none.
//! A fresh allocation that still has a non-atom child after that is left
//! alone, and still leaks. That is a deliberate, documented stopping
//! point: closing it needs a calling convention where a returned value is
//! OWNED, which in turn needs functions to release their own parameters —
//! see DESIGN.md's "gap 1" entry for why that is not a small change.
//!
//! A `Call` is never hoisted for the same reason: this backend's callees
//! do not release their parameters, so a function may return one of them
//! and the caller has no extra reference to release. Treating a call
//! result as owned would be a use-after-free for `let id (p) = p`.
//!
//! # Where the binding goes
//!
//! Immediately around the expression currently being flattened, in
//! evaluation order. That is only correct for child slots evaluated
//! unconditionally as part of that expression, so every DEFERRED or
//! CONDITIONAL slot — an `If` branch, a `Match` arm, a loop body, a
//! closure body, a `Let`'s own body — is instead treated as its own
//! region and flattened independently. Hoisting out of a match arm would
//! evaluate it whether or not the arm was taken; hoisting out of a loop
//! body would evaluate it once instead of per iteration.
//!
//! Run BEFORE `fbip::optimize`, and on the codegen path only — the
//! interpreter has a real tracing-free heap of its own and gains nothing
//! from the extra bindings.

use crate::ir::{Expr, MatchArm, SelectArm};

/// A value needing no evaluation: hoisting anything past it is free, and
/// it never needs hoisting itself.
///
/// `Str` is deliberately NOT an atom — a string literal allocates a cell
/// (see `ir::Expr::Str`), which is exactly why it shows up in the
/// `concat` measurement above.
fn is_atom(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Var(_) | Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Unit
    )
}

/// Whether `expr` always evaluates to a freshly allocated heap cell that
/// this expression therefore owns outright.
///
/// Kept in step with `fbip::allocates_fresh_heap` plus the three literal
/// forms `fbip::is_syntactically_heap` already recognizes — between them
/// those two are the complete set of "this expression owns its result",
/// and a hoisted binding is only useful if `fbip` will agree it is
/// releasable.
///
/// Excluded, each for a reason: `AsString` can return its input register
/// unchanged; every `*Reuse` node can return a cell belonging to another
/// binding; `RefNew` is handled by `refdrop`; `Closure` captures are
/// balanced by codegen itself; `Call` has no owned-return convention.
fn is_fresh_alloc(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Ctor { .. }
            | Expr::Str(_)
            | Expr::EmptyArray(_)
            | Expr::StrConcat { .. }
            | Expr::StrTrim { .. }
            | Expr::StrToUpper { .. }
            | Expr::StrToLower { .. }
            | Expr::StrReplace { .. }
            | Expr::StrRunes { .. }
            | Expr::StrSplit { .. }
            | Expr::ToString { .. }
            | Expr::ArrayPush { .. }
            | Expr::ArrayPop { .. }
            | Expr::ArraySet { .. }
            | Expr::ArrayRemove { .. }
    )
}

/// Whether `expr` may be lifted into a `Let` ahead of its siblings — a
/// fresh allocation whose only remaining work is the allocation itself.
/// See this module's doc comment for why both halves are required.
fn hoistable(expr: &Expr) -> bool {
    if !is_fresh_alloc(expr) {
        return false;
    }
    let mut all_atoms = true;
    for_each_child(expr, &mut |c| {
        if !is_atom(c) {
            all_atoms = false;
        }
    });
    all_atoms
}

/// The set of functions whose RESULT the caller owns — i.e. whose return
/// value is always a reference the caller may release.
///
/// This exists so a call-result intermediate can be released. It was the
/// last shape still leaking (48.1 MB per 1M iterations of
/// `match mk(i) { .. }`), and the reason is a real one: this backend's
/// callees do not release their parameters, so a function may RETURN one
/// and the caller then holds no extra reference. Releasing the result of
/// `let pass (p) = p` would free the caller's own value.
///
/// The safe, convention-preserving answer is to identify the functions
/// for which that cannot happen, rather than to change what a parameter
/// means. Nothing here adds an increment anywhere and no calling
/// convention moves; a function either provably hands back a new
/// reference or it does not, and "does not" is the default.
///
/// A least fixpoint from the empty set, so mutual recursion simply fails
/// to qualify rather than being assumed safe — an optimistic
/// greatest-fixpoint would be the usual formulation and would qualify
/// more functions, but being wrong here is a use-after-free, and the
/// common shape this targets (a constructor-style function whose body IS
/// a `Ctor`) converges on the first pass anyway.
pub fn owned_returning(program: &crate::ir::Program) -> std::collections::HashSet<String> {
    let mut owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let mut changed = false;
        for f in &program.functions {
            if !owned.contains(&f.name) && returns_owned(&f.body, &owned) {
                owned.insert(f.name.clone());
                changed = true;
            }
        }
        if !changed {
            return owned;
        }
    }
}

/// Whether `tail` — an expression in RETURN position — always evaluates
/// to a reference its caller owns.
///
/// Deliberately conservative, and the default is `false`. Two refusals
/// are worth naming because both look like they could be allowed:
///
/// - A bare `Var`, even a match-extracted binding that codegen DID
///   increment. Telling those apart from a parameter needs types the IR
///   does not carry, and mistaking a parameter for an owned value is a
///   use-after-free.
/// - `Index`, which loads an element with no increment — the array still
///   owns it.
///
/// A scalar return (`Int`, `Unit`, a comparison) is also `false`, which
/// matters: a scalar has nothing to release, and marking such a function
/// owned would invite a decrement on an `Int`.
fn returns_owned(tail: &Expr, owned: &std::collections::HashSet<String>) -> bool {
    if is_fresh_alloc(tail) {
        return true;
    }
    match tail {
        // Tail position threads through these unchanged.
        Expr::Let { body, .. } => returns_owned(body, owned),
        Expr::RcAnnotated { rest, .. } => returns_owned(rest, owned),
        Expr::If { then_branch, else_branch, .. } => {
            returns_owned(then_branch, owned) && returns_owned(else_branch, owned)
        }
        Expr::Match { arms, .. } => arms.iter().all(|a| returns_owned(&a.body, owned)),
        // By induction over the fixpoint: a call to an owned-returning
        // function hands back an owned reference, so returning it does
        // too.
        Expr::Call { callee, .. } => matches!(callee.as_ref(), Expr::Var(n) if owned.contains(n)),
        _ => false,
    }
}

/// A-normalises every function body and global initializer in `program`.
pub fn anf_program(program: crate::ir::Program) -> crate::ir::Program {
    let owned = owned_returning(&program);
    let mut counter = 0usize;
    crate::ir::Program {
        functions: program
            .functions
            .into_iter()
            .map(|f| crate::ir::Function {
                name: f.name,
                params: f.params,
                body: region(f.body, &mut counter, &owned),
            })
            .collect(),
        globals: program
            .globals
            .into_iter()
            .map(|g| crate::ir::Global {
                name: g.name,
                value: region(g.value, &mut counter, &owned),
            })
            .collect(),
        externs: program.externs,
    }
}

/// Whether `expr` is a call whose result the caller owns AND whose
/// arguments are all atoms — the two conditions that make it safe to
/// hoist. See `owned_returning` for the first and this module's doc
/// comment for the second (hoisting moves evaluation earlier, so a
/// non-atom argument would be reordered against siblings).
fn hoistable_owned_call(expr: &Expr, owned: &std::collections::HashSet<String>) -> bool {
    let Expr::Call { callee, args } = expr else {
        return false;
    };
    matches!(callee.as_ref(), Expr::Var(n) if owned.contains(n)) && args.iter().all(is_atom)
}

/// Flattens `expr` as a self-contained region: hoisted bindings are
/// wrapped around it rather than escaping to an enclosing expression.
///
/// The region's OWN result is never hoisted — only proper
/// subexpressions. Binding the result would hand it to `fbip` as a
/// releasable local, which for a value being returned is exactly wrong.
fn region(expr: Expr, counter: &mut usize, owned: &std::collections::HashSet<String>) -> Expr {
    let mut binds = Vec::new();
    let flat = flatten(expr, &mut binds, counter, owned);
    let mut out = flat;
    for (name, value, is_owned_call) in binds.into_iter().rev() {
        // A hoisted OWNED CALL gets its release emitted here rather than
        // by `fbip`. `fbip::allocates_fresh_heap` does not recognize a
        // `Call` (it cannot — that judgement needs the whole-program
        // `owned_returning` analysis), so it will not add a second one;
        // and `is_syntactically_heap` does not either, so the binding
        // never becomes a reuse target or picks up increments.
        //
        // The borrow test is `fbip`'s OWN predicate, not a copy of it.
        // Two passes deciding "is this use a borrow" separately is how
        // they come to disagree, and disagreeing means a double free.
        // Single-use is automatic: this pass created the one use itself.
        let body = if is_owned_call && crate::fbip::all_uses_are_borrows(&out, &name) {
            crate::fbip::drop_at_scope_end(out, &name)
        } else {
            out
        };
        out = Expr::Let {
            name,
            value: Box::new(value),
            body: Box::new(body),
        };
    }
    out
}

/// Hoists `child` into `binds` if it qualifies, returning what should
/// stand in its place.
fn hoist(
    child: Expr,
    binds: &mut Vec<(String, Expr, bool)>,
    counter: &mut usize,
    owned: &std::collections::HashSet<String>,
) -> Expr {
    let flat = flatten(child, binds, counter, owned);
    let is_owned_call = hoistable_owned_call(&flat, owned);
    if !hoistable(&flat) && !is_owned_call {
        return flat;
    }
    *counter += 1;
    // `$` cannot appear in a Plum identifier, so this can never collide
    // with a user's own name — the same guarantee `monomorphize`'s
    // mangling relies on.
    let name = format!("anf${counter}");
    binds.push((name.clone(), flat, is_owned_call));
    Expr::Var(name)
}

/// Walks `expr`, hoisting qualifying subexpressions from its
/// unconditionally-evaluated child slots into `binds` (in evaluation
/// order) and recursing into its deferred slots as separate regions.
///
/// Exhaustive over `Expr` with no `_` arm, deliberately: a new variant
/// must force a decision about whether its children are inline or
/// deferred, rather than silently defaulting to one.
fn flatten(
    expr: Expr,
    binds: &mut Vec<(String, Expr, bool)>,
    counter: &mut usize,
    owned: &std::collections::HashSet<String>,
) -> Expr {
    match expr {
        // Atoms and childless nodes.
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

        // --- deferred/conditional slots: separate regions ---
        //
        // Hoisting out of any of these would change WHETHER or HOW OFTEN
        // the expression is evaluated, not just when.
        Expr::If { cond, then_branch, else_branch } => Expr::If {
            cond: Box::new(hoist(*cond, binds, counter, owned)),
            then_branch: Box::new(region(*then_branch, counter, owned)),
            else_branch: Box::new(region(*else_branch, counter, owned)),
        },
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(hoist(*scrutinee, binds, counter, owned)),
            arms: arms
                .into_iter()
                .map(|arm| MatchArm {
                    tag: arm.tag,
                    bindings: arm.bindings,
                    guard: arm.guard.map(|g| Box::new(region(*g, counter, owned))),
                    body: region(arm.body, counter, owned),
                })
                .collect(),
        },
        Expr::For { var, start, end, body } => Expr::For {
            var,
            start: Box::new(hoist(*start, binds, counter, owned)),
            end: Box::new(hoist(*end, binds, counter, owned)),
            body: Box::new(region(*body, counter, owned)),
        },
        Expr::Closure { params, param_types, ret_type, body } => Expr::Closure {
            params,
            param_types,
            ret_type,
            body: Box::new(region(*body, counter, owned)),
        },
        Expr::Spawn { block } => Expr::Spawn { block: Box::new(region(*block, counter, owned)) },
        // Both slots of a `Select` arm are deferred — a receiver is polled
        // as part of the select's own scheduling, not evaluated inline
        // here.
        Expr::Select { arms } => Expr::Select {
            arms: arms
                .into_iter()
                .map(|arm| SelectArm {
                    receiver: region(arm.receiver, counter, owned),
                    body: region(arm.body, counter, owned),
                })
                .collect(),
        },
        // A `Let`'s value IS its binding, so it is never hoisted itself
        // (that is the whole point) — but subexpressions inside it are.
        // Its body is a region: hoisting from there would place the
        // binding before `value` evaluates.
        Expr::Let { name, value, body } => Expr::Let {
            name,
            value: Box::new(flatten(*value, binds, counter, owned)),
            body: Box::new(region(*body, counter, owned)),
        },
        Expr::Assign { name, value, rest } => Expr::Assign {
            name,
            value: Box::new(hoist(*value, binds, counter, owned)),
            rest: Box::new(region(*rest, counter, owned)),
        },
        Expr::RcAnnotated { op, target, rest } => Expr::RcAnnotated {
            op,
            target,
            rest: Box::new(region(*rest, counter, owned)),
        },

        // --- inline slots: hoisted in left-to-right evaluation order ---
        Expr::Unary(op, e) => Expr::Unary(op, Box::new(hoist(*e, binds, counter, owned))),
        Expr::AsCStr(e) => Expr::AsCStr(Box::new(hoist(*e, binds, counter, owned))),
        Expr::AsString(e) => Expr::AsString(Box::new(hoist(*e, binds, counter, owned))),
        Expr::ToIntTrunc(e) => Expr::ToIntTrunc(Box::new(hoist(*e, binds, counter, owned))),
        Expr::ToIntRound(e) => Expr::ToIntRound(Box::new(hoist(*e, binds, counter, owned))),
        Expr::ToFloat(e) => Expr::ToFloat(Box::new(hoist(*e, binds, counter, owned))),
        Expr::Binary(op, l, r) => {
            let l = hoist(*l, binds, counter, owned);
            let r = hoist(*r, binds, counter, owned);
            Expr::Binary(op, Box::new(l), Box::new(r))
        }
        Expr::Call { callee, args } => {
            let callee = hoist(*callee, binds, counter, owned);
            let args = args.into_iter().map(|a| hoist(a, binds, counter, owned)).collect();
            Expr::Call { callee: Box::new(callee), args }
        }
        Expr::ExternCall { name, args } => Expr::ExternCall {
            name,
            args: args.into_iter().map(|a| hoist(a, binds, counter, owned)).collect(),
        },
        Expr::Ctor { tag, fields } => Expr::Ctor {
            tag,
            fields: fields.into_iter().map(|f| hoist(f, binds, counter, owned)).collect(),
        },
        Expr::CtorReuse { reuse_of, tag, fields } => Expr::CtorReuse {
            reuse_of,
            tag,
            fields: fields.into_iter().map(|f| hoist(f, binds, counter, owned)).collect(),
        },
        Expr::TaskJoin { task } => Expr::TaskJoin { task: Box::new(hoist(*task, binds, counter, owned)) },
        Expr::ChannelSend { sender, value } => {
            let sender = hoist(*sender, binds, counter, owned);
            let value = hoist(*value, binds, counter, owned);
            Expr::ChannelSend { sender: Box::new(sender), value: Box::new(value) }
        }
        Expr::ChannelRecv { receiver } => Expr::ChannelRecv {
            receiver: Box::new(hoist(*receiver, binds, counter, owned)),
        },

        Expr::Index { base, index } => {
            let base = hoist(*base, binds, counter, owned);
            let index = hoist(*index, binds, counter, owned);
            Expr::Index { base: Box::new(base), index: Box::new(index) }
        }
        Expr::ArrayLen { array } => Expr::ArrayLen { array: Box::new(hoist(*array, binds, counter, owned)) },
        Expr::ArrayPop { array } => Expr::ArrayPop { array: Box::new(hoist(*array, binds, counter, owned)) },
        Expr::ArrayPush { array, value } => {
            let array = hoist(*array, binds, counter, owned);
            let value = hoist(*value, binds, counter, owned);
            Expr::ArrayPush { array: Box::new(array), value: Box::new(value) }
        }
        Expr::ArraySet { array, index, value } => {
            let array = hoist(*array, binds, counter, owned);
            let index = hoist(*index, binds, counter, owned);
            let value = hoist(*value, binds, counter, owned);
            Expr::ArraySet { array: Box::new(array), index: Box::new(index), value: Box::new(value) }
        }
        Expr::ArrayRemove { array, index } => {
            let array = hoist(*array, binds, counter, owned);
            let index = hoist(*index, binds, counter, owned);
            Expr::ArrayRemove { array: Box::new(array), index: Box::new(index) }
        }
        Expr::ArrayPushReuse { reuse_of, value } => Expr::ArrayPushReuse {
            reuse_of,
            value: Box::new(hoist(*value, binds, counter, owned)),
        },
        Expr::ArraySetReuse { reuse_of, index, value } => {
            let index = hoist(*index, binds, counter, owned);
            let value = hoist(*value, binds, counter, owned);
            Expr::ArraySetReuse { reuse_of, index: Box::new(index), value: Box::new(value) }
        }
        Expr::ArrayRemoveReuse { reuse_of, index } => Expr::ArrayRemoveReuse {
            reuse_of,
            index: Box::new(hoist(*index, binds, counter, owned)),
        },

        Expr::StrConcat { base, other } => {
            let base = hoist(*base, binds, counter, owned);
            let other = hoist(*other, binds, counter, owned);
            Expr::StrConcat { base: Box::new(base), other: Box::new(other) }
        }
        Expr::StrConcatReuse { reuse_of, other } => Expr::StrConcatReuse {
            reuse_of,
            other: Box::new(hoist(*other, binds, counter, owned)),
        },
        Expr::StrRunes { base } => Expr::StrRunes { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::StrTrim { base } => Expr::StrTrim { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::StrToUpper { base } => Expr::StrToUpper { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::StrToLower { base } => Expr::StrToLower { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::ToString { base } => Expr::ToString { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::StrHash { base } => Expr::StrHash { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::StrSplit { base, sep } => {
            let base = hoist(*base, binds, counter, owned);
            let sep = hoist(*sep, binds, counter, owned);
            Expr::StrSplit { base: Box::new(base), sep: Box::new(sep) }
        }
        Expr::StrContains { base, needle } => {
            let base = hoist(*base, binds, counter, owned);
            let needle = hoist(*needle, binds, counter, owned);
            Expr::StrContains { base: Box::new(base), needle: Box::new(needle) }
        }
        Expr::StrStartsWith { base, prefix } => {
            let base = hoist(*base, binds, counter, owned);
            let prefix = hoist(*prefix, binds, counter, owned);
            Expr::StrStartsWith { base: Box::new(base), prefix: Box::new(prefix) }
        }
        Expr::StrEndsWith { base, suffix } => {
            let base = hoist(*base, binds, counter, owned);
            let suffix = hoist(*suffix, binds, counter, owned);
            Expr::StrEndsWith { base: Box::new(base), suffix: Box::new(suffix) }
        }
        Expr::StrReplace { base, from, to } => {
            let base = hoist(*base, binds, counter, owned);
            let from = hoist(*from, binds, counter, owned);
            let to = hoist(*to, binds, counter, owned);
            Expr::StrReplace { base: Box::new(base), from: Box::new(from), to: Box::new(to) }
        }
        Expr::StrReplaceReuse { reuse_of, from, to } => {
            let from = hoist(*from, binds, counter, owned);
            let to = hoist(*to, binds, counter, owned);
            Expr::StrReplaceReuse { reuse_of, from: Box::new(from), to: Box::new(to) }
        }

        Expr::RefNew { value } => Expr::RefNew { value: Box::new(hoist(*value, binds, counter, owned)) },
        Expr::RefGet { base } => Expr::RefGet { base: Box::new(hoist(*base, binds, counter, owned)) },
        Expr::RefSet { base, value } => {
            let base = hoist(*base, binds, counter, owned);
            let value = hoist(*value, binds, counter, owned);
            Expr::RefSet { base: Box::new(base), value: Box::new(value) }
        }

        Expr::ReadFileRaw { path } => Expr::ReadFileRaw { path: Box::new(hoist(*path, binds, counter, owned)) },
        Expr::WriteFileRaw { path, contents } => {
            let path = hoist(*path, binds, counter, owned);
            let contents = hoist(*contents, binds, counter, owned);
            Expr::WriteFileRaw { path: Box::new(path), contents: Box::new(contents) }
        }
        Expr::EnvVarRaw { name } => Expr::EnvVarRaw { name: Box::new(hoist(*name, binds, counter, owned)) },
        Expr::PanicRaw { message } => Expr::PanicRaw { message: Box::new(hoist(*message, binds, counter, owned)) },
    }
}

/// Applies `f` to every direct child subexpression — used only by
/// `hoistable`'s atom check. Exhaustive with no `_` arm so a new variant
/// cannot silently be judged hoistable on the strength of children this
/// never looked at.
fn for_each_child<'a>(expr: &'a Expr, f: &mut dyn FnMut(&'a Expr)) {
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
        | Expr::StrToLowerReuse { .. } => {}

        Expr::Unary(_, e)
        | Expr::AsCStr(e)
        | Expr::AsString(e)
        | Expr::ToIntTrunc(e)
        | Expr::ToIntRound(e)
        | Expr::ToFloat(e) => f(e),
        Expr::Binary(_, l, r) => {
            f(l);
            f(r);
        }
        Expr::Let { value, body, .. } => {
            f(value);
            f(body);
        }
        Expr::If { cond, then_branch, else_branch } => {
            f(cond);
            f(then_branch);
            f(else_branch);
        }
        Expr::Call { callee, args } => {
            f(callee);
            args.iter().for_each(|a| f(a));
        }
        Expr::ExternCall { args, .. } => args.iter().for_each(|a| f(a)),
        Expr::Ctor { fields, .. } | Expr::CtorReuse { fields, .. } => fields.iter().for_each(|x| f(x)),
        Expr::Match { scrutinee, arms } => {
            f(scrutinee);
            for a in arms {
                if let Some(g) = &a.guard {
                    f(g);
                }
                f(&a.body);
            }
        }
        Expr::RcAnnotated { rest, .. } => f(rest),
        Expr::For { start, end, body, .. } => {
            f(start);
            f(end);
            f(body);
        }
        Expr::Closure { body, .. } => f(body),
        Expr::Assign { value, rest, .. } => {
            f(value);
            f(rest);
        }
        Expr::Spawn { block } => f(block),
        Expr::TaskJoin { task } => f(task),
        Expr::ChannelSend { sender, value } => {
            f(sender);
            f(value);
        }
        Expr::ChannelRecv { receiver } => f(receiver),
        Expr::Select { arms } => {
            for a in arms {
                f(&a.receiver);
                f(&a.body);
            }
        }

        Expr::Index { base, index } => {
            f(base);
            f(index);
        }
        Expr::ArrayLen { array } | Expr::ArrayPop { array } => f(array),
        Expr::ArrayPush { array, value } => {
            f(array);
            f(value);
        }
        Expr::ArraySet { array, index, value } => {
            f(array);
            f(index);
            f(value);
        }
        Expr::ArrayRemove { array, index } => {
            f(array);
            f(index);
        }
        Expr::ArrayPushReuse { value, .. } => f(value),
        Expr::ArraySetReuse { index, value, .. } => {
            f(index);
            f(value);
        }
        Expr::ArrayRemoveReuse { index, .. } => f(index),

        Expr::StrConcat { base, other } => {
            f(base);
            f(other);
        }
        Expr::StrConcatReuse { other, .. } => f(other),
        Expr::StrRunes { base }
        | Expr::StrTrim { base }
        | Expr::StrToUpper { base }
        | Expr::StrToLower { base }
        | Expr::ToString { base }
        | Expr::StrHash { base }
        | Expr::RefGet { base } => f(base),
        Expr::StrSplit { base, sep } => {
            f(base);
            f(sep);
        }
        Expr::StrContains { base, needle } => {
            f(base);
            f(needle);
        }
        Expr::StrStartsWith { base, prefix } => {
            f(base);
            f(prefix);
        }
        Expr::StrEndsWith { base, suffix } => {
            f(base);
            f(suffix);
        }
        Expr::StrReplace { base, from, to } => {
            f(base);
            f(from);
            f(to);
        }
        Expr::StrReplaceReuse { from, to, .. } => {
            f(from);
            f(to);
        }

        Expr::RefNew { value } => f(value),
        Expr::RefSet { base, value } => {
            f(base);
            f(value);
        }
        Expr::ReadFileRaw { path } => f(path),
        Expr::WriteFileRaw { path, contents } => {
            f(path);
            f(contents);
        }
        Expr::EnvVarRaw { name } => f(name),
        Expr::PanicRaw { message } => f(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BinOp, Program};

    fn var(n: &str) -> Expr {
        Expr::Var(n.to_string())
    }

    fn ctor(tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::Ctor { tag: tag.to_string(), fields }
    }

    fn len(e: Expr) -> Expr {
        Expr::ArrayLen { array: Box::new(e) }
    }

    /// Runs the pass over a single expression, the way a function body
    /// would be.
    fn anf(e: Expr) -> Expr {
        let p = Program {
            functions: vec![crate::ir::Function { name: "f".into(), params: vec![], body: e }],
            globals: vec![],
            externs: vec![],
        };
        anf_program(p).functions.into_iter().next().unwrap().body
    }

    /// The `Let` chain wrapping `expr`, as `(name, value)` pairs, plus
    /// whatever it wraps.
    fn peel(mut e: Expr) -> (Vec<(String, Expr)>, Expr) {
        let mut binds = Vec::new();
        while let Expr::Let { name, value, body } = e {
            if !name.starts_with("anf$") {
                return (binds, Expr::Let { name, value, body });
            }
            binds.push((name, *value));
            e = *body;
        }
        (binds, e)
    }

    // --- which functions hand back an owned reference ---

    fn prog(fns: Vec<(&str, Vec<&str>, Expr)>) -> Program {
        Program {
            functions: fns
                .into_iter()
                .map(|(n, ps, b)| crate::ir::Function {
                    name: n.to_string(),
                    params: ps.into_iter().map(|p| p.to_string()).collect(),
                    body: b,
                })
                .collect(),
            globals: vec![],
            externs: vec![],
        }
    }

    fn call(f: &str, args: Vec<Expr>) -> Expr {
        Expr::Call { callee: Box::new(var(f)), args }
    }

    #[test]
    fn a_function_returning_a_fresh_allocation_returns_owned() {
        let p = prog(vec![("mk", vec!["n"], ctor("Point", vec![var("n")]))]);
        assert!(owned_returning(&p).contains("mk"));
    }

    #[test]
    fn a_function_returning_its_own_parameter_does_not() {
        // THE case this whole analysis exists for. This backend's callees
        // do not release their parameters, so `pass` hands back the
        // CALLER's reference and the caller has nothing extra to release.
        // Releasing it would free the caller's own value.
        let p = prog(vec![("pass", vec!["p"], var("p"))]);
        assert!(!owned_returning(&p).contains("pass"));
    }

    #[test]
    fn returning_a_parameter_from_only_one_branch_still_disqualifies() {
        let p = prog(vec![(
            "pick",
            vec!["a", "b", "first"],
            Expr::If {
                cond: Box::new(var("first")),
                then_branch: Box::new(var("a")),
                else_branch: Box::new(ctor("Point", vec![])),
            },
        )]);
        assert!(!owned_returning(&p).contains("pick"));
    }

    #[test]
    fn ownership_propagates_through_calls_to_other_owned_functions() {
        // The fixpoint's only real job. `build`'s tail is a call to `mk`,
        // so it qualifies once `mk` does.
        let p = prog(vec![
            ("mk", vec!["n"], ctor("Point", vec![var("n")])),
            ("build", vec!["n"], call("mk", vec![var("n")])),
        ]);
        let owned = owned_returning(&p);
        assert!(owned.contains("mk") && owned.contains("build"), "got {owned:?}");
    }

    #[test]
    fn calling_a_non_owned_function_in_tail_position_disqualifies() {
        let p = prog(vec![
            ("pass", vec!["p"], var("p")),
            ("wrap", vec!["p"], call("pass", vec![var("p")])),
        ]);
        assert!(!owned_returning(&p).contains("wrap"));
    }

    #[test]
    fn a_scalar_returning_function_does_not_return_owned() {
        // Not an oversight: a scalar has nothing to release, and marking
        // it owned would invite a decrement on an `Int`.
        let p = prog(vec![("count", vec![], Expr::Int(3))]);
        assert!(!owned_returning(&p).contains("count"));
    }

    #[test]
    fn returning_a_bare_match_binding_conservatively_does_not_qualify() {
        // Codegen DOES increment a refcounted extracted field, so this
        // could in principle qualify — but telling such a binding apart
        // from a parameter needs types the IR does not carry, and guessing
        // wrong is a use-after-free. The cost is a missed release.
        let p = prog(vec![(
            "unwrap",
            vec!["n"],
            Expr::Match {
                scrutinee: Box::new(var("n")),
                arms: vec![crate::ir::MatchArm {
                    tag: "Named".into(),
                    bindings: vec!["s".into()],
                    guard: None,
                    body: var("s"),
                }],
            },
        )]);
        assert!(!owned_returning(&p).contains("unwrap"));
    }

    #[test]
    fn mutual_recursion_fails_to_qualify_rather_than_being_assumed_safe() {
        // A least fixpoint from the empty set. An optimistic
        // greatest-fixpoint would qualify these and is the usual
        // formulation, but being wrong here is a use-after-free.
        let p = prog(vec![
            ("ping", vec!["n"], call("pong", vec![var("n")])),
            ("pong", vec!["n"], call("ping", vec![var("n")])),
        ]);
        assert!(owned_returning(&p).is_empty());
    }

    #[test]
    fn an_owned_calls_result_is_hoisted_and_released_in_a_borrow_slot() {
        let p = prog(vec![
            ("mk", vec!["n"], ctor("Point", vec![var("n")])),
            ("go", vec![], len(call("mk", vec![Expr::Int(1)]))),
        ]);
        let out = anf_program(p);
        let go = out.functions.into_iter().find(|f| f.name == "go").unwrap();
        let (binds, _) = peel(go.body.clone());
        assert_eq!(binds.len(), 1, "the call result should be bound: {binds:?}");
        assert!(
            mentions_dec(&go.body, &binds[0].0),
            "and released, since `.len()` only borrows it: {:?}",
            go.body
        );
    }

    #[test]
    fn a_non_owned_calls_result_is_not_hoisted() {
        let p = prog(vec![
            ("pass", vec!["p"], var("p")),
            ("go", vec!["p"], len(call("pass", vec![var("p")]))),
        ]);
        let out = anf_program(p);
        let go = out.functions.into_iter().find(|f| f.name == "go").unwrap();
        let (binds, _) = peel(go.body);
        assert!(binds.is_empty(), "must not be hoisted: {binds:?}");
    }

    #[test]
    fn an_owned_calls_result_in_a_consuming_slot_is_hoisted_but_not_released() {
        // Hoisting is harmless; releasing would not be — the `Ctor` takes
        // ownership of the value.
        let p = prog(vec![
            ("mk", vec!["n"], ctor("Point", vec![var("n")])),
            ("go", vec![], ctor("Wrapper", vec![call("mk", vec![Expr::Int(1)])])),
        ]);
        let out = anf_program(p);
        let go = out.functions.into_iter().find(|f| f.name == "go").unwrap();
        let (binds, _) = peel(go.body.clone());
        assert_eq!(binds.len(), 1);
        assert!(
            !mentions_dec(&go.body, &binds[0].0),
            "a stored value must not be released: {:?}",
            go.body
        );
    }

    fn mentions_dec(expr: &Expr, name: &str) -> bool {
        if let Expr::RcAnnotated { op: crate::ir::RcOp::Dec, target, .. } = expr {
            if target == name {
                return true;
            }
        }
        let mut found = false;
        for_each_child(expr, &mut |c| {
            if !found && mentions_dec(c, name) {
                found = true;
            }
        });
        found
    }

    #[test]
    fn a_fresh_allocation_in_a_borrow_slot_is_hoisted_to_a_named_temporary() {
        // `Point { .. }.len()` — the intermediate had no name, so nothing
        // could release it. Now it does.
        let (binds, rest) = peel(anf(len(ctor("Point", vec![Expr::Int(1)]))));
        assert_eq!(binds.len(), 1, "expected one hoisted binding: {binds:?}");
        assert_eq!(binds[0].1, ctor("Point", vec![Expr::Int(1)]));
        assert_eq!(rest, len(var(&binds[0].0)));
    }

    #[test]
    fn nested_fresh_allocations_are_all_hoisted_innermost_first() {
        // `"a".concat("b").len()`: three allocations (two literals and the
        // concat). Children are processed first, so each literal becomes
        // an atom before the concat is considered — which is what lets the
        // concat qualify at all.
        let e = len(Expr::StrConcat {
            base: Box::new(Expr::Str("a".into())),
            other: Box::new(Expr::Str("b".into())),
        });
        let (binds, rest) = peel(anf(e));
        assert_eq!(binds.len(), 3, "expected both literals and the concat: {binds:?}");
        assert_eq!(binds[0].1, Expr::Str("a".into()));
        assert_eq!(binds[1].1, Expr::Str("b".into()));
        assert_eq!(
            binds[2].1,
            Expr::StrConcat { base: Box::new(var(&binds[0].0)), other: Box::new(var(&binds[1].0)) },
            "the concat must be rebound over the two temporaries, in evaluation order"
        );
        assert_eq!(rest, len(var(&binds[2].0)));
    }

    #[test]
    fn a_call_is_never_hoisted() {
        // This backend's callees do not release their parameters, so a
        // function may return one of them and the caller has no extra
        // reference. Treating a call result as owned would be a
        // use-after-free for `let id (p) = p`.
        let e = len(Expr::Call { callee: Box::new(var("mk")), args: vec![Expr::Int(1)] });
        let (binds, _) = peel(anf(e));
        assert!(binds.is_empty(), "a Call must not be hoisted: {binds:?}");
    }

    #[test]
    fn a_fresh_allocation_over_a_non_atom_is_not_hoisted() {
        // Hoisting moves evaluation EARLIER, past any sibling to its
        // left, so it is only sound for an expression whose sole effect is
        // allocating. This one would drag a call along with it.
        let e = len(ctor(
            "Point",
            vec![Expr::Call { callee: Box::new(var("g")), args: vec![] }],
        ));
        let (binds, _) = peel(anf(e));
        assert!(binds.is_empty(), "a Ctor over a Call must not be hoisted: {binds:?}");
    }

    #[test]
    fn a_regions_own_result_is_never_hoisted() {
        // Binding the result would hand it to `fbip` as a releasable
        // local, which for a value being returned is exactly wrong.
        let e = ctor("Point", vec![Expr::Int(1)]);
        assert_eq!(anf(e.clone()), e);
    }

    #[test]
    fn nothing_is_hoisted_out_of_a_match_arm() {
        // Hoisting from an arm would evaluate it whether or not the arm
        // was taken.
        let arm_body = len(ctor("Inner", vec![Expr::Int(1)]));
        let e = Expr::Match {
            scrutinee: Box::new(var("s")),
            arms: vec![crate::ir::MatchArm {
                tag: "Tag".into(),
                bindings: vec![],
                guard: None,
                body: arm_body,
            }],
        };
        let (binds, rest) = peel(anf(e));
        assert!(binds.is_empty(), "nothing may escape the arm: {binds:?}");
        // ...but the arm is still flattened internally, as its own region.
        let Expr::Match { arms, .. } = rest else {
            panic!("expected a Match");
        };
        let (inner_binds, _) = peel(arms[0].body.clone());
        assert_eq!(inner_binds.len(), 1, "the arm's own region should hoist: {inner_binds:?}");
    }

    #[test]
    fn nothing_is_hoisted_out_of_a_loop_body() {
        // Hoisting from a loop body would evaluate it once instead of per
        // iteration — and would defeat the entire point, since releasing
        // per iteration is what keeps memory flat.
        let e = Expr::For {
            var: "i".into(),
            start: Box::new(Expr::Int(0)),
            end: Box::new(Expr::Int(10)),
            body: Box::new(len(ctor("Point", vec![Expr::Int(1)]))),
        };
        let (binds, rest) = peel(anf(e));
        assert!(binds.is_empty(), "nothing may escape the loop: {binds:?}");
        let Expr::For { body, .. } = rest else {
            panic!("expected a For");
        };
        let (inner, _) = peel(*body);
        assert_eq!(inner.len(), 1, "the body's own region should hoist: {inner:?}");
    }

    #[test]
    fn siblings_keep_their_evaluation_order() {
        // Both operands qualify, so both are hoisted — and the bindings
        // must appear left-to-right, or the program's evaluation order
        // changes.
        let e = Expr::Binary(
            BinOp::Add,
            Box::new(len(ctor("A", vec![]))),
            Box::new(len(ctor("B", vec![]))),
        );
        let (binds, _) = peel(anf(e));
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].1, ctor("A", vec![]));
        assert_eq!(binds[1].1, ctor("B", vec![]));
    }

    #[test]
    fn an_expression_with_nothing_to_hoist_is_returned_unchanged() {
        let e = Expr::Binary(BinOp::Add, Box::new(var("a")), Box::new(Expr::Int(1)));
        assert_eq!(anf(e.clone()), e);
    }
}
