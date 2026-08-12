use crate::ir::{Expr, Function, Global, MatchArm, Program, RcOp, SelectArm};
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

/// Runs the full FBIP pipeline in order: refcount insertion, then
/// reuse-in-place analysis on top of its output. This is the function
/// a later pass (eventually codegen) would actually call.
pub fn optimize(expr: Expr) -> Expr {
    mark_reuse(insert_refcount_ops(expr))
}

/// Runs `optimize` over every function body in a program. This is the
/// entry point a real driver (plumc) should call between lowering and
/// loading a program into the interpreter — without it, functions run
/// with no refcounting or reuse-in-place at all, since `optimize` only
/// ever touched whatever single expression was handed to it directly.
pub fn optimize_program(program: Program) -> Program {
    Program {
        functions: program
            .functions
            .into_iter()
            .map(|f| Function {
                name: f.name,
                params: f.params,
                body: optimize(f.body),
            })
            .collect(),
        globals: program
            .globals
            .into_iter()
            .map(|g| Global {
                name: g.name,
                value: optimize(g.value),
            })
            .collect(),
        externs: program.externs,
    }
}

/// Reuse-in-place analysis — the second half of FBIP, run after
/// `insert_refcount_ops`. Marks `Ctor` constructions as `CtorReuse`
/// candidates when they appear directly as a match arm's body and have
/// the same field count as what that arm deconstructed (field COUNT is
/// this pass's stand-in for real layout/size compatibility, which
/// needs a type checker we don't have yet).
pub fn mark_reuse(expr: Expr) -> Expr {
    mark_reuse_scoped(expr, &HashSet::new())
}

/// The real body of `mark_reuse` — takes a `known_heap` set tracked in
/// EXACT lockstep with `transform`'s own (same `is_syntactically_heap`
/// calls, same `Let`-only growth, same non-extension for `Match` arm
/// bindings/`Closure` params/`For` loop vars) so that a reuse rewrite
/// only ever fires for a name `insert_refcount_ops` actually protected
/// with Inc/Dec.
///
/// This guard is load-bearing, not defensive: a function PARAMETER is
/// never added to `known_heap` (see `transform`'s own `Closure`/params
/// doc comment — no type checker in this IR to prove a param is
/// heap-shaped), so `insert_refcount_ops` never Incs it even when it's
/// genuinely aliased (used twice in one body). Before this guard,
/// `mark_reuse` rewrote ANY bare-`Var` base into a `*Reuse` node
/// regardless — so two simultaneous uses of the same unprotected
/// parameter (e.g. `s.concat(rep(s, n - 1))`, where `rep` recurses on
/// `s` again) could each see the runtime refcount as 1 and both
/// destructively reuse the SAME heap cell, silently corrupting
/// whichever wrote first. Confirmed real via a minimal repro agreeing
/// across both backends — see DESIGN.md's "Open questions" entry this
/// fix closes. Restricting reuse to names actually present in
/// `known_heap` costs some optimization opportunity (a parameter used
/// only once could, in principle, still be safely reused, but proving
/// that needs real type info this IR doesn't have) in exchange for
/// always being correct.
fn mark_reuse_scoped(expr: Expr, known_heap: &HashSet<String>) -> Expr {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::Var(_) | Expr::EmptyArray(_) => expr,
        Expr::Unary(op, e) => Expr::Unary(op, Box::new(mark_reuse_scoped(*e, known_heap))),
        Expr::AsCStr(e) => Expr::AsCStr(Box::new(mark_reuse_scoped(*e, known_heap))),
        Expr::AsString(e) => Expr::AsString(Box::new(mark_reuse_scoped(*e, known_heap))),
        Expr::ToIntTrunc(e) => Expr::ToIntTrunc(Box::new(mark_reuse_scoped(*e, known_heap))),
        Expr::ToIntRound(e) => Expr::ToIntRound(Box::new(mark_reuse_scoped(*e, known_heap))),
        Expr::ToFloat(e) => Expr::ToFloat(Box::new(mark_reuse_scoped(*e, known_heap))),
        Expr::Binary(op, l, r) => Expr::Binary(
            op,
            Box::new(mark_reuse_scoped(*l, known_heap)),
            Box::new(mark_reuse_scoped(*r, known_heap)),
        ),
        Expr::Let { name, value, body } => {
            // Mirrors `transform`'s own `Let` arm exactly: the decision
            // uses the PRE-rewrite value shape (safe to recompute here
            // since neither pass restructures a Let's own top-level
            // Ctor/Str-literal/EmptyArray-literal/Var(already-heap)
            // value node, only nested uses within it).
            let is_heap_value = is_syntactically_heap(&value, known_heap);
            let value_t = mark_reuse_scoped(*value, known_heap);
            let mut inner_heap = known_heap.clone();
            if is_heap_value {
                inner_heap.insert(name.clone());
            }
            let body_t = mark_reuse_scoped(*body, &inner_heap);
            Expr::Let {
                name,
                value: Box::new(value_t),
                body: Box::new(body_t),
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => Expr::If {
            cond: Box::new(mark_reuse_scoped(*cond, known_heap)),
            then_branch: Box::new(mark_reuse_scoped(*then_branch, known_heap)),
            else_branch: Box::new(mark_reuse_scoped(*else_branch, known_heap)),
        },
        Expr::Call { callee, args } => Expr::Call {
            callee: Box::new(mark_reuse_scoped(*callee, known_heap)),
            args: args.into_iter().map(|a| mark_reuse_scoped(a, known_heap)).collect(),
        },
        Expr::ExternCall { name, args } => Expr::ExternCall {
            name,
            args: args.into_iter().map(|a| mark_reuse_scoped(a, known_heap)).collect(),
        },
        Expr::Ctor { tag, fields } => Expr::Ctor {
            tag,
            fields: fields.into_iter().map(|f| mark_reuse_scoped(f, known_heap)).collect(),
        },
        Expr::CtorReuse {
            reuse_of,
            tag,
            fields,
        } => Expr::CtorReuse {
            reuse_of,
            tag,
            fields: fields.into_iter().map(|f| mark_reuse_scoped(f, known_heap)).collect(),
        },
        Expr::RcAnnotated { op, target, rest } => Expr::RcAnnotated {
            op,
            target,
            rest: Box::new(mark_reuse_scoped(*rest, known_heap)),
        },
        Expr::For { var, start, end, body } => Expr::For {
            var,
            start: Box::new(mark_reuse_scoped(*start, known_heap)),
            end: Box::new(mark_reuse_scoped(*end, known_heap)),
            body: Box::new(mark_reuse_scoped(*body, known_heap)),
        },
        // Params aren't added — same as `transform`'s `Closure` arm,
        // and for the same reason: a closure param is never provably
        // heap-shaped here either. The outer `known_heap` still flows
        // through unchanged (matching `transform`), so captured names
        // stay correctly tracked.
        Expr::Closure { params, param_types, ret_type, body } => Expr::Closure {
            params,
            param_types,
            ret_type,
            body: Box::new(mark_reuse_scoped(*body, known_heap)),
        },
        Expr::Assign { name, value, rest } => Expr::Assign {
            name,
            value: Box::new(mark_reuse_scoped(*value, known_heap)),
            rest: Box::new(mark_reuse_scoped(*rest, known_heap)),
        },
        Expr::Spawn { block } => Expr::Spawn {
            block: Box::new(mark_reuse_scoped(*block, known_heap)),
        },
        Expr::TaskJoin { task } => Expr::TaskJoin {
            task: Box::new(mark_reuse_scoped(*task, known_heap)),
        },
        Expr::Channel => Expr::Channel,
        Expr::ChannelSend { sender, value } => Expr::ChannelSend {
            sender: Box::new(mark_reuse_scoped(*sender, known_heap)),
            value: Box::new(mark_reuse_scoped(*value, known_heap)),
        },
        Expr::ChannelRecv { receiver } => Expr::ChannelRecv {
            receiver: Box::new(mark_reuse_scoped(*receiver, known_heap)),
        },
        Expr::RefNew { value } => Expr::RefNew {
            value: Box::new(mark_reuse_scoped(*value, known_heap)),
        },
        Expr::RefGet { base } => Expr::RefGet {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
        },
        Expr::RefSet { base, value } => Expr::RefSet {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
            value: Box::new(mark_reuse_scoped(*value, known_heap)),
        },
        Expr::ReadFileRaw { path } => Expr::ReadFileRaw {
            path: Box::new(mark_reuse_scoped(*path, known_heap)),
        },
        Expr::WriteFileRaw { path, contents } => Expr::WriteFileRaw {
            path: Box::new(mark_reuse_scoped(*path, known_heap)),
            contents: Box::new(mark_reuse_scoped(*contents, known_heap)),
        },
        Expr::EnvVarRaw { name } => Expr::EnvVarRaw {
            name: Box::new(mark_reuse_scoped(*name, known_heap)),
        },
        // `ArgsRaw` carries no sub-expression at all (see its own doc
        // comment in `ir.rs`) — nothing to recurse into.
        Expr::ArgsRaw => Expr::ArgsRaw,
        Expr::RandomRaw => Expr::RandomRaw,
        Expr::PanicRaw { message } => Expr::PanicRaw {
            message: Box::new(mark_reuse_scoped(*message, known_heap)),
        },
        Expr::Select { arms } => Expr::Select {
            arms: arms
                .into_iter()
                .map(|arm| SelectArm {
                    receiver: mark_reuse_scoped(arm.receiver, known_heap),
                    body: mark_reuse_scoped(arm.body, known_heap),
                })
                .collect(),
        },
        Expr::Index { base, index } => Expr::Index {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
            index: Box::new(mark_reuse_scoped(*index, known_heap)),
        },
        Expr::ArrayLen { array } => Expr::ArrayLen {
            array: Box::new(mark_reuse_scoped(*array, known_heap)),
        },
        // A plain-variable `array` is a reuse-in-place CANDIDATE ONLY
        // when `insert_refcount_ops` actually tracked it (`name` is in
        // `known_heap`) — same "only a bare variable names a specific
        // cell we could target" precedent as `Match`'s scrutinee below,
        // now paired with the refcount-tracking guard the doc comment
        // above explains. Without a `known_heap` entry, the runtime
        // refcount check in `Interpreter::eval`'s `*Reuse` handling has
        // nothing meaningful to check against — a second, un-Inc'd
        // alias would read as refcount 1 even though it isn't unique.
        Expr::ArrayPush { array, value } => match array.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::ArrayPushReuse {
                reuse_of: name.clone(),
                value: Box::new(mark_reuse_scoped(*value, known_heap)),
            },
            _ => Expr::ArrayPush {
                array: Box::new(mark_reuse_scoped(*array, known_heap)),
                value: Box::new(mark_reuse_scoped(*value, known_heap)),
            },
        },
        Expr::ArrayPop { array } => match array.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::ArrayPopReuse { reuse_of: name.clone() },
            _ => Expr::ArrayPop {
                array: Box::new(mark_reuse_scoped(*array, known_heap)),
            },
        },
        Expr::ArraySet { array, index, value } => match array.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::ArraySetReuse {
                reuse_of: name.clone(),
                index: Box::new(mark_reuse_scoped(*index, known_heap)),
                value: Box::new(mark_reuse_scoped(*value, known_heap)),
            },
            _ => Expr::ArraySet {
                array: Box::new(mark_reuse_scoped(*array, known_heap)),
                index: Box::new(mark_reuse_scoped(*index, known_heap)),
                value: Box::new(mark_reuse_scoped(*value, known_heap)),
            },
        },
        Expr::ArrayRemove { array, index } => match array.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::ArrayRemoveReuse {
                reuse_of: name.clone(),
                index: Box::new(mark_reuse_scoped(*index, known_heap)),
            },
            _ => Expr::ArrayRemove {
                array: Box::new(mark_reuse_scoped(*array, known_heap)),
                index: Box::new(mark_reuse_scoped(*index, known_heap)),
            },
        },
        // Shouldn't normally appear as INPUT here — these are produced
        // BY this pass, same "handled for robustness in case pass
        // ordering ever changes" precedent as `CtorReuse` below.
        Expr::ArrayPushReuse { reuse_of, value } => Expr::ArrayPushReuse {
            reuse_of,
            value: Box::new(mark_reuse_scoped(*value, known_heap)),
        },
        Expr::ArrayPopReuse { reuse_of } => Expr::ArrayPopReuse { reuse_of },
        Expr::ArraySetReuse { reuse_of, index, value } => Expr::ArraySetReuse {
            reuse_of,
            index: Box::new(mark_reuse_scoped(*index, known_heap)),
            value: Box::new(mark_reuse_scoped(*value, known_heap)),
        },
        Expr::ArrayRemoveReuse { reuse_of, index } => Expr::ArrayRemoveReuse {
            reuse_of,
            index: Box::new(mark_reuse_scoped(*index, known_heap)),
        },
        // Same known_heap-gated reuse candidacy check as the array ops
        // above — see this function's doc comment for why the guard is
        // required, not just defensive.
        Expr::StrConcat { base, other } => match base.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::StrConcatReuse {
                reuse_of: name.clone(),
                other: Box::new(mark_reuse_scoped(*other, known_heap)),
            },
            _ => Expr::StrConcat {
                base: Box::new(mark_reuse_scoped(*base, known_heap)),
                other: Box::new(mark_reuse_scoped(*other, known_heap)),
            },
        },
        Expr::StrConcatReuse { reuse_of, other } => Expr::StrConcatReuse {
            reuse_of,
            other: Box::new(mark_reuse_scoped(*other, known_heap)),
        },
        Expr::StrRunes { base } => Expr::StrRunes {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
        },
        Expr::StrTrim { base } => match base.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::StrTrimReuse { reuse_of: name.clone() },
            _ => Expr::StrTrim {
                base: Box::new(mark_reuse_scoped(*base, known_heap)),
            },
        },
        Expr::StrTrimReuse { reuse_of } => Expr::StrTrimReuse { reuse_of },
        Expr::StrSplit { base, sep } => Expr::StrSplit {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
            sep: Box::new(mark_reuse_scoped(*sep, known_heap)),
        },
        Expr::StrToUpper { base } => match base.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::StrToUpperReuse { reuse_of: name.clone() },
            _ => Expr::StrToUpper {
                base: Box::new(mark_reuse_scoped(*base, known_heap)),
            },
        },
        Expr::StrToUpperReuse { reuse_of } => Expr::StrToUpperReuse { reuse_of },
        Expr::StrToLower { base } => match base.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::StrToLowerReuse { reuse_of: name.clone() },
            _ => Expr::StrToLower {
                base: Box::new(mark_reuse_scoped(*base, known_heap)),
            },
        },
        Expr::StrToLowerReuse { reuse_of } => Expr::StrToLowerReuse { reuse_of },
        Expr::StrContains { base, needle } => Expr::StrContains {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
            needle: Box::new(mark_reuse_scoped(*needle, known_heap)),
        },
        Expr::StrStartsWith { base, prefix } => Expr::StrStartsWith {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
            prefix: Box::new(mark_reuse_scoped(*prefix, known_heap)),
        },
        Expr::StrEndsWith { base, suffix } => Expr::StrEndsWith {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
            suffix: Box::new(mark_reuse_scoped(*suffix, known_heap)),
        },
        Expr::StrReplace { base, from, to } => match base.as_ref() {
            Expr::Var(name) if known_heap.contains(name) => Expr::StrReplaceReuse {
                reuse_of: name.clone(),
                from: Box::new(mark_reuse_scoped(*from, known_heap)),
                to: Box::new(mark_reuse_scoped(*to, known_heap)),
            },
            _ => Expr::StrReplace {
                base: Box::new(mark_reuse_scoped(*base, known_heap)),
                from: Box::new(mark_reuse_scoped(*from, known_heap)),
                to: Box::new(mark_reuse_scoped(*to, known_heap)),
            },
        },
        Expr::StrReplaceReuse { reuse_of, from, to } => Expr::StrReplaceReuse {
            reuse_of,
            from: Box::new(mark_reuse_scoped(*from, known_heap)),
            to: Box::new(mark_reuse_scoped(*to, known_heap)),
        },
        Expr::ToString { base } => Expr::ToString {
            base: Box::new(mark_reuse_scoped(*base, known_heap)),
        },
        Expr::Match { scrutinee, arms } => {
            // Only a plain variable ALREADY TRACKED in known_heap names
            // a specific cell we could safely reuse — a call result, an
            // untracked parameter, or anything else isn't something we
            // can point reuse at without risking the same aliasing bug
            // this function's doc comment describes.
            let reuse_target = match scrutinee.as_ref() {
                Expr::Var(name) if known_heap.contains(name) => Some(name.clone()),
                _ => None,
            };
            let new_arms = arms
                .into_iter()
                .map(|arm| {
                    let arity = arm.bindings.len();
                    // Arm bindings aren't added to `known_heap` here
                    // either — same conservative limitation `transform`
                    // documents for its own `Match` arm.
                    let body = mark_reuse_scoped(arm.body, known_heap);
                    let body = match (&reuse_target, &body) {
                        // 0-field constructions (e.g. `Nil`) have
                        // nothing worth reusing.
                        (Some(reuse_of), Expr::Ctor { tag, fields })
                            if arity > 0 && fields.len() == arity =>
                        {
                            Expr::CtorReuse {
                                reuse_of: reuse_of.clone(),
                                tag: tag.clone(),
                                fields: fields.clone(),
                            }
                        }
                        _ => body,
                    };
                    MatchArm {
                        tag: arm.tag,
                        bindings: arm.bindings,
                        guard: arm.guard.map(|g| Box::new(mark_reuse_scoped(*g, known_heap))),
                        body,
                    }
                })
                .collect();
            Expr::Match {
                scrutinee: Box::new(mark_reuse_scoped(*scrutinee, known_heap)),
                arms: new_arms,
            }
        }
    }
}

/// Walks the whole tree, and at every `Let`, decides whether the bound
/// name is heap-shaped and — if so — runs `mark_last_uses` over its
/// body to insert the actual Inc/Dec ops for that one name.
fn transform(expr: Expr, known_heap: &HashSet<String>) -> Expr {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::Var(_) | Expr::EmptyArray(_) => expr,
        Expr::Unary(op, e) => Expr::Unary(op, Box::new(transform(*e, known_heap))),
        // `.as_cstr()` is architecturally UNIQUE among heap-consuming
        // operations (see `ir::Expr::AsCStr`'s own doc comment and
        // `codegen_as_cstr`'s): it's the ONLY one whose codegen
        // unconditionally decrements its operand's refcount, with NO
        // separate `RcAnnotated::Dec` ever inserted for it elsewhere —
        // every other consuming operation (`.concat()`'s `StrConcat`,
        // `.push()`'s `ArrayPush`, ...) instead leaves refcount
        // management ENTIRELY to whatever `RcAnnotated` this pass
        // separately inserts, which only ever fires for a TRACKED
        // (`known_heap`) name, so an untracked operand there just never
        // gets decremented at all (a leak, not a use-after-free).
        // `.as_cstr()` breaks that invariant by design, which means the
        // ordinary "untracked name -> no Inc/Dec inserted, nothing to
        // get wrong" reasoning this whole pass otherwise relies on does
        // NOT hold for it — a real, confirmed use-after-free (`.as_cstr
        // ()` called twice on a plain function PARAMETER, or on a top-
        // level GLOBAL, both — neither is ever added to `known_heap` by
        // design, see this function's own scope note above) that
        // manifested as garbage bytes on an actual live socket before
        // being root-caused here, not a theoretical gap.
        //
        // The fix: when `.as_cstr()`'s operand is a `Var` that ISN'T
        // tracked, wrap it in a protective `Inc` right here — `.as_cstr
        // ()`'s own guaranteed `Dec` then just cancels that BACK out
        // (net zero effect on the real refcount), turning what would
        // otherwise be an unconditional consume into a safe, ordinary
        // borrow-and-copy. A TRACKED name needs no such protection —
        // this pass's EXISTING `mark_last_uses` machinery already
        // proves whether it's genuinely safe to consume (and inserts
        // its own `Inc` first if it isn't), which is exactly why this
        // arm only ever fires for the UNTRACKED case, never doubling up
        // with that existing mechanism.
        Expr::AsCStr(e) => match e.as_ref() {
            Expr::Var(name) if !known_heap.contains(name) => Expr::RcAnnotated {
                op: RcOp::Inc,
                target: name.clone(),
                rest: Box::new(Expr::AsCStr(e)),
            },
            _ => Expr::AsCStr(Box::new(transform(*e, known_heap))),
        },
        Expr::AsString(e) => Expr::AsString(Box::new(transform(*e, known_heap))),
        Expr::ToIntTrunc(e) => Expr::ToIntTrunc(Box::new(transform(*e, known_heap))),
        Expr::ToIntRound(e) => Expr::ToIntRound(Box::new(transform(*e, known_heap))),
        Expr::ToFloat(e) => Expr::ToFloat(Box::new(transform(*e, known_heap))),
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
        Expr::ExternCall { name, args } => Expr::ExternCall {
            name,
            args: args.into_iter().map(|a| transform(a, known_heap)).collect(),
        },
        Expr::Ctor { tag, fields } => Expr::Ctor {
            tag,
            fields: fields.into_iter().map(|f| transform(f, known_heap)).collect(),
        },
        // Shouldn't normally appear as input here — CtorReuse is
        // produced BY the reuse pass, which runs after this one — but
        // handled for robustness in case pass ordering ever changes.
        Expr::CtorReuse { reuse_of, tag, fields } => Expr::CtorReuse {
            reuse_of,
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
                    guard: arm.guard.map(|g| Box::new(transform(*g, known_heap))),
                    body: transform(arm.body, known_heap),
                })
                .collect(),
        },
        Expr::RcAnnotated { op, target, rest } => Expr::RcAnnotated {
            op,
            target,
            rest: Box::new(transform(*rest, known_heap)),
        },
        // The loop variable is always an Int (the only iterable
        // supported is a Range — see ir.rs's `For` doc comment), so it
        // never needs heap tracking. `body` is transformed with the
        // SAME known_heap as the surrounding context, not extended —
        // it doesn't introduce anything new the way a `Let` does.
        Expr::For { var, start, end, body } => Expr::For {
            var,
            start: Box::new(transform(*start, known_heap)),
            end: Box::new(transform(*end, known_heap)),
            body: Box::new(transform(*body, known_heap)),
        },
        // Params aren't added to known_heap, same as a function's own
        // params never are — this pass starts fresh (`HashSet::new()`)
        // for every function body, so a param is never provably
        // heap-shaped without a type checker either way.
        Expr::Closure { params, param_types, ret_type, body } => Expr::Closure {
            params,
            param_types,
            ret_type,
            body: Box::new(transform(*body, known_heap)),
        },
        // See ir.rs's `Assign` doc comment: reassigning a heap-tracked
        // name doesn't Dec the value it's overwriting — the old cell
        // is simply orphaned (a leak, not a soundness gap), since this
        // pass doesn't track per-binding "generations." `value`/`rest`
        // still need ordinary recursion so nested constructs elsewhere
        // in them get transformed.
        Expr::Assign { name, value, rest } => Expr::Assign {
            name,
            value: Box::new(transform(*value, known_heap)),
            rest: Box::new(transform(*rest, known_heap)),
        },
        // `block` runs on another thread entirely — no heap tracking
        // carries across (see ir.rs's `Spawn` doc comment), but nested
        // constructs WITHIN `block` still need ordinary transformation,
        // same as a `Closure` body.
        Expr::Spawn { block } => Expr::Spawn {
            block: Box::new(transform(*block, known_heap)),
        },
        Expr::TaskJoin { task } => Expr::TaskJoin {
            task: Box::new(transform(*task, known_heap)),
        },
        Expr::Channel => Expr::Channel,
        Expr::ChannelSend { sender, value } => Expr::ChannelSend {
            sender: Box::new(transform(*sender, known_heap)),
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::ChannelRecv { receiver } => Expr::ChannelRecv {
            receiver: Box::new(transform(*receiver, known_heap)),
        },
        Expr::RefNew { value } => Expr::RefNew {
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::RefGet { base } => Expr::RefGet {
            base: Box::new(transform(*base, known_heap)),
        },
        Expr::RefSet { base, value } => Expr::RefSet {
            base: Box::new(transform(*base, known_heap)),
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::ReadFileRaw { path } => Expr::ReadFileRaw {
            path: Box::new(transform(*path, known_heap)),
        },
        Expr::WriteFileRaw { path, contents } => Expr::WriteFileRaw {
            path: Box::new(transform(*path, known_heap)),
            contents: Box::new(transform(*contents, known_heap)),
        },
        Expr::EnvVarRaw { name } => Expr::EnvVarRaw {
            name: Box::new(transform(*name, known_heap)),
        },
        Expr::ArgsRaw => Expr::ArgsRaw,
        Expr::RandomRaw => Expr::RandomRaw,
        Expr::PanicRaw { message } => Expr::PanicRaw {
            message: Box::new(transform(*message, known_heap)),
        },
        // Arm bindings (whatever a `Let`/`Match` node WITHIN `body`
        // introduces for the received value — see ir.rs's `Select` doc
        // comment) aren't added to `known_heap` here either, same
        // reasoning as `Match`'s own arm bodies just below.
        Expr::Select { arms } => Expr::Select {
            arms: arms
                .into_iter()
                .map(|arm| SelectArm {
                    receiver: transform(arm.receiver, known_heap),
                    body: transform(arm.body, known_heap),
                })
                .collect(),
        },
        Expr::Index { base, index } => Expr::Index {
            base: Box::new(transform(*base, known_heap)),
            index: Box::new(transform(*index, known_heap)),
        },
        Expr::ArrayLen { array } => Expr::ArrayLen {
            array: Box::new(transform(*array, known_heap)),
        },
        Expr::ArrayPush { array, value } => Expr::ArrayPush {
            array: Box::new(transform(*array, known_heap)),
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::ArrayPop { array } => Expr::ArrayPop {
            array: Box::new(transform(*array, known_heap)),
        },
        Expr::ArraySet { array, index, value } => Expr::ArraySet {
            array: Box::new(transform(*array, known_heap)),
            index: Box::new(transform(*index, known_heap)),
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::ArrayRemove { array, index } => Expr::ArrayRemove {
            array: Box::new(transform(*array, known_heap)),
            index: Box::new(transform(*index, known_heap)),
        },
        // Shouldn't normally appear as input here — these are produced
        // BY `mark_reuse`, which runs AFTER this pass — but handled for
        // robustness in case pass ordering ever changes, same
        // precedent as `CtorReuse` above.
        Expr::ArrayPushReuse { reuse_of, value } => Expr::ArrayPushReuse {
            reuse_of,
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::ArrayPopReuse { reuse_of } => Expr::ArrayPopReuse { reuse_of },
        Expr::ArraySetReuse { reuse_of, index, value } => Expr::ArraySetReuse {
            reuse_of,
            index: Box::new(transform(*index, known_heap)),
            value: Box::new(transform(*value, known_heap)),
        },
        Expr::ArrayRemoveReuse { reuse_of, index } => Expr::ArrayRemoveReuse {
            reuse_of,
            index: Box::new(transform(*index, known_heap)),
        },
        Expr::StrConcat { base, other } => Expr::StrConcat {
            base: Box::new(transform(*base, known_heap)),
            other: Box::new(transform(*other, known_heap)),
        },
        Expr::StrConcatReuse { reuse_of, other } => Expr::StrConcatReuse {
            reuse_of,
            other: Box::new(transform(*other, known_heap)),
        },
        Expr::StrRunes { base } => Expr::StrRunes {
            base: Box::new(transform(*base, known_heap)),
        },
        Expr::StrTrim { base } => Expr::StrTrim {
            base: Box::new(transform(*base, known_heap)),
        },
        Expr::StrTrimReuse { reuse_of } => Expr::StrTrimReuse { reuse_of },
        Expr::StrSplit { base, sep } => Expr::StrSplit {
            base: Box::new(transform(*base, known_heap)),
            sep: Box::new(transform(*sep, known_heap)),
        },
        Expr::StrToUpper { base } => Expr::StrToUpper {
            base: Box::new(transform(*base, known_heap)),
        },
        Expr::StrToUpperReuse { reuse_of } => Expr::StrToUpperReuse { reuse_of },
        Expr::StrToLower { base } => Expr::StrToLower {
            base: Box::new(transform(*base, known_heap)),
        },
        Expr::StrToLowerReuse { reuse_of } => Expr::StrToLowerReuse { reuse_of },
        Expr::StrContains { base, needle } => Expr::StrContains {
            base: Box::new(transform(*base, known_heap)),
            needle: Box::new(transform(*needle, known_heap)),
        },
        Expr::StrStartsWith { base, prefix } => Expr::StrStartsWith {
            base: Box::new(transform(*base, known_heap)),
            prefix: Box::new(transform(*prefix, known_heap)),
        },
        Expr::StrEndsWith { base, suffix } => Expr::StrEndsWith {
            base: Box::new(transform(*base, known_heap)),
            suffix: Box::new(transform(*suffix, known_heap)),
        },
        Expr::StrReplace { base, from, to } => Expr::StrReplace {
            base: Box::new(transform(*base, known_heap)),
            from: Box::new(transform(*from, known_heap)),
            to: Box::new(transform(*to, known_heap)),
        },
        Expr::StrReplaceReuse { reuse_of, from, to } => Expr::StrReplaceReuse {
            reuse_of,
            from: Box::new(transform(*from, known_heap)),
            to: Box::new(transform(*to, known_heap)),
        },
        Expr::ToString { base } => Expr::ToString {
            base: Box::new(transform(*base, known_heap)),
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

/// A coarse (shadowing-unaware, over-approximating is fine — see the
/// `For` arm of `mark_last_uses`) check for whether `name` appears
/// anywhere inside `expr` at all.
fn expr_mentions_var(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Var(n) => n == name,
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) => false,
        Expr::Unary(_, e) => expr_mentions_var(e, name),
        Expr::AsCStr(e) => expr_mentions_var(e, name),
        Expr::AsString(e) => expr_mentions_var(e, name),
        Expr::ToIntTrunc(e) => expr_mentions_var(e, name),
        Expr::ToIntRound(e) => expr_mentions_var(e, name),
        Expr::ToFloat(e) => expr_mentions_var(e, name),
        Expr::Binary(_, l, r) => expr_mentions_var(l, name) || expr_mentions_var(r, name),
        Expr::Let { value, body, .. } => expr_mentions_var(value, name) || expr_mentions_var(body, name),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => expr_mentions_var(cond, name) || expr_mentions_var(then_branch, name) || expr_mentions_var(else_branch, name),
        Expr::Call { callee, args } => {
            expr_mentions_var(callee, name) || args.iter().any(|a| expr_mentions_var(a, name))
        }
        Expr::ExternCall { args, .. } => args.iter().any(|a| expr_mentions_var(a, name)),
        Expr::Ctor { fields, .. } => fields.iter().any(|f| expr_mentions_var(f, name)),
        Expr::CtorReuse { fields, .. } => fields.iter().any(|f| expr_mentions_var(f, name)),
        Expr::RcAnnotated { rest, .. } => expr_mentions_var(rest, name),
        Expr::Match { scrutinee, arms } => {
            expr_mentions_var(scrutinee, name)
                || arms.iter().any(|a| {
                    expr_mentions_var(&a.body, name)
                        || a.guard.as_deref().is_some_and(|g| expr_mentions_var(g, name))
                })
        }
        Expr::For { start, end, body, .. } => {
            expr_mentions_var(start, name) || expr_mentions_var(end, name) || expr_mentions_var(body, name)
        }
        Expr::Closure { body, .. } => expr_mentions_var(body, name),
        Expr::Assign { value, rest, .. } => expr_mentions_var(value, name) || expr_mentions_var(rest, name),
        Expr::Spawn { block } => expr_mentions_var(block, name),
        Expr::TaskJoin { task } => expr_mentions_var(task, name),
        Expr::Channel => false,
        Expr::ChannelSend { sender, value } => {
            expr_mentions_var(sender, name) || expr_mentions_var(value, name)
        }
        Expr::ChannelRecv { receiver } => expr_mentions_var(receiver, name),
        Expr::RefNew { value } => expr_mentions_var(value, name),
        Expr::RefGet { base } => expr_mentions_var(base, name),
        Expr::RefSet { base, value } => expr_mentions_var(base, name) || expr_mentions_var(value, name),
        Expr::ReadFileRaw { path } => expr_mentions_var(path, name),
        Expr::WriteFileRaw { path, contents } => expr_mentions_var(path, name) || expr_mentions_var(contents, name),
        Expr::EnvVarRaw { name: n } => expr_mentions_var(n, name),
        Expr::ArgsRaw => false,
        Expr::RandomRaw => false,
        Expr::PanicRaw { message } => expr_mentions_var(message, name),
        Expr::Select { arms } => arms
            .iter()
            .any(|arm| expr_mentions_var(&arm.receiver, name) || expr_mentions_var(&arm.body, name)),
        Expr::Index { base, index } => expr_mentions_var(base, name) || expr_mentions_var(index, name),
        Expr::ArrayLen { array } => expr_mentions_var(array, name),
        Expr::ArrayPush { array, value } => expr_mentions_var(array, name) || expr_mentions_var(value, name),
        Expr::ArrayPop { array } => expr_mentions_var(array, name),
        Expr::ArraySet { array, index, value } => {
            expr_mentions_var(array, name) || expr_mentions_var(index, name) || expr_mentions_var(value, name)
        }
        Expr::ArrayRemove { array, index } => expr_mentions_var(array, name) || expr_mentions_var(index, name),
        // `reuse_of` isn't checked against `name` here, same as
        // `CtorReuse`'s `reuse_of` isn't — see that precedent's own
        // comment above.
        Expr::ArrayPushReuse { value, .. } => expr_mentions_var(value, name),
        Expr::ArrayPopReuse { .. } => false,
        Expr::ArraySetReuse { index, value, .. } => expr_mentions_var(index, name) || expr_mentions_var(value, name),
        Expr::ArrayRemoveReuse { index, .. } => expr_mentions_var(index, name),
        Expr::StrConcat { base, other } => expr_mentions_var(base, name) || expr_mentions_var(other, name),
        Expr::StrConcatReuse { other, .. } => expr_mentions_var(other, name),
        Expr::StrRunes { base } => expr_mentions_var(base, name),
        Expr::StrTrim { base } => expr_mentions_var(base, name),
        Expr::StrTrimReuse { .. } => false,
        Expr::StrSplit { base, sep } => expr_mentions_var(base, name) || expr_mentions_var(sep, name),
        Expr::StrToUpper { base } => expr_mentions_var(base, name),
        Expr::StrToUpperReuse { .. } => false,
        Expr::StrToLower { base } => expr_mentions_var(base, name),
        Expr::StrToLowerReuse { .. } => false,
        Expr::StrContains { base, needle } => expr_mentions_var(base, name) || expr_mentions_var(needle, name),
        Expr::StrStartsWith { base, prefix } => expr_mentions_var(base, name) || expr_mentions_var(prefix, name),
        Expr::StrEndsWith { base, suffix } => expr_mentions_var(base, name) || expr_mentions_var(suffix, name),
        Expr::StrReplace { base, from, to } => {
            expr_mentions_var(base, name) || expr_mentions_var(from, name) || expr_mentions_var(to, name)
        }
        Expr::StrReplaceReuse { from, to, .. } => expr_mentions_var(from, name) || expr_mentions_var(to, name),
        Expr::ToString { base } => expr_mentions_var(base, name),
    }
}

/// A name is provably heap-shaped if it's a direct `Ctor` construction,
/// or a plain alias of an already-known-heap variable. Everything else
/// (call results, match results, literals) is left untracked — see
/// this module's scope note.
fn is_syntactically_heap(expr: &Expr, known_heap: &HashSet<String>) -> bool {
    match expr {
        Expr::Ctor { .. } => true,
        // A string LITERAL is now heap-allocated too — see `ir::Expr::
        // Str`'s doc comment. Same treatment as `Ctor` here: always
        // heap-shaped, unconditionally.
        Expr::Str(_) => true,
        // An empty array literal is heap-allocated too — see
        // `EmptyArray`'s own doc comment. Without this arm, `let a = [];
        // ...` would fall through to the catch-all `false` below and
        // never get refcount-tracked at all, silently leaking (never
        // Dec'd) rather than being unsound — but still a real
        // correctness gap worth avoiding outright.
        Expr::EmptyArray(_) => true,
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
        Expr::Var(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) => {
            (expr, live_after)
        }
        Expr::Unary(op, e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::Unary(op, Box::new(e_t)), used)
        }
        Expr::AsCStr(e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::AsCStr(Box::new(e_t)), used)
        }
        Expr::AsString(e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::AsString(Box::new(e_t)), used)
        }
        Expr::ToIntTrunc(e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::ToIntTrunc(Box::new(e_t)), used)
        }
        Expr::ToIntRound(e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::ToIntRound(Box::new(e_t)), used)
        }
        Expr::ToFloat(e) => {
            let (e_t, used) = mark_last_uses(*e, name, live_after);
            (Expr::ToFloat(Box::new(e_t)), used)
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
        Expr::ExternCall { name: fn_name, args } => {
            let mut acc_used = live_after;
            let mut new_args = Vec::with_capacity(args.len());
            for a in args.into_iter().rev() {
                let (a_t, used) = mark_last_uses(a, name, acc_used);
                acc_used = acc_used || used;
                new_args.push(a_t);
            }
            new_args.reverse();
            (
                Expr::ExternCall {
                    name: fn_name,
                    args: new_args,
                },
                acc_used,
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
        Expr::CtorReuse {
            reuse_of,
            tag,
            fields,
        } => {
            let mut acc_used = live_after;
            let mut new_fields = Vec::with_capacity(fields.len());
            for f in fields.into_iter().rev() {
                let (f_t, used) = mark_last_uses(f, name, acc_used);
                acc_used = acc_used || used;
                new_fields.push(f_t);
            }
            new_fields.reverse();
            (
                Expr::CtorReuse {
                    reuse_of,
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
                        // — its body (and guard) can't refer to the
                        // outer name.
                        arm
                    } else {
                        let (body_t, used_body) = mark_last_uses(arm.body, name, live_after);
                        // The guard runs BEFORE the body but AFTER the
                        // bindings, and — since body only runs if the
                        // guard passes — a use in the guard is
                        // conservatively always `live_after = true`
                        // when the body also uses `name`, same
                        // "leak over use-after-free" tradeoff as
                        // `For`'s body just below.
                        let (guard_t, used_guard) = match arm.guard {
                            Some(g) => {
                                let (g_t, used) = mark_last_uses(*g, name, live_after || used_body);
                                (Some(Box::new(g_t)), used)
                            }
                            None => (None, false),
                        };
                        let used = used_body || used_guard;
                        used_any_arm = used_any_arm || used;
                        MatchArm {
                            tag: arm.tag,
                            bindings: arm.bindings,
                            guard: guard_t,
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
        Expr::For { var, start, end, body } => {
            // `body` is a SINGLE syntactic subtree that may run zero,
            // one, or many times at runtime — ordinary last-use
            // reasoning doesn't apply to it. Marking a use of an OUTER
            // heap-tracked variable inside `body` as "the last use"
            // would insert a Dec that fires on the FIRST iteration,
            // freeing something a LATER iteration still needs.
            // Conservatively force `live_after = true` for the whole
            // body, so any use there always gets Inc'd (dup'd) rather
            // than treated as a move — this leaks one reference per
            // iteration instead of risking a use-after-free, the safe
            // direction to be wrong in. Precise per-iteration Dec
            // accounting is real loop-refcounting work, deferred.
            let (body_t, used_in_body) = if expr_mentions_var(&body, name) {
                let (t, _) = mark_last_uses(*body, name, true);
                (t, true)
            } else {
                (*body, false)
            };
            let after_loop = live_after || used_in_body;
            let (end_t, used_end) = mark_last_uses(*end, name, after_loop);
            let (start_t, used_start) = mark_last_uses(*start, name, after_loop || used_end);
            (
                Expr::For {
                    var,
                    start: Box::new(start_t),
                    end: Box::new(end_t),
                    body: Box::new(body_t),
                },
                used_start || used_end || used_in_body,
            )
        }
        Expr::Closure { params, param_types, ret_type, body } => {
            // Even more than a loop body, a closure can ESCAPE this
            // expression entirely — returned, stored, called later, or
            // called many times. A use of an outer heap-tracked
            // variable inside it can never be treated as a last use of
            // the OUTER binding, so `used` still forces `live_after`
            // outward exactly as before.
            //
            // But `body` itself is deliberately NOT recursed into with
            // this per-name walk (unlike `For`, which DOES recurse its
            // body with `live_after` forced true). A captured name's
            // lifetime inside the closure is no longer owned by this
            // OUTER walk at all — plum-codegen inc's each heap-shaped
            // capture exactly once at closure-creation time and dec's
            // it once when the closure cell's own refcount hits zero;
            // recursing here would additionally wrap EVERY mention of
            // `name` inside the body in its own `Inc` (since a forced-
            // true `live_after` never lets the inner walk see a "last
            // use"), one extra unmatched Inc per static occurrence,
            // firing once per closure CALL, not once per capture — an
            // unbounded leak proportional to call count. (Verified
            // this doesn't trade a leak for a use-after-free in either
            // backend: `RcOp::Dec` is only ever emitted by this pass's
            // `Let` arm's "name never referenced at all" case, gated
            // on `used == false` — a closure capturing `name` always
            // makes `used == true`, so that branch was already
            // unreachable for a captured name before this change, and
            // still is. No path ever Dec's a captured name's original
            // reference while an escaping closure holds it.)
            let used = expr_mentions_var(&body, name);
            (Expr::Closure { params, param_types, ret_type, body }, live_after || used)
        }
        // `block` runs on a genuinely separate thread/heap — even more
        // isolated than a `Closure`, since nothing inside it can
        // actually ALIAS a heap-tracked value from out here at all
        // (crossing means a deep copy, not a shared reference — see
        // ir.rs's `Spawn` doc comment). Still, the SOURCE-side use of
        // `name` (before it gets copied across) needs the same
        // forced-live-after treatment as `Closure`'s body: `block` may
        // run at some unknown later point, possibly after what would
        // otherwise look like `name`'s last use out here.
        Expr::Spawn { block } => {
            let (block_t, used) = if expr_mentions_var(&block, name) {
                let (t, _) = mark_last_uses(*block, name, true);
                (t, true)
            } else {
                (*block, false)
            };
            (Expr::Spawn { block: Box::new(block_t) }, live_after || used)
        }
        // An ordinary single-child node — `task` evaluates to a
        // `Value::Task`, never itself heap-tracked (see `is_syntactically_
        // heap`'s scope note), so no special escaping treatment applies.
        Expr::TaskJoin { task } => {
            let (task_t, used) = mark_last_uses(*task, name, live_after);
            (Expr::TaskJoin { task: Box::new(task_t) }, used)
        }
        Expr::Channel => (Expr::Channel, live_after),
        // `sender.send(value)`: evaluation order is sender-then-value
        // (matching an ordinary method-style call), so backward
        // analysis processes `value` first. Neither is heap-tracked
        // itself in the Ctor sense (a `Value::Sender` isn't a
        // `HeapRef`) — this is just ordinary sequential-subexpression
        // bookkeeping, same shape as `Binary`.
        Expr::ChannelSend { sender, value } => {
            let (value_t, used_value) = mark_last_uses(*value, name, live_after);
            let (sender_t, used_sender) = mark_last_uses(*sender, name, live_after || used_value);
            (
                Expr::ChannelSend {
                    sender: Box::new(sender_t),
                    value: Box::new(value_t),
                },
                used_sender || used_value,
            )
        }
        Expr::ChannelRecv { receiver } => {
            let (receiver_t, used) = mark_last_uses(*receiver, name, live_after);
            (Expr::ChannelRecv { receiver: Box::new(receiver_t) }, used)
        }
        Expr::RefNew { value } => {
            let (value_t, used) = mark_last_uses(*value, name, live_after);
            (Expr::RefNew { value: Box::new(value_t) }, used)
        }
        Expr::RefGet { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::RefGet { base: Box::new(base_t) }, used)
        }
        Expr::RefSet { base, value } => {
            let (value_t, used_value) = mark_last_uses(*value, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_value);
            (
                Expr::RefSet {
                    base: Box::new(base_t),
                    value: Box::new(value_t),
                },
                used_base || used_value,
            )
        }
        Expr::ReadFileRaw { path } => {
            let (path_t, used) = mark_last_uses(*path, name, live_after);
            (Expr::ReadFileRaw { path: Box::new(path_t) }, used)
        }
        Expr::WriteFileRaw { path, contents } => {
            let (contents_t, used_contents) = mark_last_uses(*contents, name, live_after);
            let (path_t, used_path) = mark_last_uses(*path, name, live_after || used_contents);
            (
                Expr::WriteFileRaw {
                    path: Box::new(path_t),
                    contents: Box::new(contents_t),
                },
                used_path || used_contents,
            )
        }
        Expr::EnvVarRaw { name: n } => {
            let (n_t, used) = mark_last_uses(*n, name, live_after);
            (Expr::EnvVarRaw { name: Box::new(n_t) }, used)
        }
        Expr::ArgsRaw => (Expr::ArgsRaw, false),
        Expr::RandomRaw => (Expr::RandomRaw, false),
        Expr::PanicRaw { message } => {
            let (message_t, used) = mark_last_uses(*message, name, live_after);
            (Expr::PanicRaw { message: Box::new(message_t) }, used)
        }
        // Bodies are ALTERNATIVES — only ONE arm's `body` actually
        // runs at runtime (whichever channel becomes ready first) —
        // same treatment as `Match`'s arms: each processed with the
        // SAME `live_after`, combined with OR. No separate shadowing
        // check is needed here the way `Match` needs one for its own
        // `bindings` field: a `Select` arm's received-value binding is
        // baked directly into `body` as an ordinary `Let`/`Match` node
        // (see `lower.rs`'s `wrap_select_arm_pattern`), so `body`'s own
        // existing shadowing logic already handles it correctly.
        // Receivers are evaluated UNCONDITIONALLY and SEQUENTIALLY
        // (every arm's channel expression runs, in order, before
        // waiting on any of them) — same backward-threading treatment
        // as `Call`'s args, starting from whatever the bodies
        // collectively needed.
        Expr::Select { arms } => {
            let mut used_any_body = false;
            let (receivers, bodies): (Vec<Expr>, Vec<Expr>) = arms.into_iter().map(|arm| (arm.receiver, arm.body)).unzip();
            let new_bodies: Vec<Expr> = bodies
                .into_iter()
                .map(|body| {
                    let (body_t, used) = mark_last_uses(body, name, live_after);
                    used_any_body = used_any_body || used;
                    body_t
                })
                .collect();
            let mut acc_used = live_after || used_any_body;
            let mut new_receivers = Vec::with_capacity(receivers.len());
            for r in receivers.into_iter().rev() {
                let (r_t, used) = mark_last_uses(r, name, acc_used);
                acc_used = acc_used || used;
                new_receivers.push(r_t);
            }
            new_receivers.reverse();
            let new_arms = new_receivers
                .into_iter()
                .zip(new_bodies)
                .map(|(receiver, body)| SelectArm { receiver, body })
                .collect();
            (Expr::Select { arms: new_arms }, acc_used)
        }
        // `base[index]` — evaluation order is `base` then `index`
        // (matching how a normal method-style call would evaluate its
        // receiver before its argument), so backward analysis
        // processes `index` first.
        Expr::Index { base, index } => {
            let (index_t, used_index) = mark_last_uses(*index, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_index);
            (
                Expr::Index {
                    base: Box::new(base_t),
                    index: Box::new(index_t),
                },
                used_base || used_index,
            )
        }
        Expr::ArrayLen { array } => {
            let (array_t, used) = mark_last_uses(*array, name, live_after);
            (Expr::ArrayLen { array: Box::new(array_t) }, used)
        }
        Expr::StrRunes { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::StrRunes { base: Box::new(base_t) }, used)
        }
        Expr::ToString { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::ToString { base: Box::new(base_t) }, used)
        }
        Expr::StrTrim { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::StrTrim { base: Box::new(base_t) }, used)
        }
        // Evaluation order `base`, then `sep` — same "receiver, then
        // argument" convention as `StrConcat`.
        Expr::StrSplit { base, sep } => {
            let (sep_t, used_sep) = mark_last_uses(*sep, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_sep);
            (
                Expr::StrSplit {
                    base: Box::new(base_t),
                    sep: Box::new(sep_t),
                },
                used_base || used_sep,
            )
        }
        Expr::StrToUpper { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::StrToUpper { base: Box::new(base_t) }, used)
        }
        Expr::StrToLower { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::StrToLower { base: Box::new(base_t) }, used)
        }
        Expr::StrContains { base, needle } => {
            let (needle_t, used_needle) = mark_last_uses(*needle, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_needle);
            (
                Expr::StrContains {
                    base: Box::new(base_t),
                    needle: Box::new(needle_t),
                },
                used_base || used_needle,
            )
        }
        Expr::StrStartsWith { base, prefix } => {
            let (prefix_t, used_prefix) = mark_last_uses(*prefix, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_prefix);
            (
                Expr::StrStartsWith {
                    base: Box::new(base_t),
                    prefix: Box::new(prefix_t),
                },
                used_base || used_prefix,
            )
        }
        Expr::StrEndsWith { base, suffix } => {
            let (suffix_t, used_suffix) = mark_last_uses(*suffix, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_suffix);
            (
                Expr::StrEndsWith {
                    base: Box::new(base_t),
                    suffix: Box::new(suffix_t),
                },
                used_base || used_suffix,
            )
        }
        // Evaluation order `base`, then `from`, then `to`.
        Expr::StrReplace { base, from, to } => {
            let (to_t, used_to) = mark_last_uses(*to, name, live_after);
            let (from_t, used_from) = mark_last_uses(*from, name, live_after || used_to);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_to || used_from);
            (
                Expr::StrReplace {
                    base: Box::new(base_t),
                    from: Box::new(from_t),
                    to: Box::new(to_t),
                },
                used_base || used_from || used_to,
            )
        }
        Expr::ArrayPush { array, value } => {
            let (value_t, used_value) = mark_last_uses(*value, name, live_after);
            let (array_t, used_array) = mark_last_uses(*array, name, live_after || used_value);
            (
                Expr::ArrayPush {
                    array: Box::new(array_t),
                    value: Box::new(value_t),
                },
                used_array || used_value,
            )
        }
        Expr::ArrayPop { array } => {
            let (array_t, used) = mark_last_uses(*array, name, live_after);
            (Expr::ArrayPop { array: Box::new(array_t) }, used)
        }
        // Evaluation order `array`, then `index`, then `value` — same
        // as `ArrayPush`'s "receiver, then argument(s)" convention, so
        // backward analysis processes them in reverse.
        Expr::ArraySet { array, index, value } => {
            let (value_t, used_value) = mark_last_uses(*value, name, live_after);
            let (index_t, used_index) = mark_last_uses(*index, name, live_after || used_value);
            let (array_t, used_array) = mark_last_uses(*array, name, live_after || used_value || used_index);
            (
                Expr::ArraySet {
                    array: Box::new(array_t),
                    index: Box::new(index_t),
                    value: Box::new(value_t),
                },
                used_array || used_index || used_value,
            )
        }
        Expr::ArrayRemove { array, index } => {
            let (index_t, used_index) = mark_last_uses(*index, name, live_after);
            let (array_t, used_array) = mark_last_uses(*array, name, live_after || used_index);
            (
                Expr::ArrayRemove {
                    array: Box::new(array_t),
                    index: Box::new(index_t),
                },
                used_array || used_index,
            )
        }
        // Evaluation order `base`, then `other` — same "receiver, then
        // argument" convention as `ArrayPush`.
        Expr::StrConcat { base, other } => {
            let (other_t, used_other) = mark_last_uses(*other, name, live_after);
            let (base_t, used_base) = mark_last_uses(*base, name, live_after || used_other);
            (
                Expr::StrConcat {
                    base: Box::new(base_t),
                    other: Box::new(other_t),
                },
                used_base || used_other,
            )
        }
        // Shouldn't normally appear as input here — same "produced BY
        // `mark_reuse`, which runs after this" precedent as
        // `CtorReuse`'s own case just above. `reuse_of` (a bare
        // String, not an `Expr`) isn't itself walked, same as
        // `CtorReuse`'s `reuse_of`.
        Expr::ArrayPushReuse { reuse_of, value } => {
            let (value_t, used) = mark_last_uses(*value, name, live_after);
            (
                Expr::ArrayPushReuse {
                    reuse_of,
                    value: Box::new(value_t),
                },
                used,
            )
        }
        Expr::ArrayPopReuse { reuse_of } => (Expr::ArrayPopReuse { reuse_of }, live_after),
        Expr::ArraySetReuse { reuse_of, index, value } => {
            let (value_t, used_value) = mark_last_uses(*value, name, live_after);
            let (index_t, used_index) = mark_last_uses(*index, name, live_after || used_value);
            (
                Expr::ArraySetReuse {
                    reuse_of,
                    index: Box::new(index_t),
                    value: Box::new(value_t),
                },
                used_index || used_value,
            )
        }
        Expr::ArrayRemoveReuse { reuse_of, index } => {
            let (index_t, used) = mark_last_uses(*index, name, live_after);
            (
                Expr::ArrayRemoveReuse {
                    reuse_of,
                    index: Box::new(index_t),
                },
                used,
            )
        }
        Expr::StrConcatReuse { reuse_of, other } => {
            let (other_t, used) = mark_last_uses(*other, name, live_after);
            (
                Expr::StrConcatReuse {
                    reuse_of,
                    other: Box::new(other_t),
                },
                used,
            )
        }
        Expr::StrTrimReuse { reuse_of } => (Expr::StrTrimReuse { reuse_of }, live_after),
        Expr::StrToUpperReuse { reuse_of } => (Expr::StrToUpperReuse { reuse_of }, live_after),
        Expr::StrToLowerReuse { reuse_of } => (Expr::StrToLowerReuse { reuse_of }, live_after),
        Expr::StrReplaceReuse { reuse_of, from, to } => {
            let (to_t, used_to) = mark_last_uses(*to, name, live_after);
            let (from_t, used_from) = mark_last_uses(*from, name, live_after || used_to);
            (
                Expr::StrReplaceReuse {
                    reuse_of,
                    from: Box::new(from_t),
                    to: Box::new(to_t),
                },
                used_from || used_to,
            )
        }
        // The reassignment TARGET (`name`) is a plain String field,
        // never an `Expr::Var` occurrence — so, unlike `Let`, there's
        // no shadowing case to special-case here even when the target
        // happens to equal the variable being tracked. `value` and
        // `rest` are just two ordinary sequential subexpressions
        // (`value` evaluates first), so this is standard backward
        // analysis, no forced-live-after needed the way `For`/`Closure`
        // bodies need it. What's NOT handled: if `name` is itself the
        // tracked heap-shaped variable, its OLD value isn't Dec'd at
        // the reassignment point — see ir.rs's `Assign` doc comment,
        // same accepted-leak precedent as `For`/`Closure`.
        Expr::Assign { name: target, value, rest } => {
            let (rest_t, used_rest) = mark_last_uses(*rest, name, live_after);
            let (value_t, used_value) = mark_last_uses(*value, name, used_rest || live_after);
            (
                Expr::Assign {
                    name: target,
                    value: Box::new(value_t),
                    rest: Box::new(rest_t),
                },
                used_rest || used_value,
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
    // Builds the `known_heap` set `mark_reuse_scoped` needs to treat a
    // name as reuse-eligible — see `mark_reuse_scoped`'s doc comment
    // for why this is now required, not optional: only names
    // `insert_refcount_ops` actually protects with Inc/Dec (i.e. would
    // land in this set for real) are safe reuse targets.
    fn known(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
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
    fn ctor_reuse(reuse_of: &str, tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::CtorReuse {
            reuse_of: reuse_of.to_string(),
            tag: tag.to_string(),
            fields,
        }
    }
    fn match_(scrutinee: Expr, arms: Vec<MatchArm>) -> Expr {
        Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        }
    }
    fn arm(tag: &str, bindings: Vec<&str>, body: Expr) -> MatchArm {
        MatchArm {
            tag: tag.to_string(),
            bindings: bindings.into_iter().map(|s| s.to_string()).collect(),
            guard: None,
            body,
        }
    }
    fn call(callee: Expr, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(callee),
            args,
        }
    }
    fn for_(var_name: &str, start: Expr, end: Expr, body: Expr) -> Expr {
        Expr::For {
            var: var_name.to_string(),
            start: Box::new(start),
            end: Box::new(end),
            body: Box::new(body),
        }
    }
    fn closure_(params: Vec<&str>, body: Expr) -> Expr {
        Expr::Closure {
            params: params.into_iter().map(|s| s.to_string()).collect(),
            param_types: None,
            ret_type: None,
            body: Box::new(body),
        }
    }
    fn assign_(name: &str, value: Expr, rest: Expr) -> Expr {
        Expr::Assign {
            name: name.to_string(),
            value: Box::new(value),
            rest: Box::new(rest),
        }
    }

    #[test]
    fn map_like_shape_marks_reuse() {
        // The canonical Perceus example: deconstruct a 2-field Cons,
        // reconstruct a 2-field Cons with entirely different (computed)
        // field values. Field COUNT matches (2 == 2), so this is a
        // reuse candidate even though no field is a bare copy of what
        // was torn down.
        let input = match_(
            var("list"),
            vec![
                arm(
                    "Cons",
                    vec!["head", "tail"],
                    ctor(
                        "Cons",
                        vec![call(var("f"), vec![var("head")]), call(var("map"), vec![var("f"), var("tail")])],
                    ),
                ),
                arm("Nil", vec![], ctor("Nil", vec![])),
            ],
        );
        let expected = match_(
            var("list"),
            vec![
                arm(
                    "Cons",
                    vec!["head", "tail"],
                    ctor_reuse(
                        "list",
                        "Cons",
                        vec![call(var("f"), vec![var("head")]), call(var("map"), vec![var("f"), var("tail")])],
                    ),
                ),
                // Nil has 0 fields — nothing worth reusing, stays a
                // plain Ctor. See zero_arity_is_not_a_reuse_candidate.
                arm("Nil", vec![], ctor("Nil", vec![])),
            ],
        );
        assert_eq!(mark_reuse_scoped(input, &known(&["list"])), expected);
    }

    #[test]
    fn a_scrutinee_not_present_in_known_heap_is_not_a_reuse_candidate() {
        // Same shape as `map_like_shape_marks_reuse`, but `list` was
        // never protected by `insert_refcount_ops` (e.g. it's a bare
        // function parameter) — must NOT be reused, see
        // `mark_reuse_scoped`'s doc comment for why.
        let input = match_(
            var("list"),
            vec![arm("Cons", vec!["head", "tail"], ctor("Cons", vec![var("head"), var("tail")]))],
        );
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn zero_arity_is_not_a_reuse_candidate() {
        let input = match_(var("list"), vec![arm("Nil", vec![], ctor("Nil", vec![]))]);
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn mismatched_arity_is_not_a_reuse_candidate() {
        // Deconstructs 2 fields but reconstructs a 3-field value —
        // different shape, not safe to overwrite in place.
        let input = match_(
            var("pair"),
            vec![arm(
                "Pair",
                vec!["a", "b"],
                ctor("Triple", vec![var("a"), var("b"), int(0)]),
            )],
        );
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn arm_body_not_a_direct_ctor_is_not_a_reuse_candidate() {
        let input = match_(
            var("pair"),
            vec![arm("Pair", vec!["a", "b"], var("a"))],
        );
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn non_variable_scrutinee_is_not_a_reuse_candidate() {
        // Can't name a specific cell to reuse if the scrutinee isn't a
        // plain variable (e.g. it's a call result).
        let input = match_(
            call(var("get_pair"), vec![]),
            vec![arm(
                "Pair",
                vec!["a", "b"],
                ctor("Pair", vec![var("b"), var("a")]),
            )],
        );
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn array_push_on_a_plain_variable_marks_reuse() {
        let input = Expr::ArrayPush {
            array: Box::new(var("a")),
            value: Box::new(int(3)),
        };
        let expected = Expr::ArrayPushReuse {
            reuse_of: "a".to_string(),
            value: Box::new(int(3)),
        };
        assert_eq!(mark_reuse_scoped(input, &known(&["a"])), expected);
    }

    #[test]
    fn array_push_on_a_variable_absent_from_known_heap_is_not_a_reuse_candidate() {
        // `a` was never protected by `insert_refcount_ops` (e.g. a bare
        // function parameter) — see `mark_reuse_scoped`'s doc comment.
        let input = Expr::ArrayPush {
            array: Box::new(var("a")),
            value: Box::new(int(3)),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn array_pop_on_a_plain_variable_marks_reuse() {
        let input = Expr::ArrayPop { array: Box::new(var("a")) };
        let expected = Expr::ArrayPopReuse { reuse_of: "a".to_string() };
        assert_eq!(mark_reuse_scoped(input, &known(&["a"])), expected);
    }

    #[test]
    fn array_set_on_a_plain_variable_marks_reuse() {
        let input = Expr::ArraySet {
            array: Box::new(var("a")),
            index: Box::new(int(0)),
            value: Box::new(int(9)),
        };
        let expected = Expr::ArraySetReuse {
            reuse_of: "a".to_string(),
            index: Box::new(int(0)),
            value: Box::new(int(9)),
        };
        assert_eq!(mark_reuse_scoped(input, &known(&["a"])), expected);
    }

    #[test]
    fn array_remove_on_a_plain_variable_marks_reuse() {
        let input = Expr::ArrayRemove {
            array: Box::new(var("a")),
            index: Box::new(int(0)),
        };
        let expected = Expr::ArrayRemoveReuse {
            reuse_of: "a".to_string(),
            index: Box::new(int(0)),
        };
        assert_eq!(mark_reuse_scoped(input, &known(&["a"])), expected);
    }

    #[test]
    fn array_push_on_a_non_variable_base_is_not_a_reuse_candidate() {
        // Can't name a specific cell to reuse if the base isn't a plain
        // variable — same "non_variable_scrutinee" precedent as Match.
        let input = Expr::ArrayPush {
            array: Box::new(call(var("get_array"), vec![])),
            value: Box::new(int(3)),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn str_concat_on_a_plain_variable_marks_reuse() {
        let input = Expr::StrConcat {
            base: Box::new(var("s")),
            other: Box::new(Expr::Str("x".to_string())),
        };
        let expected = Expr::StrConcatReuse {
            reuse_of: "s".to_string(),
            other: Box::new(Expr::Str("x".to_string())),
        };
        assert_eq!(mark_reuse_scoped(input, &known(&["s"])), expected);
    }

    #[test]
    fn str_concat_on_a_variable_absent_from_known_heap_is_not_a_reuse_candidate() {
        // `s` was never protected by `insert_refcount_ops` (e.g. a bare
        // function parameter) — see `mark_reuse_scoped`'s doc comment.
        // This is the exact shape of the real bug found while building
        // `String.repeat` (chunk 15): `s.concat(rep(s, n - 1))` where
        // `s` is aliased between this call's receiver and a nested use
        // — without this guard, both would wrongly believe they own
        // the only reference and both would reuse the same cell.
        let input = Expr::StrConcat {
            base: Box::new(var("s")),
            other: Box::new(Expr::Str("x".to_string())),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn str_concat_on_a_non_variable_base_is_not_a_reuse_candidate() {
        let input = Expr::StrConcat {
            base: Box::new(call(var("get_str"), vec![])),
            other: Box::new(Expr::Str("x".to_string())),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn str_trim_on_a_plain_variable_marks_reuse() {
        let input = Expr::StrTrim { base: Box::new(var("s")) };
        let expected = Expr::StrTrimReuse { reuse_of: "s".to_string() };
        assert_eq!(mark_reuse_scoped(input, &known(&["s"])), expected);
    }

    #[test]
    fn str_trim_on_a_non_variable_base_is_not_a_reuse_candidate() {
        let input = Expr::StrTrim {
            base: Box::new(call(var("get_str"), vec![])),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn str_to_upper_on_a_plain_variable_marks_reuse() {
        let input = Expr::StrToUpper { base: Box::new(var("s")) };
        let expected = Expr::StrToUpperReuse { reuse_of: "s".to_string() };
        assert_eq!(mark_reuse_scoped(input, &known(&["s"])), expected);
    }

    #[test]
    fn str_to_lower_on_a_plain_variable_marks_reuse() {
        let input = Expr::StrToLower { base: Box::new(var("s")) };
        let expected = Expr::StrToLowerReuse { reuse_of: "s".to_string() };
        assert_eq!(mark_reuse_scoped(input, &known(&["s"])), expected);
    }

    #[test]
    fn str_replace_on_a_plain_variable_marks_reuse() {
        let input = Expr::StrReplace {
            base: Box::new(var("s")),
            from: Box::new(Expr::Str("x".to_string())),
            to: Box::new(Expr::Str("y".to_string())),
        };
        let expected = Expr::StrReplaceReuse {
            reuse_of: "s".to_string(),
            from: Box::new(Expr::Str("x".to_string())),
            to: Box::new(Expr::Str("y".to_string())),
        };
        assert_eq!(mark_reuse_scoped(input, &known(&["s"])), expected);
    }

    #[test]
    fn str_replace_on_a_non_variable_base_is_not_a_reuse_candidate() {
        let input = Expr::StrReplace {
            base: Box::new(call(var("get_str"), vec![])),
            from: Box::new(Expr::Str("x".to_string())),
            to: Box::new(Expr::Str("y".to_string())),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn str_contains_is_never_a_reuse_candidate() {
        // Returns `Bool`, never heap-allocated — nothing to reuse into,
        // so `mark_reuse` leaves it structurally unchanged regardless
        // of whether `base` is a plain variable.
        let input = Expr::StrContains {
            base: Box::new(var("s")),
            needle: Box::new(Expr::Str("x".to_string())),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn ref_set_is_never_a_reuse_candidate() {
        // `Ref` cells always mutate unconditionally in place at
        // runtime already (see `RefHandle`'s doc comment, plum-interp)
        // — `mark_reuse` never touches `RefNew`/`RefGet`/`RefSet` at
        // all, regardless of whether `base` is a plain variable.
        let input = Expr::RefSet {
            base: Box::new(var("r")),
            value: Box::new(int(6)),
        };
        assert_eq!(mark_reuse(input.clone()), input);
    }

    #[test]
    fn a_let_bound_ref_is_not_tracked_as_heap_shaped() {
        // `Ref` lives entirely outside the toy `Heap`/FBIP's refcount-
        // insertion machinery (unlike `Ctor`/`Str`) — a `let`-bound
        // `Ref` with zero further uses does NOT get the "insert a Dec"
        // treatment `zero_uses_drops_immediately` proves for `Ctor`.
        let input = let_(
            "r",
            Expr::RefNew { value: Box::new(int(5)) },
            int(10),
        );
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn recurses_into_nested_matches() {
        let inner = match_(
            var("inner_list"),
            vec![arm(
                "Cons",
                vec!["h", "t"],
                ctor("Cons", vec![var("h"), var("t")]),
            )],
        );
        let expected_inner = match_(
            var("inner_list"),
            vec![arm(
                "Cons",
                vec!["h", "t"],
                ctor_reuse("inner_list", "Cons", vec![var("h"), var("t")]),
            )],
        );
        let input = let_("inner_list", var("outer"), inner);
        let expected = let_("inner_list", var("outer"), expected_inner);
        // `outer` itself must already be known-heap for `inner_list`'s
        // `Var("outer")` value to qualify — see `mark_reuse_scoped`'s
        // `Let` handling.
        assert_eq!(mark_reuse_scoped(input, &known(&["outer"])), expected);
    }

    #[test]
    fn composes_after_refcount_insertion() {
        // The realistic pipeline: refcount insertion runs first, then
        // reuse marking on its output — starting from a `Let`-bound
        // `Ctor`, the one shape `insert_refcount_ops` actually proves
        // heap-shaped without a type checker (a bare `Var("list")`
        // scrutinee with no enclosing `Let`, e.g. a function parameter,
        // is deliberately NOT a reuse candidate — see
        // `a_bare_untracked_scrutinee_does_not_reuse_even_through_the_
        // full_pipeline` below for that regression).
        let input = let_(
            "list",
            ctor("Cons", vec![int(1), ctor("Nil", vec![])]),
            match_(
                var("list"),
                vec![
                    arm(
                        "Cons",
                        vec!["head", "tail"],
                        ctor("Cons", vec![var("head"), var("tail")]),
                    ),
                    arm("Nil", vec![], ctor("Nil", vec![])),
                ],
            ),
        );
        let piped = optimize(input);
        match &piped {
            Expr::Let { body, .. } => match body.as_ref() {
                Expr::Match { arms, .. } => match &arms[0].body {
                    Expr::CtorReuse { reuse_of, tag, .. } => {
                        assert_eq!(reuse_of, "list");
                        assert_eq!(tag, "Cons");
                    }
                    other => panic!("expected a CtorReuse candidate, got {other:?}"),
                },
                other => panic!("expected a Match, got {other:?}"),
            },
            other => panic!("expected a Let, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_untracked_scrutinee_does_not_reuse_even_through_the_full_pipeline() {
        // Same shape as `composes_after_refcount_insertion`, but `list`
        // is a bare, un-`Let`-bound variable — the realistic shape of a
        // function PARAMETER, which `insert_refcount_ops` never proves
        // heap-shaped (no type checker in this IR). Must NOT reuse,
        // even after the full `optimize` pipeline — the regression this
        // whole fix closes (see `mark_reuse_scoped`'s doc comment).
        let input = match_(
            var("list"),
            vec![
                arm(
                    "Cons",
                    vec!["head", "tail"],
                    ctor("Cons", vec![var("head"), var("tail")]),
                ),
                arm("Nil", vec![], ctor("Nil", vec![])),
            ],
        );
        let piped = optimize(input);
        match &piped {
            Expr::Match { arms, .. } => match &arms[0].body {
                Expr::Ctor { tag, .. } => assert_eq!(tag, "Cons"),
                other => panic!("expected a plain (non-reused) Ctor, got {other:?}"),
            },
            other => panic!("expected a Match, got {other:?}"),
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
    fn a_let_bound_string_literal_is_tracked_as_heap_shaped() {
        // `Expr::Str` is now heap-allocated (see `ir::Expr::Str`'s doc
        // comment) — a `let`-bound string with zero further uses gets
        // the exact same "drops immediately" treatment a `Ctor` does,
        // proving `is_syntactically_heap` recognizes it.
        let input = let_("s", Expr::Str("hi".to_string()), int(5));
        let expected = let_("s", Expr::Str("hi".to_string()), dec("s", int(5)));
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
    fn a_heap_value_used_inside_a_loop_body_is_always_dupd_never_moved() {
        // `p` is bound before the loop and used inside `body`, which may
        // run zero, one, or many times at runtime. If this used ordinary
        // last-use reasoning, the (syntactically single) use inside the
        // loop body would be treated as the LAST use and the binding
        // would move there with no Inc — freeing `p` after the FIRST
        // iteration and leaving nothing for a second one. It must always
        // be Inc'd instead.
        let input = let_(
            "p",
            ctor("Point", vec![]),
            for_("i", int(0), int(5), var("p")),
        );
        let expected = let_(
            "p",
            ctor("Point", vec![]),
            for_("i", int(0), int(5), inc("p", var("p"))),
        );
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn a_heap_value_never_used_in_the_loop_is_dropped_immediately_as_usual() {
        // Same "never referenced at all" handling as everywhere else in
        // this pass (see transform's Let arm) — a loop that doesn't
        // touch `p` at all is no different from any other dead binding.
        let input = let_("p", ctor("Point", vec![]), for_("i", int(0), int(5), int(0)));
        let expected = let_(
            "p",
            ctor("Point", vec![]),
            dec("p", for_("i", int(0), int(5), int(0))),
        );
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn the_loop_variable_itself_is_never_treated_as_heap_shaped() {
        // `i` is an Int (the only iterable is a Range), so a `Let`
        // binding a Ctor to the same-shaped name elsewhere shouldn't be
        // confused by anything the loop does with `i` — this mostly
        // proves `transform`/`mark_reuse` recurse into a `For` without
        // panicking on a node shape they don't otherwise touch.
        let input = for_("i", int(0), int(5), var("i"));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn mark_reuse_recurses_into_a_loop_body() {
        // A reuse-eligible shape (deconstruct-then-reconstruct-same-
        // arity) still gets marked even when it's inside a loop body —
        // `mark_reuse` doesn't need to special-case `For` the way
        // `mark_last_uses` does, since it isn't reasoning about
        // sequencing/liveness, just local match-arm shapes.
        let input = for_(
            "i",
            int(0),
            int(5),
            match_(var("list"), vec![arm("Cons", vec!["h", "t"], ctor("Cons", vec![var("h"), var("t")]))]),
        );
        let expected = for_(
            "i",
            int(0),
            int(5),
            match_(
                var("list"),
                vec![arm("Cons", vec!["h", "t"], ctor_reuse("list", "Cons", vec![var("h"), var("t")]))],
            ),
        );
        assert_eq!(mark_reuse_scoped(input, &known(&["list"])), expected);
    }

    #[test]
    fn a_heap_value_captured_by_a_closure_is_not_inc_d_per_mention_inside_the_body() {
        // `p` is bound before the closure literal and used inside its
        // body. The BODY itself is left untouched by this pass — no
        // `Inc` planted on `p`'s mention — since `p`'s lifetime inside
        // the closure is owned by the closure cell itself (plum-codegen
        // inc's it once at capture time, dec's it once at release);
        // planting an Inc here too would be an extra unmatched
        // increment firing once per closure CALL, not once per
        // capture. The closure literal itself still isn't `move`d/
        // consumed as `p`'s own last use (see the next test).
        let input = let_("p", ctor("Point", vec![]), closure_(vec!["x"], var("p")));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn a_heap_value_captured_and_mentioned_multiple_times_in_a_closure_body_is_never_inc_d() {
        // The exact scenario a real leak was found in: `p` mentioned
        // twice in one closure body used to plant TWO separate `Inc`s
        // (one per static occurrence), each firing on every call.
        let input = let_(
            "p",
            ctor("Point", vec![]),
            closure_(vec!["x"], Expr::Binary(BinOp::Add, Box::new(var("p")), Box::new(var("p")))),
        );
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn a_heap_value_never_used_in_a_closure_is_dropped_immediately_as_usual() {
        let input = let_("p", ctor("Point", vec![]), closure_(vec!["x"], var("x")));
        let expected = let_("p", ctor("Point", vec![]), dec("p", closure_(vec!["x"], var("x"))));
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn mark_reuse_recurses_into_a_closure_body() {
        let input = closure_(
            vec!["list"],
            match_(var("list"), vec![arm("Cons", vec!["h", "t"], ctor("Cons", vec![var("h"), var("t")]))]),
        );
        let expected = closure_(
            vec!["list"],
            match_(
                var("list"),
                vec![arm("Cons", vec!["h", "t"], ctor_reuse("list", "Cons", vec![var("h"), var("t")]))],
            ),
        );
        assert_eq!(mark_reuse_scoped(input, &known(&["list"])), expected);
    }

    #[test]
    fn reassigning_a_non_heap_value_inserts_no_refcount_ops() {
        // The classic accumulator shape (`sum = sum + i`) never touches
        // anything heap-shaped, so this pass should be a complete
        // no-op on it — proves ordinary numeric mutation doesn't drag
        // in any refcounting machinery at all.
        let input = let_(
            "sum",
            int(0),
            assign_(
                "sum",
                Expr::Binary(BinOp::Add, Box::new(var("sum")), Box::new(int(1))),
                var("sum"),
            ),
        );
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn reassigning_a_heap_tracked_variable_leaks_the_old_value_by_design() {
        // See ir.rs's `Assign` doc comment: the ORIGINAL `Point{}` is
        // never Dec'd anywhere in the output — reassignment orphans it.
        // This is the accepted, documented leak (never a soundness
        // issue, since nothing ever double-frees or use-after-frees
        // it — it's just never freed at all), matching the same
        // tradeoff already made for `For`/`Closure`. Proven here by
        // showing the WHOLE tree comes back completely unchanged: no
        // Dec appears for the orphaned original anywhere.
        let input = let_(
            "p",
            ctor("Point", vec![]),
            assign_("p", ctor("Point", vec![]), var("p")),
        );
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }

    #[test]
    fn mark_reuse_recurses_into_assign_value_and_rest() {
        let input = assign_(
            "x",
            match_(var("list"), vec![arm("Cons", vec!["h", "t"], ctor("Cons", vec![var("h"), var("t")]))]),
            match_(var("list2"), vec![arm("Cons", vec!["h", "t"], ctor("Cons", vec![var("h"), var("t")]))]),
        );
        let expected = assign_(
            "x",
            match_(
                var("list"),
                vec![arm("Cons", vec!["h", "t"], ctor_reuse("list", "Cons", vec![var("h"), var("t")]))],
            ),
            match_(
                var("list2"),
                vec![arm("Cons", vec!["h", "t"], ctor_reuse("list2", "Cons", vec![var("h"), var("t")]))],
            ),
        );
        assert_eq!(mark_reuse_scoped(input, &known(&["list", "list2"])), expected);
    }

    #[test]
    fn optimize_program_runs_optimize_on_every_function_body() {
        // A function that just constructs a value should come out with
        // refcount ops inserted, exactly as if `optimize` had been
        // called on its body directly — proving the program-level
        // wrapper isn't a no-op.
        let program = Program {
            functions: vec![Function {
                name: "make".to_string(),
                params: vec![],
                body: let_("p", ctor("Point", vec![int(1), int(2)]), var("p")),
            }],
            globals: vec![],
            externs: vec![],
        };
        let optimized = optimize_program(program);
        assert_eq!(optimized.functions.len(), 1);
        assert_eq!(
            optimized.functions[0].body,
            optimize(let_("p", ctor("Point", vec![int(1), int(2)]), var("p")))
        );
    }

    #[test]
    fn optimize_program_preserves_function_names_params_and_order() {
        let program = Program {
            functions: vec![
                Function {
                    name: "first".to_string(),
                    params: vec!["a".to_string()],
                    body: var("a"),
                },
                Function {
                    name: "second".to_string(),
                    params: vec!["b".to_string(), "c".to_string()],
                    body: var("b"),
                },
            ],
            globals: vec![],
            externs: vec![],
        };
        let optimized = optimize_program(program);
        assert_eq!(optimized.functions[0].name, "first");
        assert_eq!(optimized.functions[0].params, vec!["a".to_string()]);
        assert_eq!(optimized.functions[1].name, "second");
        assert_eq!(optimized.functions[1].params, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn optimize_program_runs_optimize_on_every_global_too() {
        let program = Program {
            functions: vec![],
            globals: vec![Global {
                name: "origin".to_string(),
                value: let_("p", ctor("Point", vec![int(1), int(2)]), var("p")),
            }],
            externs: vec![],
        };
        let optimized = optimize_program(program);
        assert_eq!(optimized.globals[0].name, "origin");
        assert_eq!(
            optimized.globals[0].value,
            optimize(let_("p", ctor("Point", vec![int(1), int(2)]), var("p")))
        );
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

    // --- `.as_cstr()` on an untracked variable — the real use-after-
    // free found and fixed while building the OS module (see `transform
    // `'s own `AsCStr` arm doc comment for the full "why"). ---

    #[test]
    fn as_cstr_on_an_untracked_variable_is_wrapped_with_a_protective_inc() {
        // `s` here is a bare free variable (no enclosing `let`), the
        // EXACT "unprotected parameter"/"untracked global" shape that
        // corrupted for real — `insert_refcount_ops` starts with an
        // empty `known_heap`, so `s` is never tracked no matter what
        // it's aliased from.
        let input = Expr::AsCStr(Box::new(var("s")));
        assert_eq!(insert_refcount_ops(input.clone()), inc("s", Expr::AsCStr(Box::new(var("s")))));
    }

    #[test]
    fn as_cstr_on_a_tracked_last_use_needs_no_extra_protection() {
        // `s` comes from a real `let` and is used exactly once — the
        // EXISTING last-use machinery already proves this consume is
        // safe, so the NEW untracked-only fix must not fire here (no
        // redundant `Inc` on top of what's already correct).
        let input = let_("s", Expr::Str("hi".to_string()), Expr::AsCStr(Box::new(var("s"))));
        assert_eq!(insert_refcount_ops(input), let_("s", Expr::Str("hi".to_string()), Expr::AsCStr(Box::new(var("s")))));
    }

    #[test]
    fn as_cstr_on_a_tracked_variable_used_again_afterward_is_dupd_by_the_existing_machinery() {
        // A tracked `s` used by `.as_cstr()` FIRST, then again
        // afterward, already needs (and already gets, via the pre-
        // existing `mark_last_uses` pass, not this fix) a protective
        // `Inc` before the `.as_cstr()` call — this proves the OLD
        // mechanism and the NEW untracked-only fix don't double up for
        // a tracked name.
        let input = let_(
            "s",
            Expr::Str("hi".to_string()),
            Expr::Binary(BinOp::Add, Box::new(Expr::AsCStr(Box::new(var("s")))), Box::new(var("s"))),
        );
        // `mark_last_uses` wraps the specific (non-last) OCCURRENCE
        // with its protective `Inc`, not the whole surrounding
        // expression — confirmed by running it, not assumed.
        let expected = let_(
            "s",
            Expr::Str("hi".to_string()),
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::AsCStr(Box::new(inc("s", var("s"))))),
                Box::new(var("s")),
            ),
        );
        assert_eq!(insert_refcount_ops(input), expected);
    }

    #[test]
    fn as_cstr_on_a_non_variable_operand_is_left_alone() {
        // A fresh, unnamed temporary (a literal here) has no binding
        // for a protective `Inc` to even target, and needs none — it's
        // not shared with anything else by construction.
        let input = Expr::AsCStr(Box::new(Expr::Str("hi".to_string())));
        assert_eq!(insert_refcount_ops(input.clone()), input);
    }
}
