use crate::ir::{Expr, Function, Global, MatchArm, Program, RcOp, SelectArm};
use std::collections::{HashMap, HashSet};

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
///
/// **REVERTED, not just never-attempted**: this briefly seeded `known_
/// heap` with parameters PROVEN array/string-shaped by their own body's
/// use (via `confirmed_array_or_str_params`, still present below,
/// unused) — intended to fix a real perf bug (self-tail-recursive
/// accumulators threaded through a parameter never reaching the reuse-
/// in-place `.push()` path, see DESIGN.md's "Array push scaling bug"
/// section). It was wrong: syntactic proof that a parameter is ARRAY-
/// SHAPED isn't proof that it's SAFE TO REUSE — a parameter can be
/// array-shaped AND still be an alias of something the CALLER still
/// needs (e.g. `bind_params`'s own `acc` parameter, called with a
/// closure's CAPTURED environment — extracted via a `Match` arm binding
/// that this pass has never tracked as `known_heap` either, so nothing
/// upstream Inc'd it before handing it in), and this pass's `known_heap`
/// gating was never meant to answer "is this the right TYPE," only "do
/// I actually know this name's refcount is being correctly threaded" —
/// see `mark_reuse_scoped`'s own doc comment for why that distinction
/// is load-bearing. Found via a REAL crash (`exec_corpus/closures`,
/// `apply_twice` calling the same closure twice — a segfault, not
/// silently wrong output), not by inspection; reverted the same session
/// rather than shipping something merely believed safe. A genuinely
/// correct fix needs to also account for match-extracted bindings'
/// ownership (deeper than parameter-tracking alone) — left for a future,
/// more careful pass; see DESIGN.md's own section for the full story.
pub fn optimize_program(program: Program) -> Program {
    let reusable = reusable_params(&program);
    optimize_program_with_reusable_params(program, &reusable)
}

/// `optimize_program` with the parameter-reuse eligibility supplied from
/// outside.
///
/// Exists because that analysis must be computed BEFORE `anf` runs, while
/// this pass runs after it — see `reusable_params`' own ordering note. The
/// codegen pipeline computes it up front and hands it in here; the
/// interpreter's path has no `anf` stage and lets `optimize_program`
/// derive it directly.
///
/// The eligible parameters seed `mark_reuse`'s `known_heap`, and ONLY
/// `mark_reuse`'s. `insert_refcount_ops` is untouched: nothing here adds
/// an increment or a release for a parameter, so no calling convention
/// changes. This deliberately relaxes the invariant
/// `mark_reuse_scoped`'s doc comment describes — reuse normally fires only
/// for names `insert_refcount_ops` protected — and `reusable_params`'
/// two conditions are what stand in for that protection instead.
pub fn optimize_program_with_reusable_params(
    program: Program,
    reusable: &HashMap<String, HashSet<String>>,
) -> Program {
    let empty = HashSet::new();
    Program {
        functions: program
            .functions
            .into_iter()
            .map(|f| {
                let seed = reusable.get(&f.name).unwrap_or(&empty);
                Function {
                    body: mark_reuse_scoped(insert_refcount_ops(f.body), seed),
                    name: f.name,
                    params: f.params,
                }
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
            // A name `insert_refcount_ops` decided to release at scope
            // end (`all_uses_are_borrows`) must NOT also be a reuse
            // candidate. `CtorReuse` consumes the old cell — it releases
            // it before overwriting in place — so a scope-end release on
            // top of that is a double free. This is the one interaction
            // between the two halves of FBIP where getting it wrong is
            // memory-unsafe rather than merely leaky.
            //
            // Detected by looking for the `Dec` itself rather than by
            // recomputing `all_uses_are_borrows` here: the two passes
            // would then have to agree on that predicate forever, and
            // this pass runs on `insert_refcount_ops`' OUTPUT, so the
            // decision it needs is already written into the tree. The
            // pre-existing `used == false` release is covered by the same
            // check, harmlessly — a name nothing references was never a
            // reuse candidate anyway.
            let is_heap_value = is_syntactically_heap(&value, known_heap);
            let value_t = mark_reuse_scoped(*value, known_heap);
            let mut inner_heap = known_heap.clone();
            if is_heap_value {
                inner_heap.insert(name.clone());
            }
            let body_t = mark_reuse_scoped(*body, &inner_heap);
            // Reuse and scope-end release both claim the SAME reference,
            // so exactly one of them may happen — doing both is a double
            // free. Reuse WINS wherever it fires: it avoids the
            // allocation as well as releasing the cell, so it strictly
            // dominates.
            //
            // Resolved here rather than in `insert_refcount_ops` because
            // this is where the decision is actually made. That pass runs
            // first and cannot know whether reuse will fire without
            // duplicating the conditions above — and a duplicated
            // predicate that drifted apart would produce a double free,
            // the worst possible failure mode. So the release is inserted
            // optimistically there and retracted here, by the code that
            // just decided to reuse.
            let body_t = if reuses_name(&body_t, &name) {
                strip_scope_end_release(body_t, &name)
            } else {
                body_t
            };
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
        Expr::Channel { tag } => Expr::Channel { tag: tag.clone() },
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
        Expr::StrHash { base } => Expr::StrHash {
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
        Expr::Channel { tag } => Expr::Channel { tag: tag.clone() },
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
        Expr::StrHash { base } => Expr::StrHash {
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

            let releasable = is_heap_value || allocates_fresh_heap(&value_t);
            let (marked, used) = if releasable {
                mark_last_uses(body_t.clone(), &name, false)
            } else {
                (body_t.clone(), false)
            };

            // Every use is a BORROW — see `all_uses_are_borrows`. The
            // binding is released at the end of its scope, and no use
            // needs a `dup` at all (`marked` is discarded), because the
            // binding itself keeps the cell alive across all of them.
            //
            // This is the case Perceus's "the last use CONSUMES" rule
            // gets wrong, and it is the ordinary case, not an exotic one:
            // `let p = Point { .. }; match p { .. }` leaked the cell
            // outright, measured linear at 13.7/47.9/185.1 MB for
            // 250k/1M/4M iterations. Before this arm existed, the ONLY
            // `Dec` this pass ever emitted was the `used == false` one
            // below, so every heap value a program actually USED was
            // never freed — see DESIGN.md's own memory-model section,
            // which has specified "at a variable's final use, insert a
            // `drop`" the whole time.
            //
            // Gated on `used` so the `used == false` case below still
            // wins: a binding with no uses at all trivially satisfies
            // `all_uses_are_borrows`, but releasing it IMMEDIATELY is
            // strictly better than at scope end. `used` is also the
            // shadowing-aware answer — `expr_mentions_var` is
            // deliberately coarse and would call an immediately-shadowed
            // binding "used".
            //
            // Gated on `!incs_name(&marked, ..)` — i.e. exactly ONE use —
            // for a much sharper reason, found as a real wrong-answer
            // regression (`let s = "ab"; let t = s.concat("cd");
            // s.len() + t.len()` returned 8 instead of 6). Those `Inc`s
            // are not merely release bookkeeping: reuse-in-place has NO
            // static check at all, only a runtime `rc == 1` test, so a
            // non-last use's `Inc` is the only thing stopping
            // `StrConcatReuse` from destructively overwriting a string
            // that is still needed afterwards. Take the increments away
            // and reuse silently corrupts the value.
            //
            // With exactly one use there is no increment to remove, so
            // reuse keeps working unchanged and multi-use bindings keep
            // today's behaviour byte for byte. Every case measured for
            // this change is single-use anyway — `let p = Point { .. };
            // match p { .. }` and friends.
            if releasable && used && !incs_name(&marked, &name) && all_uses_are_borrows(&body_t, &name) {
                return Expr::Let {
                    name: name.clone(),
                    value: Box::new(value_t),
                    body: Box::new(drop_at_scope_end(body_t, &name)),
                };
            }

            let final_body = if is_heap_value {
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

/// Whether `expr` ALWAYS evaluates to a freshly allocated heap cell this
/// binding therefore owns outright.
///
/// Deliberately kept OUT of `is_syntactically_heap`, and that separation
/// is the point. `is_syntactically_heap` feeds both `mark_last_uses` and
/// `mark_reuse`, so widening it would newly expose these shapes to
/// dup-insertion and to reuse-in-place — a broad behaviour change with
/// real risk, and the same kind of widening whose earlier attempt is
/// recorded in DESIGN.md as found unsafe and reverted. This predicate is
/// consulted at exactly ONE site: the scope-end release for a binding
/// whose every use is a borrow. So it can only ever turn a leak into a
/// release, never perturb an existing decision.
///
/// It exists because `let s = a.concat(b); s.len()` leaked 139MB per 1M
/// iterations while the `Ctor` version was fixed by the borrow analysis
/// alone: `StrConcat` is not a `Ctor`/`Str` literal/`EmptyArray`, so
/// `is_syntactically_heap` never saw it as heap at all.
///
/// Every arm below is type-INDEPENDENT — it allocates whatever its
/// operand types are — which is what makes a `Dec` on the result always
/// well-defined. Two exclusions are load-bearing:
///
/// - `AsString` may return its input register UNCHANGED when that input
///   is already a `Str` (see `codegen_as_string`), so the result can
///   alias a cell this binding does not own.
/// - Every `*Reuse` node may return the very cell it reused, which
///   belongs to a different binding. Releasing it here would be a double
///   free.
fn allocates_fresh_heap(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::StrConcat { .. }
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

/// Whether `expr` contains an `Inc` targeting `name` — i.e. whether
/// `mark_last_uses` found more than one use of it. See
/// `insert_refcount_ops`' `Let` arm for why that distinction is
/// load-bearing rather than an optimization detail.
fn incs_name(expr: &Expr, name: &str) -> bool {
    if let Expr::RcAnnotated { op: RcOp::Inc, target, .. } = expr {
        if target == name {
            return true;
        }
    }
    let mut found = false;
    for_each_child(expr, &mut |c| {
        if !found && incs_name(c, name) {
            found = true;
        }
    });
    found
}

/// Whether `expr` reuses `name`'s cell in place — i.e. whether one of
/// the `*Reuse` nodes this pass produces has taken over responsibility
/// for that reference. See `mark_reuse_scoped`'s `Let` arm.
fn reuses_name(expr: &Expr, name: &str) -> bool {
    let claims = match expr {
        Expr::CtorReuse { reuse_of, .. }
        | Expr::ArrayPushReuse { reuse_of, .. }
        | Expr::ArrayPopReuse { reuse_of }
        | Expr::ArraySetReuse { reuse_of, .. }
        | Expr::ArrayRemoveReuse { reuse_of, .. }
        | Expr::StrConcatReuse { reuse_of, .. }
        | Expr::StrTrimReuse { reuse_of }
        | Expr::StrToUpperReuse { reuse_of }
        | Expr::StrToLowerReuse { reuse_of }
        | Expr::StrReplaceReuse { reuse_of, .. } => reuse_of == name,
        _ => false,
    };
    if claims {
        return true;
    }
    let mut found = false;
    for_each_child(expr, &mut |c| {
        if !found && reuses_name(c, name) {
            found = true;
        }
    });
    found
}

/// Undoes `drop_at_scope_end` for `name`, recognizing the exact shape it
/// produces and nothing else — if `expr` is not that shape it is returned
/// untouched, so a failure to match can only ever leave a leak, never
/// strip something load-bearing.
fn strip_scope_end_release(expr: Expr, name: &str) -> Expr {
    let tmp = format!("drop${name}");
    if let Expr::Let { name: outer, value, body } = &expr {
        if *outer == tmp {
            if let Expr::RcAnnotated { op: RcOp::Dec, target, rest } = body.as_ref() {
                if target == name {
                    if let Expr::Var(v) = rest.as_ref() {
                        if *v == tmp {
                            return (**value).clone();
                        }
                    }
                }
            }
        }
    }
    expr
}

/// Applies `f` to every direct child subexpression.
///
/// Exhaustive over `Expr` with NO `_` arm, deliberately. An earlier
/// version had a catch-all and silently skipped `StrConcat` (among
/// others), which made `incs_name` miss an increment nested inside one —
/// so a two-use string binding was mistaken for single-use, its
/// protective increment was dropped, and reuse-in-place then destroyed a
/// value still needed later. That surfaced as `let s = "ab"; let t =
/// s.concat("cd"); s.len() + t.len()` returning 8 instead of 6. A missing
/// arm here is a wrong-answer bug, so it must fail to compile instead.
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


/// Whether EVERY mention of `name` inside `expr` sits in a position that
/// only READS the value, never taking ownership of it.
///
/// The whitelist is deliberately narrow, and the default is OWNED: any
/// position not listed here makes this return `false`, which leaves the
/// binding on the pre-existing `mark_last_uses` path unchanged. So
/// widening this predicate can turn a leak into a release, but never
/// turns a working program into a double-free — the risk is entirely
/// one-directional, which is what makes it safe to grow one position at
/// a time with a measurement behind each.
///
/// What disqualifies a binding, and why each matters:
///
/// - Anything that STORES the value (a `Ctor`/`CtorReuse` field, an
///   array element, a `Let` alias) takes ownership of the reference.
/// - A `Call`/`ExternCall` argument, conservatively: this backend's
///   callees do not release their own parameters, but a callee is free
///   to store an argument into a cell that outlives the call, and
///   nothing at this call site can tell the two apart.
/// - A bare mention in the body's TAIL position — that is the function's
///   return value, and returning transfers ownership to the caller.
///   Not distinguished structurally here; it falls out of `Var` not
///   being whitelisted anywhere except inside the listed borrow slots.
/// - `AsCStr`, specifically: `transform` inserts a PROTECTIVE `Inc`
///   before one to close a real use-after-free, and a binding released
///   at scope end must not also be on that path.
/// - `reuse_of` on any of the `*Reuse` nodes. That name's cell is
///   consumed by the reuse itself (`CtorReuse` releases the old cell
///   before overwriting it), so releasing it again at scope end would be
///   a double free. This is the one case where getting it wrong is
///   memory-unsafe rather than merely leaky, so it is checked
///   explicitly rather than left to the OWNED default.
pub(crate) fn all_uses_are_borrows(expr: &Expr, name: &str) -> bool {
    match expr {
        // THE base case. Reaching a bare `Var(name)` here means the
        // parent did not intercept it as a borrow slot, so this use owns
        // the value: a return, an alias, a stored field, a call argument.
        Expr::Var(n) => n != name,

        // --- the borrow slots ---
        //
        // Each reads through the pointer and hands back something that
        // does not alias the cell's ownership. Verified against each
        // runtime definition, not assumed: `Match` inspects the tag and
        // copies fields out (incrementing any refcounted field it binds),
        // `ArrayLen` loads the length word, and every pure string
        // operation reads its operands' bytes and allocates a fresh cell
        // for the result.
        Expr::Match { scrutinee, arms } => {
            borrowed_slot(scrutinee, name)
                && arms.iter().all(|arm| {
                    // A rebinding shadows the outer name, so uses below it
                    // refer to something else entirely.
                    arm.bindings.iter().any(|b| b == name)
                        || (arm.guard.as_ref().map(|g| all_uses_are_borrows(g, name)).unwrap_or(true)
                            && all_uses_are_borrows(&arm.body, name))
                })
        }
        Expr::ArrayLen { array } => borrowed_slot(array, name),
        Expr::StrConcat { base, other } => borrowed_slot(base, name) && borrowed_slot(other, name),
        Expr::StrContains { base, needle } => borrowed_slot(base, name) && borrowed_slot(needle, name),
        Expr::StrStartsWith { base, prefix } => borrowed_slot(base, name) && borrowed_slot(prefix, name),
        Expr::StrEndsWith { base, suffix } => borrowed_slot(base, name) && borrowed_slot(suffix, name),
        Expr::StrSplit { base, sep } => borrowed_slot(base, name) && borrowed_slot(sep, name),
        Expr::StrReplace { base, from, to } => {
            borrowed_slot(base, name) && borrowed_slot(from, name) && borrowed_slot(to, name)
        }
        Expr::StrRunes { base }
        | Expr::StrTrim { base }
        | Expr::StrToUpper { base }
        | Expr::StrToLower { base }
        | Expr::ToString { base }
        | Expr::StrHash { base } => borrowed_slot(base, name),

        // --- consuming slots, which STILL recurse ---
        //
        // Recursing rather than short-circuiting to `false` is what makes
        // a borrow nested inside a consuming slot work: in
        // `println(n.to_string().len())` the call ARGUMENT is owned, but
        // `n` itself only appears inside `ToString`'s borrow slot. An
        // earlier version answered `false` for anything a `Call`
        // mentioned at all, which silently disabled the release for most
        // real code — `i.to_string().len()` included.
        //
        // A direct `Var(name)` in any of these lands on the base case
        // above and correctly reports owned.
        Expr::Ctor { fields, .. } => fields.iter().all(|f| all_uses_are_borrows(f, name)),
        Expr::Call { callee, args } => {
            all_uses_are_borrows(callee, name) && args.iter().all(|a| all_uses_are_borrows(a, name))
        }
        Expr::ExternCall { args, .. } => args.iter().all(|a| all_uses_are_borrows(a, name)),
        // `Index` is deliberately NOT a borrow slot, despite looking like
        // the most obvious one. `codegen_index` loads the element word and
        // hands it straight back with NO increment, so `a[0]` on an array
        // of heap values returns a pointer the array still owns.
        // Releasing `a` at scope end would leave that element dangling —
        // found as a segfault in the string/JSON tests, not by inspection.
        // Whether it is safe depends on the ELEMENT type, which this IR
        // does not carry.
        Expr::Index { base, index } => all_uses_are_borrows(base, name) && all_uses_are_borrows(index, name),
        // The array operations read their receiver and allocate a fresh
        // array, so the receiver looks like a borrow — but each has a
        // `*Reuse` counterpart that `mark_reuse` may rewrite it into,
        // which CONSUMES the receiver. Left owned rather than reasoning
        // about that interaction here.
        Expr::ArrayPop { array } => all_uses_are_borrows(array, name),
        Expr::ArrayPush { array, value } => {
            all_uses_are_borrows(array, name) && all_uses_are_borrows(value, name)
        }
        Expr::ArraySet { array, index, value } => {
            all_uses_are_borrows(array, name)
                && all_uses_are_borrows(index, name)
                && all_uses_are_borrows(value, name)
        }
        Expr::ArrayRemove { array, index } => {
            all_uses_are_borrows(array, name) && all_uses_are_borrows(index, name)
        }
        // `AsCStr` gets a PROTECTIVE `Inc` from `transform` to close a
        // real use-after-free, so a binding released at scope end must not
        // also be on that path.
        Expr::AsCStr(e) => all_uses_are_borrows(e, name),
        Expr::AsString(e) => all_uses_are_borrows(e, name),
        // Captured by a closure, or crossing into a spawned block: both
        // are handled entirely by codegen (it increments each heap-shaped
        // capture and the generated release function decrements it), but
        // left owned here rather than reasoning about that ownership
        // transfer — the cost is a missed release, never a double free.
        Expr::Closure { body, .. } => !expr_mentions_var(body, name),
        Expr::Spawn { block } => !expr_mentions_var(block, name),

        // Every `*Reuse` node CONSUMES the cell named by `reuse_of` — it
        // releases the old one before overwriting in place — so releasing
        // it again at scope end would be a double free. This is the one
        // case where getting it wrong is memory-unsafe rather than merely
        // leaky, so each is checked explicitly.
        Expr::CtorReuse { reuse_of, fields, .. } => {
            reuse_of != name && fields.iter().all(|f| all_uses_are_borrows(f, name))
        }
        Expr::ArrayPushReuse { reuse_of, value } => reuse_of != name && all_uses_are_borrows(value, name),
        Expr::ArrayPopReuse { reuse_of }
        | Expr::StrTrimReuse { reuse_of }
        | Expr::StrToUpperReuse { reuse_of }
        | Expr::StrToLowerReuse { reuse_of } => reuse_of != name,
        Expr::ArraySetReuse { reuse_of, index, value } => {
            reuse_of != name && all_uses_are_borrows(index, name) && all_uses_are_borrows(value, name)
        }
        Expr::ArrayRemoveReuse { reuse_of, index } => reuse_of != name && all_uses_are_borrows(index, name),
        Expr::StrConcatReuse { reuse_of, other } => reuse_of != name && all_uses_are_borrows(other, name),
        Expr::StrReplaceReuse { reuse_of, from, to } => {
            reuse_of != name && all_uses_are_borrows(from, name) && all_uses_are_borrows(to, name)
        }

        // `Ref` operations are owned by `refdrop`, which runs separately
        // and has its own borrow analysis; a `Ref` binding never reaches
        // this pass at all (see `is_syntactically_heap`).
        Expr::RefNew { value } => all_uses_are_borrows(value, name),
        Expr::RefGet { base } => all_uses_are_borrows(base, name),
        Expr::RefSet { base, value } => {
            all_uses_are_borrows(base, name) && all_uses_are_borrows(value, name)
        }

        // --- structural: cannot own anything themselves ---
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) => true,
        Expr::Channel { .. } | Expr::ArgsRaw | Expr::RandomRaw => true,
        Expr::Unary(_, e) | Expr::ToIntTrunc(e) | Expr::ToIntRound(e) | Expr::ToFloat(e) => {
            all_uses_are_borrows(e, name)
        }
        Expr::Binary(_, l, r) => all_uses_are_borrows(l, name) && all_uses_are_borrows(r, name),
        Expr::If { cond, then_branch, else_branch } => {
            all_uses_are_borrows(cond, name)
                && all_uses_are_borrows(then_branch, name)
                && all_uses_are_borrows(else_branch, name)
        }
        Expr::Let { name: n, value, body } => {
            all_uses_are_borrows(value, name) && (n == name || all_uses_are_borrows(body, name))
        }
        Expr::RcAnnotated { target, rest, .. } => target != name && all_uses_are_borrows(rest, name),
        Expr::Assign { name: n, value, rest } => {
            // Assigning THROUGH the name replaces the binding this
            // analysis is about, which its scope-end release cannot
            // account for.
            n != name && all_uses_are_borrows(value, name) && all_uses_are_borrows(rest, name)
        }
        Expr::For { var, start, end, body } => {
            all_uses_are_borrows(start, name)
                && all_uses_are_borrows(end, name)
                && (var == name || all_uses_are_borrows(body, name))
        }
        Expr::TaskJoin { task } => all_uses_are_borrows(task, name),
        Expr::ChannelSend { sender, value } => {
            all_uses_are_borrows(sender, name) && all_uses_are_borrows(value, name)
        }
        Expr::ChannelRecv { receiver } => all_uses_are_borrows(receiver, name),
        Expr::Select { arms } => arms
            .iter()
            .all(|a| all_uses_are_borrows(&a.receiver, name) && all_uses_are_borrows(&a.body, name)),
        Expr::ReadFileRaw { path } => all_uses_are_borrows(path, name),
        Expr::WriteFileRaw { path, contents } => {
            all_uses_are_borrows(path, name) && all_uses_are_borrows(contents, name)
        }
        Expr::EnvVarRaw { name: n } => all_uses_are_borrows(n, name),
        Expr::PanicRaw { message } => all_uses_are_borrows(message, name),
    }
}

/// A slot that BORROWS: a direct `Var(name)` here is fine. Anything else
/// is an ordinary subexpression, checked normally — the borrow applies to
/// the slot, not to whatever computes the value that lands in it.
fn borrowed_slot(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Var(n) if n == name) || all_uses_are_borrows(expr, name)
}

/// Wraps `body` so `name` is released AFTER `body` has produced its
/// value.
///
/// `RcAnnotated { op: Dec, .. }` decrements BEFORE its `rest`, and this
/// IR has no "decrement after this expression produces its value" node,
/// so the body's result is bound to a synthetic temporary first. `$`
/// cannot appear in a Plum identifier, so the name can never collide
/// with a user's own (the same guarantee `monomorphize`'s mangling
/// relies on); it is derived from `name` rather than a counter so this
/// stays a pure function of its input, which the bootstrap fixed point
/// depends on.
///
/// One consequence worth naming: `body` stops being in tail position, so
/// a call at the end of it is no longer a `musttail` candidate. That is
/// a correctness requirement rather than a lost optimization — a scope
/// with a value still to release cannot hand its frame away.
pub(crate) fn drop_at_scope_end(body: Expr, name: &str) -> Expr {
    let tmp = format!("drop${name}");
    Expr::Let {
        name: tmp.clone(),
        value: Box::new(body),
        body: Box::new(Expr::RcAnnotated {
            op: RcOp::Dec,
            target: name.to_string(),
            rest: Box::new(Expr::Var(tmp)),
        }),
    }
}

/// Which of each function's PARAMETERS may be targeted by
/// reuse-in-place.
///
/// # The problem this solves
///
/// `mark_reuse` only ever targets a name in `known_heap`, and a parameter
/// is never in it — so a tail-recursive accumulator held as a parameter
/// never gets reused, and every `acc.concat(x)` copies the whole
/// accumulator. Measured on the same 20,000-character result:
///
/// | accumulator held as... | allocations | bytes |
/// | --- | --- | --- |
/// | a PARAMETER (`go(acc.concat("x"), n - 1)`) | 40,005 | 200.7 MB |
/// | a LOCAL rebinding (`let mut` in a `for`) | 20,005 | 0.36 MB |
///
/// 557x, and 200.7 MB is exactly the sum of 1..20000 — O(n^2) copying. The
/// self-hosted compiler is written in tail-recursive accumulator style
/// throughout, which is where its 4.5 GB comes from.
///
/// # Why the obvious version is unsafe
///
/// Parameter reuse was tried once and reverted (DESIGN.md's "Gap 1"): two
/// simultaneous uses of the same unprotected parameter could each observe
/// refcount 1 and both destructively reuse the same cell —
/// `s.concat(rep(s, n - 1))`, a real segfault.
///
/// Reuse's only guard is a runtime `rc == 1` check, so it is safe exactly
/// when nothing else needs the cell at that moment. Two things could:
///
/// 1. **The callee itself, after the reuse site.** Ruled out by
///    `max_uses_on_path <= 1` — if the parameter is used once, there is no
///    second use to corrupt. `If`/`Match` branches count as ALTERNATIVES
///    rather than additively, which is what lets the accumulator shape
///    (one use in each branch) qualify while `s.concat(rep(s, ...))` (two
///    uses on one path) does not.
/// 2. **The caller, after the call.** For a tracked argument this is
///    already handled — `mark_last_uses` increments an argument the caller
///    still needs, so the callee observes `rc > 1`. For an UNTRACKED one
///    (itself a parameter, a call result, a match binding) no increment
///    exists, and that is precisely the reverted crash. Ruled out by
///    requiring every call site to pass a provably uniquely-owned value.
///
/// # What "uniquely owned" means here
///
/// Deliberately the narrowest useful answer: the argument is a
/// syntactically fresh allocation, or a `*Reuse` node (which yields a cell
/// its own analysis already proved unique). Both give the callee a cell
/// with refcount 1 that nothing else references. A bare `Var` does NOT
/// qualify, which is what rejects `rep(s, n - 1)` and
/// `{ let r = f(q); q.len() }` alike.
///
/// A function whose name is ever mentioned other than as a direct callee
/// (used as a value, passed to a higher-order function) has call sites this
/// cannot enumerate, so ALL its parameters are ineligible.
///
/// # Ordering
///
/// Must be computed BEFORE `anf`, which hoists a fresh-allocation argument
/// into a temporary and would leave a bare `Var` in its place. That is not
/// a soundness hole either way — an ANF temporary is a `Let`-bound fresh
/// allocation, hence tracked, single-use, and therefore safe by case 2
/// above — but computing it first is what keeps the accumulator shape
/// recognizable.
pub fn reusable_params(program: &Program) -> HashMap<String, HashSet<String>> {
    let declared: HashSet<&str> = program.functions.iter().map(|f| f.name.as_str()).collect();
    let mut escaped: HashSet<String> = HashSet::new();
    // Per function, per parameter index: does EVERY call site pass a
    // provably uniquely-owned value here? Starts optimistic and is
    // narrowed by each call site seen.
    let mut args_unique: HashMap<&str, Vec<bool>> = program
        .functions
        .iter()
        .map(|f| (f.name.as_str(), vec![true; f.params.len()]))
        .collect();

    for f in &program.functions {
        scan_calls(&f.body, &declared, &mut escaped, &mut args_unique);
    }
    for g in &program.globals {
        scan_calls(&g.value, &declared, &mut escaped, &mut args_unique);
    }

    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for f in &program.functions {
        if escaped.contains(&f.name) {
            continue;
        }
        let unique = &args_unique[f.name.as_str()];
        let mut eligible = HashSet::new();
        for (i, p) in f.params.iter().enumerate() {
            if unique.get(i).copied() == Some(true) && max_uses_on_path(&f.body, p) <= 1 {
                eligible.insert(p.clone());
            }
        }
        if !eligible.is_empty() {
            out.insert(f.name.clone(), eligible);
        }
    }
    out
}

/// Records, for every call to a declared function, whether each argument
/// is provably uniquely owned; and records any mention of a declared
/// function's name OUTSIDE a direct callee position as an escape.
fn scan_calls<'a>(
    expr: &'a Expr,
    declared: &HashSet<&'a str>,
    escaped: &mut HashSet<String>,
    args_unique: &mut HashMap<&'a str, Vec<bool>>,
) {
    if let Expr::Call { callee, args } = expr {
        if let Expr::Var(name) = callee.as_ref() {
            if let Some(name) = declared.get(name.as_str()) {
                let slots = args_unique.get_mut(*name).expect("declared implies present");
                if args.len() != slots.len() {
                    // A shape this cannot reason about (partial
                    // application, or an arity mismatch that should not
                    // happen) — decline every parameter rather than guess.
                    slots.iter_mut().for_each(|s| *s = false);
                } else {
                    for (i, a) in args.iter().enumerate() {
                        if !is_uniquely_owned(a) {
                            slots[i] = false;
                        }
                    }
                }
                // The callee `Var` is deliberately NOT walked: naming a
                // function in order to call it is not an escape.
                for a in args {
                    scan_calls(a, declared, escaped, args_unique);
                }
                return;
            }
        }
    }
    if let Expr::Var(name) = expr {
        if declared.contains(name.as_str()) {
            escaped.insert(name.clone());
        }
        return;
    }
    for_each_child(expr, &mut |c| scan_calls(c, declared, escaped, args_unique));
}

/// Whether `expr` evaluates to a cell nothing else holds a reference to.
///
/// A fresh allocation obviously qualifies. So does a `*Reuse` node: it
/// either overwrote a cell whose refcount was already 1, or allocated a
/// new one, and in both cases the result is uniquely owned.
fn is_uniquely_owned(expr: &Expr) -> bool {
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
            | Expr::CtorReuse { .. }
            | Expr::ArrayPushReuse { .. }
            | Expr::ArrayPopReuse { .. }
            | Expr::ArraySetReuse { .. }
            | Expr::ArrayRemoveReuse { .. }
            | Expr::StrConcatReuse { .. }
            | Expr::StrTrimReuse { .. }
            | Expr::StrToUpperReuse { .. }
            | Expr::StrToLowerReuse { .. }
            | Expr::StrReplaceReuse { .. }
    )
}

/// The greatest number of times `name` can be used on any single path
/// through `expr`.
///
/// `If`/`Match` branches are ALTERNATIVES — only one runs — so their
/// contribution is the maximum across branches, not the sum. That
/// distinction is the whole point: it is what separates the
/// tail-recursive accumulator (one use per branch) from
/// `s.concat(rep(s, n - 1))` (two uses on one path).
///
/// A loop body and a closure body both count as 2 regardless of what is
/// inside them: either can run more than once, so a single syntactic use
/// is not a single dynamic use. Saturates at 2 — nothing needs to
/// distinguish "twice" from "many".
fn max_uses_on_path(expr: &Expr, name: &str) -> usize {
    match expr {
        Expr::Var(n) => usize::from(n == name),
        // Branches are alternatives.
        Expr::If { cond, then_branch, else_branch } => (max_uses_on_path(cond, name)
            + max_uses_on_path(then_branch, name).max(max_uses_on_path(else_branch, name)))
        .min(2),
        Expr::Match { scrutinee, arms } => (max_uses_on_path(scrutinee, name)
            + arms
                .iter()
                .map(|a| {
                    if a.bindings.iter().any(|b| b == name) {
                        0
                    } else {
                        a.guard.as_ref().map(|g| max_uses_on_path(g, name)).unwrap_or(0)
                            + max_uses_on_path(&a.body, name)
                    }
                })
                .max()
                .unwrap_or(0))
        .min(2),
        // Shadowed: only the value is still the outer binding.
        Expr::Let { name: n, value, body } => {
            if n == name {
                max_uses_on_path(value, name)
            } else {
                (max_uses_on_path(value, name) + max_uses_on_path(body, name)).min(2)
            }
        }
        // Repeatable: one syntactic use is not one dynamic use.
        Expr::For { var, start, end, body } => {
            let head = max_uses_on_path(start, name) + max_uses_on_path(end, name);
            let inner = if var == name { 0 } else { max_uses_on_path(body, name) };
            (head + if inner > 0 { 2 } else { 0 }).min(2)
        }
        Expr::Closure { params, body, .. } => {
            if params.iter().any(|p| p == name) || max_uses_on_path(body, name) == 0 {
                0
            } else {
                2
            }
        }
        // `reuse_of` names a cell this expression CONSUMES, which is as
        // real a use as any.
        Expr::CtorReuse { reuse_of, fields, .. } => (usize::from(reuse_of == name)
            + fields.iter().map(|f| max_uses_on_path(f, name)).sum::<usize>())
        .min(2),
        Expr::ArrayPopReuse { reuse_of }
        | Expr::StrTrimReuse { reuse_of }
        | Expr::StrToUpperReuse { reuse_of }
        | Expr::StrToLowerReuse { reuse_of } => usize::from(reuse_of == name),
        Expr::ArrayPushReuse { reuse_of, value } => {
            (usize::from(reuse_of == name) + max_uses_on_path(value, name)).min(2)
        }
        Expr::ArraySetReuse { reuse_of, index, value } => (usize::from(reuse_of == name)
            + max_uses_on_path(index, name)
            + max_uses_on_path(value, name))
        .min(2),
        Expr::ArrayRemoveReuse { reuse_of, index } => {
            (usize::from(reuse_of == name) + max_uses_on_path(index, name)).min(2)
        }
        Expr::StrConcatReuse { reuse_of, other } => {
            (usize::from(reuse_of == name) + max_uses_on_path(other, name)).min(2)
        }
        Expr::StrReplaceReuse { reuse_of, from, to } => (usize::from(reuse_of == name)
            + max_uses_on_path(from, name)
            + max_uses_on_path(to, name))
        .min(2),
        // Everything else evaluates all of its children, so uses add.
        _ => {
            let mut total = 0usize;
            for_each_child(expr, &mut |c| total += max_uses_on_path(c, name));
            total.min(2)
        }
    }
}

/// Releases MATCH-EXTRACTED bindings whose every use is a borrow — the
/// third and last place a heap value could be owned without ever being
/// released.
///
/// # Why this is a separate pass
///
/// A match arm's binding is already an OWNED reference: codegen
/// increments every refcounted field as it extracts it
/// (`bind_arm_env`), transferring one reference from the scrutinee to the
/// binding. Nothing ever released it, so extracting a heap field leaked
/// it — measured per 1M iterations of a loop: 32.5 MB for a `String`
/// field, 63.1 MB for an `Array[Int]` field, 32.6 MB for a nested struct
/// field.
///
/// It is separate from `insert_refcount_ops` for one reason: this needs
/// to know which fields are refcounted, and the IR carries no types. The
/// judgement is made by `plum_codegen::is_refcounted` — the same function
/// codegen's own increment is gated on, handed in as `tag_heap` rather
/// than re-derived, because deriving it twice is exactly how an increment
/// and a release come to disagree.
///
/// Runs AFTER `optimize`, so reuse decisions and refcount annotations are
/// already in the tree and this only ever adds to them.
///
/// # The rule
///
/// Identical to `insert_refcount_ops`' own: exactly ONE use, and that use
/// in a borrow position. Single-use for the same sharp reason — a
/// multi-use binding's increments are what keep reuse-in-place honest —
/// and borrow-only so a binding that ESCAPES (returned from the arm,
/// stored into a `Ctor`) keeps the reference it was given. That escape
/// case is the whole point of the extraction increment, so breaking it
/// would be worse than the leak.
///
/// A catch-all arm binds the WHOLE scrutinee rather than a field, and
/// codegen does not increment it (`DEFAULT_ARM_TAG` in `bind_arm_env`) —
/// it is a pure borrow. Such an arm has no `tag_heap` entry, so it is
/// skipped, and the `is_empty` guard below makes that explicit rather
/// than incidental.
pub fn release_match_bindings(expr: Expr, tag_heap: &HashMap<String, Vec<bool>>) -> Expr {
    match expr {
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(release_match_bindings(*scrutinee, tag_heap)),
            arms: arms
                .into_iter()
                .map(|arm| {
                    let guard = arm.guard.map(|g| Box::new(release_match_bindings(*g, tag_heap)));
                    let mut body = release_match_bindings(arm.body, tag_heap);
                    let heap = tag_heap.get(&arm.tag).cloned().unwrap_or_default();
                    if !heap.is_empty() {
                        for (i, name) in arm.bindings.iter().enumerate() {
                            if heap.get(i).copied() != Some(true) {
                                continue;
                            }
                            // A use in the GUARD is a second use this
                            // analysis does not model, so skip rather
                            // than guess.
                            if guard.as_ref().map(|g| expr_mentions_var(g, name)).unwrap_or(false) {
                                continue;
                            }
                            // `mark_last_uses`' own answer for "exactly
                            // one use", reused rather than re-counted:
                            // `used` with no `Inc` inserted means the
                            // single use WAS the last use. The marked
                            // tree is discarded — the increments it would
                            // add are precisely what this avoids needing.
                            let (marked, used) = mark_last_uses(body.clone(), name, false);
                            if used && !incs_name(&marked, name) && all_uses_are_borrows(&body, name) {
                                body = drop_at_scope_end(body, name);
                            }
                        }
                    }
                    MatchArm {
                        tag: arm.tag,
                        bindings: arm.bindings,
                        guard,
                        body,
                    }
                })
                .collect(),
        },
        other => map_children_owned(other, &mut |c| release_match_bindings(c, tag_heap)),
    }
}

/// Rebuilds `expr` with `f` applied to every direct child — the owned
/// counterpart of `for_each_child`, used only by
/// `release_match_bindings`. Exhaustive with no `_` arm so a new variant
/// carrying a subexpression cannot silently have its match arms skipped.
fn map_children_owned(expr: Expr, f: &mut dyn FnMut(Expr) -> Expr) -> Expr {
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
        // Handled by `release_match_bindings` itself, never routed here.
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
        Expr::ArrayPop { array } => Expr::ArrayPop { array: b(array) },
        Expr::ArrayPush { array, value } => Expr::ArrayPush { array: b(array), value: b(value) },
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
        Expr::StrToUpper { base } => Expr::StrToUpper { base: b(base) },
        Expr::StrToLower { base } => Expr::StrToLower { base: b(base) },
        Expr::ToString { base } => Expr::ToString { base: b(base) },
        Expr::StrHash { base } => Expr::StrHash { base: b(base) },
        Expr::StrSplit { base, sep } => Expr::StrSplit { base: b(base), sep: b(sep) },
        Expr::StrContains { base, needle } => Expr::StrContains { base: b(base), needle: b(needle) },
        Expr::StrStartsWith { base, prefix } => Expr::StrStartsWith { base: b(base), prefix: b(prefix) },
        Expr::StrEndsWith { base, suffix } => Expr::StrEndsWith { base: b(base), suffix: b(suffix) },
        Expr::StrReplace { base, from, to } => Expr::StrReplace { base: b(base), from: b(from), to: b(to) },
        Expr::StrReplaceReuse { reuse_of, from, to } => Expr::StrReplaceReuse {
            reuse_of,
            from: b(from),
            to: b(to),
        },

        Expr::RefNew { value } => Expr::RefNew { value: b(value) },
        Expr::RefGet { base } => Expr::RefGet { base: b(base) },
        Expr::RefSet { base, value } => Expr::RefSet { base: b(base), value: b(value) },

        Expr::ReadFileRaw { path } => Expr::ReadFileRaw { path: b(path) },
        Expr::WriteFileRaw { path, contents } => Expr::WriteFileRaw { path: b(path), contents: b(contents) },
        Expr::EnvVarRaw { name } => Expr::EnvVarRaw { name: b(name) },
        Expr::PanicRaw { message } => Expr::PanicRaw { message: b(message) },
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
        Expr::Channel { .. } => false,
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
        Expr::StrHash { base } => expr_mentions_var(base, name),
    }
}

/// Scans a whole function body for its OWN declared parameters used in
/// an array/string-typed OPERAND position — `.push()`/`.pop()`/`.set()`
/// /`.remove()`/`.len()`/indexing/`.concat()`/`.trim()`/`.as_cstr()`/
/// etc only exist on arrays or strings syntactically, so finding a
/// parameter directly in one of those positions PROVES it's array/
/// string-shaped there, without needing the general type information
/// this IR doesn't carry (see `mark_reuse_scoped`'s own doc comment on
/// why an ARBITRARY parameter can't otherwise be assumed heap-shaped —
/// that reasoning still holds for every OTHER parameter; this only
/// narrows it for the specific ones this evidence actually proves).
///
/// **Currently UNUSED — kept deliberately, not dead weight to delete.**
/// This was meant to let `optimize_program` seed `known_heap` with
/// confirmed-shape parameters (closing a real perf gap — see DESIGN.md's
/// "Array push scaling bug" section), and the shadowing-awareness this
/// function implements (narrowing `params` at every `Let`/`Match`-arm/
/// `Closure`/`For` binding that could shadow one) is itself genuinely
/// correct and was hard-won (a real crash found and fixed the FIRST
/// version's naive, shadowing-unaware scan). But shape evidence alone
/// turned out to be insufficient PROOF of safety: a parameter can be
/// array-shaped and still be an unsafe-to-reuse ALIAS of something the
/// CALLER still needs (found via a second real crash — a closure's
/// captured environment, extracted via a `Match` arm binding this pass
/// has never tracked, handed into a function whose own parameter this
/// scan correctly-but-insufficiently proved array-shaped). `optimize_
/// program` was reverted rather than ship something merely believed
/// safe — see its own doc comment for the full story. A genuinely
/// correct future fix needs this function's shadowing-awareness PLUS a
/// real answer for match-extracted bindings' ownership, not this alone.
#[allow(dead_code)] // kept unused deliberately — see this function's own doc comment.
fn confirmed_array_or_str_params(body: &Expr, params: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_confirmed_params(body, params, &mut out);
    out
}

fn collect_confirmed_params(expr: &Expr, params: &[String], out: &mut HashSet<String>) {
    // Records `e` as confirmed if it's a bare reference to one of
    // `params` — used at every "this operand MUST be array/string-
    // shaped" site below, alongside the ordinary recursive walk that
    // still needs to visit every subexpression regardless (a param
    // could show up in more than one place, including nested deeper
    // inside a non-evidentiary position elsewhere).
    fn mark(e: &Expr, params: &[String], out: &mut HashSet<String>) {
        if let Expr::Var(n) = e {
            if params.iter().any(|p| p == n) {
                out.insert(n.clone());
            }
        }
    }
    // `params` MINUS one shadowed name, for descending into a scope a
    // `Let`/`Closure`/`For` binding shadows it in — load-bearing, not
    // cosmetic: without this, `let acc = <a plain Int>; ... f(|acc|
    // acc.push(x))` (an unrelated INNER `acc` — a closure param, say —
    // that happens to reuse an OUTER parameter's name) would wrongly
    // attribute the inner closure's own array-shaped `acc` as evidence
    // for the OUTER parameter, seeding a genuinely non-heap-shaped
    // parameter into `known_heap` — exactly the false-positive this
    // function's own doc comment promises never happens. Confirmed
    // real via an actual crash (a genuine parameter-name collision
    // inside `bootstrap/self_host/interp/interp.plum`, caught by its
    // own `exec_corpus/closures` fixture) before this fix, not
    // theoretical.
    fn without<'a>(params: &'a [String], shadowed: &str) -> std::borrow::Cow<'a, [String]> {
        if params.iter().any(|p| p == shadowed) {
            std::borrow::Cow::Owned(params.iter().filter(|p| p.as_str() != shadowed).cloned().collect())
        } else {
            std::borrow::Cow::Borrowed(params)
        }
    }
    fn without_many<'a>(params: &'a [String], shadowed: &[String]) -> std::borrow::Cow<'a, [String]> {
        if params.iter().any(|p| shadowed.contains(p)) {
            std::borrow::Cow::Owned(params.iter().filter(|p| !shadowed.contains(p)).cloned().collect())
        } else {
            std::borrow::Cow::Borrowed(params)
        }
    }
    match expr {
        Expr::Var(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) => {}
        Expr::Unary(_, e) => collect_confirmed_params(e, params, out),
        Expr::AsCStr(e) => {
            mark(e, params, out);
            collect_confirmed_params(e, params, out);
        }
        Expr::AsString(e) => collect_confirmed_params(e, params, out),
        Expr::ToIntTrunc(e) => collect_confirmed_params(e, params, out),
        Expr::ToIntRound(e) => collect_confirmed_params(e, params, out),
        Expr::ToFloat(e) => collect_confirmed_params(e, params, out),
        Expr::Binary(_, l, r) => {
            collect_confirmed_params(l, params, out);
            collect_confirmed_params(r, params, out);
        }
        // `value` is evaluated BEFORE the shadow takes effect (same
        // convention `mark_last_uses`'s own `Let` arm already uses), so
        // it still sees the OUTER name if `bound` happens to collide;
        // `body` only ever sees the NEW, inner one.
        Expr::Let { name: bound, value, body } => {
            collect_confirmed_params(value, params, out);
            collect_confirmed_params(body, &without(params, bound), out);
        }
        Expr::If { cond, then_branch, else_branch } => {
            collect_confirmed_params(cond, params, out);
            collect_confirmed_params(then_branch, params, out);
            collect_confirmed_params(else_branch, params, out);
        }
        Expr::Call { callee, args } => {
            collect_confirmed_params(callee, params, out);
            for a in args {
                collect_confirmed_params(a, params, out);
            }
        }
        Expr::ExternCall { args, .. } => {
            for a in args {
                collect_confirmed_params(a, params, out);
            }
        }
        Expr::Ctor { fields, .. } => {
            for f in fields {
                collect_confirmed_params(f, params, out);
            }
        }
        Expr::CtorReuse { fields, .. } => {
            for f in fields {
                collect_confirmed_params(f, params, out);
            }
        }
        Expr::RcAnnotated { rest, .. } => collect_confirmed_params(rest, params, out),
        Expr::Match { scrutinee, arms } => {
            collect_confirmed_params(scrutinee, params, out);
            for a in arms {
                let arm_params = without_many(params, &a.bindings);
                collect_confirmed_params(&a.body, &arm_params, out);
                if let Some(g) = a.guard.as_deref() {
                    collect_confirmed_params(g, &arm_params, out);
                }
            }
        }
        Expr::For { var, start, end, body } => {
            collect_confirmed_params(start, params, out);
            collect_confirmed_params(end, params, out);
            collect_confirmed_params(body, &without(params, var), out);
        }
        Expr::Closure { params: cparams, body, .. } => {
            collect_confirmed_params(body, &without_many(params, cparams), out);
        }
        Expr::Assign { value, rest, .. } => {
            collect_confirmed_params(value, params, out);
            collect_confirmed_params(rest, params, out);
        }
        Expr::Spawn { block } => collect_confirmed_params(block, params, out),
        Expr::TaskJoin { task } => collect_confirmed_params(task, params, out),
        Expr::Channel { .. } => {}
        Expr::ChannelSend { sender, value } => {
            collect_confirmed_params(sender, params, out);
            collect_confirmed_params(value, params, out);
        }
        Expr::ChannelRecv { receiver } => collect_confirmed_params(receiver, params, out),
        Expr::RefNew { value } => collect_confirmed_params(value, params, out),
        Expr::RefGet { base } => collect_confirmed_params(base, params, out),
        Expr::RefSet { base, value } => {
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(value, params, out);
        }
        Expr::ReadFileRaw { path } => collect_confirmed_params(path, params, out),
        Expr::WriteFileRaw { path, contents } => {
            collect_confirmed_params(path, params, out);
            collect_confirmed_params(contents, params, out);
        }
        Expr::EnvVarRaw { name } => collect_confirmed_params(name, params, out),
        Expr::ArgsRaw => {}
        Expr::RandomRaw => {}
        Expr::PanicRaw { message } => collect_confirmed_params(message, params, out),
        Expr::Select { arms } => {
            for arm in arms {
                collect_confirmed_params(&arm.receiver, params, out);
                collect_confirmed_params(&arm.body, params, out);
            }
        }
        Expr::Index { base, index } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(index, params, out);
        }
        Expr::ArrayLen { array } => {
            mark(array, params, out);
            collect_confirmed_params(array, params, out);
        }
        Expr::ArrayPush { array, value } => {
            mark(array, params, out);
            collect_confirmed_params(array, params, out);
            collect_confirmed_params(value, params, out);
        }
        Expr::ArrayPop { array } => {
            mark(array, params, out);
            collect_confirmed_params(array, params, out);
        }
        Expr::ArraySet { array, index, value } => {
            mark(array, params, out);
            collect_confirmed_params(array, params, out);
            collect_confirmed_params(index, params, out);
            collect_confirmed_params(value, params, out);
        }
        Expr::ArrayRemove { array, index } => {
            mark(array, params, out);
            collect_confirmed_params(array, params, out);
            collect_confirmed_params(index, params, out);
        }
        // Shouldn't normally appear as input here — produced BY this
        // pass, same "handled for robustness" precedent `transform`'s
        // own arms for these already document.
        Expr::ArrayPushReuse { value, .. } => collect_confirmed_params(value, params, out),
        Expr::ArrayPopReuse { .. } => {}
        Expr::ArraySetReuse { index, value, .. } => {
            collect_confirmed_params(index, params, out);
            collect_confirmed_params(value, params, out);
        }
        Expr::ArrayRemoveReuse { index, .. } => collect_confirmed_params(index, params, out),
        Expr::StrConcat { base, other } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(other, params, out);
        }
        Expr::StrConcatReuse { other, .. } => collect_confirmed_params(other, params, out),
        Expr::StrRunes { base } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
        }
        Expr::StrTrim { base } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
        }
        Expr::StrTrimReuse { .. } => {}
        Expr::StrSplit { base, sep } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(sep, params, out);
        }
        Expr::StrToUpper { base } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
        }
        Expr::StrToUpperReuse { .. } => {}
        Expr::StrToLower { base } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
        }
        Expr::StrToLowerReuse { .. } => {}
        Expr::StrContains { base, needle } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(needle, params, out);
        }
        Expr::StrStartsWith { base, prefix } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(prefix, params, out);
        }
        Expr::StrEndsWith { base, suffix } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(suffix, params, out);
        }
        Expr::StrReplace { base, from, to } => {
            mark(base, params, out);
            collect_confirmed_params(base, params, out);
            collect_confirmed_params(from, params, out);
            collect_confirmed_params(to, params, out);
        }
        Expr::StrReplaceReuse { from, to, .. } => {
            collect_confirmed_params(from, params, out);
            collect_confirmed_params(to, params, out);
        }
        // `.to_string()`/hashing apply to ANY type (Int/Bool/Float too),
        // not just heap-shaped ones — no evidence here, deliberately.
        Expr::ToString { base } => collect_confirmed_params(base, params, out),
        Expr::StrHash { base } => collect_confirmed_params(base, params, out),
    }
}

/// Does `expr` REBIND `name` via an `Assign` — i.e. is there a
/// self-update `name = <...>` somewhere in here? Used by `mark_last_
/// uses`'s `For` arm to decide whether the loop-carried liveness of
/// `name` is actually broken by a rebind each iteration (see that arm's
/// own doc comment for the full argument).
///
/// Shadowing-aware, deliberately: an `Assign` to an INNER binding that
/// merely reuses `name`'s spelling says nothing about the OUTER name,
/// and mistaking one for the other is exactly the class of error that
/// produced a real crash in an earlier attempt at parameter tracking.
/// Descends into `Let`/`For`/`Match`-arm scopes only where they don't
/// shadow `name`.
///
/// Deliberately does NOT count an `Assign` inside a `Closure` or
/// `Spawn` body: those can run zero times, many times, or on another
/// thread entirely, so a rebind in there is no guarantee the rebind
/// actually happens on this iteration. Returning `false` there is the
/// conservative direction — it just keeps the caller on its existing
/// forced-`live_after` path.
fn expr_assigns_var(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Assign { name: target, value, rest } => {
            target == name || expr_assigns_var(value, name) || expr_assigns_var(rest, name)
        }
        // `value` is evaluated before the shadow takes effect; `body`
        // only ever sees the inner binding.
        Expr::Let { name: bound, value, body } => {
            expr_assigns_var(value, name) || (bound != name && expr_assigns_var(body, name))
        }
        Expr::For { var, start, end, body } => {
            expr_assigns_var(start, name) || expr_assigns_var(end, name) || (var != name && expr_assigns_var(body, name))
        }
        Expr::Match { scrutinee, arms } => {
            expr_assigns_var(scrutinee, name)
                || arms.iter().any(|a| {
                    !a.bindings.iter().any(|b| b == name)
                        && (expr_assigns_var(&a.body, name)
                            || a.guard.as_deref().is_some_and(|g| expr_assigns_var(g, name)))
                })
        }
        // See this function's own doc comment — deliberately not counted.
        Expr::Closure { .. } | Expr::Spawn { .. } => false,
        Expr::Var(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Unit | Expr::EmptyArray(_) => false,
        Expr::Unary(_, e) => expr_assigns_var(e, name),
        Expr::AsCStr(e) | Expr::AsString(e) | Expr::ToIntTrunc(e) | Expr::ToIntRound(e) | Expr::ToFloat(e) => {
            expr_assigns_var(e, name)
        }
        Expr::Binary(_, l, r) => expr_assigns_var(l, name) || expr_assigns_var(r, name),
        Expr::If { cond, then_branch, else_branch } => {
            expr_assigns_var(cond, name) || expr_assigns_var(then_branch, name) || expr_assigns_var(else_branch, name)
        }
        Expr::Call { callee, args } => {
            expr_assigns_var(callee, name) || args.iter().any(|a| expr_assigns_var(a, name))
        }
        Expr::ExternCall { args, .. } => args.iter().any(|a| expr_assigns_var(a, name)),
        Expr::Ctor { fields, .. } | Expr::CtorReuse { fields, .. } => fields.iter().any(|f| expr_assigns_var(f, name)),
        Expr::RcAnnotated { rest, .. } => expr_assigns_var(rest, name),
        Expr::TaskJoin { task } => expr_assigns_var(task, name),
        Expr::Channel { .. } => false,
        Expr::ChannelSend { sender, value } => expr_assigns_var(sender, name) || expr_assigns_var(value, name),
        Expr::ChannelRecv { receiver } => expr_assigns_var(receiver, name),
        Expr::RefNew { value } => expr_assigns_var(value, name),
        Expr::RefGet { base } => expr_assigns_var(base, name),
        Expr::RefSet { base, value } => expr_assigns_var(base, name) || expr_assigns_var(value, name),
        Expr::ReadFileRaw { path } => expr_assigns_var(path, name),
        Expr::WriteFileRaw { path, contents } => expr_assigns_var(path, name) || expr_assigns_var(contents, name),
        Expr::EnvVarRaw { name: n } => expr_assigns_var(n, name),
        Expr::ArgsRaw | Expr::RandomRaw => false,
        Expr::PanicRaw { message } => expr_assigns_var(message, name),
        Expr::Select { arms } => arms
            .iter()
            .any(|arm| expr_assigns_var(&arm.receiver, name) || expr_assigns_var(&arm.body, name)),
        Expr::Index { base, index } => expr_assigns_var(base, name) || expr_assigns_var(index, name),
        Expr::ArrayLen { array } | Expr::ArrayPop { array } => expr_assigns_var(array, name),
        Expr::ArrayPush { array, value } => expr_assigns_var(array, name) || expr_assigns_var(value, name),
        Expr::ArraySet { array, index, value } => {
            expr_assigns_var(array, name) || expr_assigns_var(index, name) || expr_assigns_var(value, name)
        }
        Expr::ArrayRemove { array, index } => expr_assigns_var(array, name) || expr_assigns_var(index, name),
        Expr::ArrayPushReuse { value, .. } => expr_assigns_var(value, name),
        Expr::ArrayPopReuse { .. } => false,
        Expr::ArraySetReuse { index, value, .. } => expr_assigns_var(index, name) || expr_assigns_var(value, name),
        Expr::ArrayRemoveReuse { index, .. } => expr_assigns_var(index, name),
        Expr::StrConcat { base, other } => expr_assigns_var(base, name) || expr_assigns_var(other, name),
        Expr::StrConcatReuse { other, .. } => expr_assigns_var(other, name),
        Expr::StrRunes { base } | Expr::StrTrim { base } | Expr::StrToUpper { base } | Expr::StrToLower { base } => {
            expr_assigns_var(base, name)
        }
        Expr::StrTrimReuse { .. } | Expr::StrToUpperReuse { .. } | Expr::StrToLowerReuse { .. } => false,
        Expr::StrSplit { base, sep } => expr_assigns_var(base, name) || expr_assigns_var(sep, name),
        Expr::StrContains { base, needle } => expr_assigns_var(base, name) || expr_assigns_var(needle, name),
        Expr::StrStartsWith { base, prefix } => expr_assigns_var(base, name) || expr_assigns_var(prefix, name),
        Expr::StrEndsWith { base, suffix } => expr_assigns_var(base, name) || expr_assigns_var(suffix, name),
        Expr::StrReplace { base, from, to } => {
            expr_assigns_var(base, name) || expr_assigns_var(from, name) || expr_assigns_var(to, name)
        }
        Expr::StrReplaceReuse { from, to, .. } => expr_assigns_var(from, name) || expr_assigns_var(to, name),
        Expr::ToString { base } | Expr::StrHash { base } => expr_assigns_var(base, name),
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
            // reasoning doesn't apply to it in general. Marking a use of
            // an OUTER heap-tracked variable inside `body` as "the last
            // use" lets a CONSUMING operation there (`.push()`'s
            // reuse-in-place, say) destructively recycle a cell a LATER
            // iteration still needs. So the default stays conservative:
            // force `live_after = true` for the whole body, Inc'ing
            // (dup'ing) every use rather than treating any as a move —
            // leaking one reference per iteration instead of risking a
            // use-after-free, the safe direction to be wrong in.
            //
            // THE ONE EXCEPTION, and the reason this arm isn't just that
            // blanket rule: if the body REBINDS `name` itself (`acc =
            // acc.push(x)` — `expr_assigns_var`), the value the next
            // iteration reads is the NEW binding, not the old one. The
            // rebind is precisely what carries the loop-carried
            // dependency forward, so the OLD reference's liveness does
            // NOT cross the iteration boundary, and ordinary backward
            // analysis is sound. This matters enormously in practice:
            // `let mut acc = []; for .. { acc = acc.push(x) }` is the
            // idiomatic way to build a collection in this language AND
            // what `Array.map`/`Array.filter` themselves lower to (see
            // `lower.rs::lower_array_filter`), so under the blanket rule
            // every such accumulation re-Inc'd its accumulator each
            // iteration, forcing `.push()` onto its fresh-allocate-and-
            // copy path — quadratic behavior for the single most common
            // collection-building shape in the language. See DESIGN.md's
            // "Array push scaling bug" section.
            //
            // Safety of the exception, spelled out because this is the
            // delicate part: relaxing to the ORDINARY walk does not mean
            // assuming every use in the body is a last use. A use that
            // ISN'T superseded by the rebind still gets its Inc from the
            // ordinary backward analysis — e.g. `let snap = acc; acc =
            // acc.push(x); use(snap)` walks backward, sees `acc` used by
            // the (later) `Assign`, and therefore Inc's the (earlier)
            // read into `snap`, so the push observes refcount 2 and
            // correctly takes the fresh-allocate path instead of
            // corrupting `snap`. A CONDITIONAL rebind (`if c { acc =
            // acc.push(x) }`, exactly what `Array.filter` emits) is fine
            // too: the consume and the rebind live in the same branch,
            // so on the taken path `acc` is consumed and immediately
            // rebound, and on the untaken path it's neither.
            let (body_t, used_in_body) = if expr_mentions_var(&body, name) {
                if expr_assigns_var(&body, name) {
                    // `false`, NOT the incoming `live_after` — this is
                    // the whole point of the rebind argument, and
                    // getting it wrong silently defeats the rule. A use
                    // AFTER the loop (`let mut acc = []; for .. { acc =
                    // acc.push(x) }; acc.len()` — i.e. essentially every
                    // real accumulator, since you build a collection in
                    // order to use it) reads the value the LAST
                    // iteration's rebind produced, never the old
                    // reference the body consumed. Threading the outer
                    // `live_after` in here instead would mark that
                    // consumed read as still-live and Inc it, which is
                    // exactly the fresh-allocate-and-copy behavior this
                    // rule exists to remove — verified from generated
                    // LLVM, where the stray `@plum_rc_inc` survived an
                    // earlier version of this code that did just that.
                    let (t, used) = mark_last_uses(*body, name, false);
                    (t, used)
                } else {
                    let (t, _) = mark_last_uses(*body, name, true);
                    (t, true)
                }
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
        Expr::Channel { tag } => (Expr::Channel { tag: tag.clone() }, live_after),
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
        Expr::StrHash { base } => {
            let (base_t, used) = mark_last_uses(*base, name, live_after);
            (Expr::StrHash { base: Box::new(base_t) }, used)
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
            // When this assignment REBINDS the very name being analyzed
            // (`acc = acc.concat(x)`), every use in `rest` reads the NEW
            // value — so they must NOT keep the OLD one alive. Analyzing
            // `value` with `live_after = false` is what lets the read
            // inside it be a genuine last use, so a consuming operation
            // there can reuse in place. Same severing argument as the
            // `For` arm's rebind rule, applied to ordinary sequencing
            // rather than the loop-carried edge — and BOTH are needed:
            // `for .. { if c { acc = acc.concat(" ") }; acc = acc.concat
            // (s) }` (what a string join looks like, and what `Array.
            // map`/`filter` lower to) has two rebinds in one body, and
            // without this the FIRST one saw the SECOND one's read as
            // still-live, took the allocate-and-copy path, and made the
            // whole join quadratic — measured at 900MB for a 10,000-
            // element join before this, 4x per doubling.
            //
            // Aliasing is still handled by the ordinary walk, not
            // special-cased: `let snap = acc; acc = acc.concat(x); use
            // (snap)` still Inc's the read into `snap` (the `Assign`
            // reports the name as used, so the earlier binding sees
            // `live_after = true`), leaving refcount 2 so the concat's
            // runtime check takes the fresh-allocation path instead of
            // corrupting `snap`.
            let rebinds_analyzed_name = target.as_str() == name;
            let value_live_after = if rebinds_analyzed_name { false } else { used_rest || live_after };
            let (value_t, used_value) = mark_last_uses(*value, name, value_live_after);
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
    use crate::ir::{BinOp, PrimTy, RcOp};

    /// Does `expr` contain an `RcAnnotated { op: Inc, target: name, .. }`
    /// anywhere in its tree? Used by the loop-accumulator tests, which
    /// care that a protective dup exists (or provably doesn't) somewhere,
    /// not about its exact structural position — an `assert_eq!` against
    /// a fully-built expected tree would be far more brittle for the
    /// `Let`/`For`/`Assign`-nested bodies those tests build.
    fn expr_mentions_inc(expr: &Expr, name: &str) -> bool {
        match expr {
            Expr::RcAnnotated { op: RcOp::Inc, target, .. } if target == name => true,
            Expr::RcAnnotated { rest, .. } => expr_mentions_inc(rest, name),
            Expr::Let { value, body, .. } => expr_mentions_inc(value, name) || expr_mentions_inc(body, name),
            Expr::For { start, end, body, .. } => {
                expr_mentions_inc(start, name) || expr_mentions_inc(end, name) || expr_mentions_inc(body, name)
            }
            Expr::Assign { value, rest, .. } => expr_mentions_inc(value, name) || expr_mentions_inc(rest, name),
            Expr::If { cond, then_branch, else_branch } => {
                expr_mentions_inc(cond, name) || expr_mentions_inc(then_branch, name) || expr_mentions_inc(else_branch, name)
            }
            Expr::Call { callee, args } => expr_mentions_inc(callee, name) || args.iter().any(|a| expr_mentions_inc(a, name)),
            Expr::Binary(_, l, r) => expr_mentions_inc(l, name) || expr_mentions_inc(r, name),
            Expr::ArrayPush { array, value } => expr_mentions_inc(array, name) || expr_mentions_inc(value, name),
            Expr::ArrayPushReuse { value, .. } => expr_mentions_inc(value, name),
            Expr::Match { scrutinee, arms } => {
                expr_mentions_inc(scrutinee, name) || arms.iter().any(|a| expr_mentions_inc(&a.body, name))
            }
            _ => false,
        }
    }

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

    // --- parameter reuse eligibility (see `reusable_params`) ---

    fn func(name: &str, params: Vec<&str>, body: Expr) -> Function {
        Function {
            name: name.to_string(),
            params: params.into_iter().map(|p| p.to_string()).collect(),
            body,
        }
    }

    fn program_of(fns: Vec<Function>) -> Program {
        Program { functions: fns, globals: vec![], externs: vec![] }
    }

    fn str_lit(v: &str) -> Expr {
        Expr::Str(v.to_string())
    }

    fn concat(base: Expr, other: Expr) -> Expr {
        Expr::StrConcat { base: Box::new(base), other: Box::new(other) }
    }

    /// `go (acc) (n) = if n == 0 { acc } else { go(acc.concat("x"), n - 1) }`
    /// — the tail-recursive accumulator this whole analysis exists for.
    fn accumulator_program() -> Program {
        program_of(vec![
            func(
                "go",
                vec!["acc", "n"],
                if_(
                    var("n"),
                    var("acc"),
                    call(var("go"), vec![concat(var("acc"), str_lit("x")), var("n")]),
                ),
            ),
            func("main", vec![], call(var("go"), vec![str_lit(""), int(9)])),
        ])
    }

    #[test]
    fn a_tail_recursive_accumulator_parameter_is_reuse_eligible() {
        // One use per BRANCH, and every call site passes a freshly
        // allocated string. Measured 200.7 MB -> 0.36 MB on a
        // 20,000-character accumulation.
        let r = reusable_params(&accumulator_program());
        assert_eq!(r.get("go").map(|s| s.contains("acc")), Some(true), "got {r:?}");
    }

    #[test]
    fn the_accumulator_actually_gets_rewritten_to_a_reuse_node() {
        let p = accumulator_program();
        let reusable = reusable_params(&p);
        let out = optimize_program_with_reusable_params(p, &reusable);
        let go = out.functions.into_iter().find(|f| f.name == "go").unwrap();
        assert!(
            mentions_str_concat_reuse(&go.body, "acc"),
            "expected StrConcatReuse on `acc`: {:?}",
            go.body
        );
    }

    #[test]
    fn two_uses_on_one_path_make_a_parameter_ineligible() {
        // `rep (s) (n) = s.concat(rep(s, n - 1))` — THE shape whose reuse
        // was a real segfault (DESIGN.md's "Gap 1"): two simultaneous uses
        // of the same unprotected parameter could each observe refcount 1
        // and both destructively reuse the cell.
        let p = program_of(vec![func(
            "rep",
            vec!["s", "n"],
            concat(var("s"), call(var("rep"), vec![var("s"), var("n")])),
        )]);
        let r = reusable_params(&p);
        assert!(r.get("rep").map(|s| s.contains("s")) != Some(true), "got {r:?}");
    }

    #[test]
    fn a_bare_variable_argument_makes_the_parameter_ineligible() {
        // The caller may still need it and — being untracked itself — will
        // not have incremented, so the runtime `rc == 1` check would not
        // protect it. `{ let r = f(q); q.len() }` is the failing shape.
        let p = program_of(vec![
            func("f", vec!["p"], concat(var("p"), str_lit("x"))),
            func("caller", vec!["q"], call(var("f"), vec![var("q")])),
        ]);
        let r = reusable_params(&p);
        assert!(r.get("f").map(|s| s.contains("p")) != Some(true), "got {r:?}");
    }

    #[test]
    fn one_bad_call_site_among_several_is_enough_to_disqualify() {
        let p = program_of(vec![
            func("f", vec!["p"], concat(var("p"), str_lit("x"))),
            func("good", vec![], call(var("f"), vec![str_lit("fresh")])),
            func("bad", vec!["q"], call(var("f"), vec![var("q")])),
        ]);
        let r = reusable_params(&p);
        assert!(r.get("f").map(|s| s.contains("p")) != Some(true), "got {r:?}");
    }

    #[test]
    fn a_function_used_as_a_value_has_no_eligible_parameters() {
        // Its call sites cannot be enumerated, so nothing can be proven
        // about what its arguments alias.
        let p = program_of(vec![
            func("f", vec!["p"], concat(var("p"), str_lit("x"))),
            func("takes", vec!["g"], var("g")),
            func("caller", vec![], call(var("takes"), vec![var("f")])),
        ]);
        let r = reusable_params(&p);
        assert!(r.get("f").is_none(), "got {r:?}");
    }

    #[test]
    fn a_use_inside_a_loop_body_counts_as_more_than_one() {
        // One syntactic use is not one dynamic use: the loop can run the
        // reuse site repeatedly, and after the first iteration the cell is
        // gone.
        let p = program_of(vec![func(
            "f",
            vec!["p"],
            for_("i", int(0), int(10), concat(var("p"), str_lit("x"))),
        )]);
        let r = reusable_params(&p);
        assert!(r.get("f").map(|s| s.contains("p")) != Some(true), "got {r:?}");
    }

    #[test]
    fn a_use_inside_a_closure_body_counts_as_more_than_one() {
        // A closure can be called more than once.
        let p = program_of(vec![func(
            "f",
            vec!["p"],
            closure_(vec!["x"], concat(var("p"), str_lit("y"))),
        )]);
        let r = reusable_params(&p);
        assert!(r.get("f").map(|s| s.contains("p")) != Some(true), "got {r:?}");
    }

    #[test]
    fn branches_count_as_alternatives_not_additively() {
        // The distinction the whole rule turns on: one use in each arm of
        // an `If` is still one use on any single path.
        let body = if_(var("n"), concat(var("p"), str_lit("a")), concat(var("p"), str_lit("b")));
        assert_eq!(max_uses_on_path(&body, "p"), 1);
        let sequential = Expr::Binary(
            BinOp::Add,
            Box::new(concat(var("p"), str_lit("a"))),
            Box::new(concat(var("p"), str_lit("b"))),
        );
        assert_eq!(max_uses_on_path(&sequential, "p"), 2);
    }

    #[test]
    fn a_reuse_of_name_counts_as_a_use() {
        // A `*Reuse` node CONSUMES the cell it names, which is as real a
        // use as reading it.
        let body = Expr::Binary(
            BinOp::Add,
            Box::new(Expr::StrConcatReuse { reuse_of: "p".into(), other: Box::new(str_lit("a")) }),
            Box::new(Expr::ArrayLen { array: Box::new(var("p")) }),
        );
        assert_eq!(max_uses_on_path(&body, "p"), 2);
    }

    fn mentions_str_concat_reuse(expr: &Expr, name: &str) -> bool {
        if let Expr::StrConcatReuse { reuse_of, .. } = expr {
            if reuse_of == name {
                return true;
            }
        }
        let mut found = false;
        super::for_each_child(expr, &mut |c| {
            if !found && mentions_str_concat_reuse(c, name) {
                found = true;
            }
        });
        found
    }

    // --- release for match-extracted bindings ---

    fn heap_tags(pairs: &[(&str, Vec<bool>)]) -> std::collections::HashMap<String, Vec<bool>> {
        pairs.iter().map(|(t, v)| (t.to_string(), v.clone())).collect()
    }

    #[test]
    fn a_heap_shaped_match_binding_is_released_at_the_end_of_its_arm() {
        // Codegen already increments a refcounted field as it extracts it,
        // transferring one reference from the scrutinee to the binding.
        // Nothing released it — 32.5MB per 1M iterations for a `String`
        // field.
        let e = match_(
            var("s"),
            vec![arm("Named", vec!["nm"], Expr::ArrayLen { array: Box::new(var("nm")) })],
        );
        let out = release_match_bindings(e, &heap_tags(&[("Named", vec![true])]));
        let Expr::Match { arms, .. } = &out else { panic!("expected a Match") };
        assert!(expr_mentions_dec(&arms[0].body, "nm"), "got {:?}", arms[0].body);
    }

    #[test]
    fn a_scalar_match_binding_is_left_alone() {
        // An `Int` field carries no refcount, so `RcAnnotated` on it is a
        // hard codegen error rather than a no-op.
        let e = match_(var("s"), vec![arm("Named", vec!["k"], var("k"))]);
        let out = release_match_bindings(e, &heap_tags(&[("Named", vec![false])]));
        let Expr::Match { arms, .. } = &out else { panic!("expected a Match") };
        assert!(!expr_mentions_dec(&arms[0].body, "k"));
    }

    #[test]
    fn an_escaping_match_binding_is_not_released() {
        // The arm's own result IS the binding, so releasing it would hand
        // the caller a freed cell. Keeping the extraction increment for
        // exactly this case is the whole reason it exists.
        let e = match_(var("s"), vec![arm("Named", vec!["nm"], var("nm"))]);
        let out = release_match_bindings(e, &heap_tags(&[("Named", vec![true])]));
        let Expr::Match { arms, .. } = &out else { panic!("expected a Match") };
        assert!(!expr_mentions_dec(&arms[0].body, "nm"));
    }

    #[test]
    fn a_match_binding_stored_into_a_ctor_is_not_released() {
        let e = match_(
            var("s"),
            vec![arm("Named", vec!["nm"], ctor("Wrapper", vec![var("nm")]))],
        );
        let out = release_match_bindings(e, &heap_tags(&[("Named", vec![true])]));
        let Expr::Match { arms, .. } = &out else { panic!("expected a Match") };
        assert!(!expr_mentions_dec(&arms[0].body, "nm"));
    }

    #[test]
    fn a_match_binding_used_twice_is_not_released() {
        // Same reason as `Let` bindings: the increments a multi-use
        // binding needs are what keep reuse-in-place honest, and this pass
        // deliberately adds none.
        let e = match_(
            var("s"),
            vec![arm(
                "Named",
                vec!["nm"],
                Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::ArrayLen { array: Box::new(var("nm")) }),
                    Box::new(Expr::ArrayLen { array: Box::new(var("nm")) }),
                ),
            )],
        );
        let out = release_match_bindings(e, &heap_tags(&[("Named", vec![true])]));
        let Expr::Match { arms, .. } = &out else { panic!("expected a Match") };
        assert!(!expr_mentions_dec(&arms[0].body, "nm"));
    }

    #[test]
    fn a_catch_all_arm_binding_is_never_released() {
        // A catch-all binds the WHOLE scrutinee, not a field, and codegen
        // does not increment it — it is a pure borrow. Releasing it would
        // free the scrutinee from under its owner. Such an arm has no
        // `tag_heap` entry at all, which is what makes this safe.
        let e = match_(
            var("s"),
            vec![arm("0Default", vec!["whole"], Expr::ArrayLen { array: Box::new(var("whole")) })],
        );
        let out = release_match_bindings(e.clone(), &heap_tags(&[("Named", vec![true])]));
        assert_eq!(out, e);
    }

    #[test]
    fn a_binding_used_in_the_arms_guard_is_not_released() {
        // A guard use is a second use this analysis does not model.
        let mut a = arm("Named", vec!["nm"], Expr::ArrayLen { array: Box::new(var("nm")) });
        a.guard = Some(Box::new(Expr::ArrayLen { array: Box::new(var("nm")) }));
        let out = release_match_bindings(match_(var("s"), vec![a]), &heap_tags(&[("Named", vec![true])]));
        let Expr::Match { arms, .. } = &out else { panic!("expected a Match") };
        assert!(!expr_mentions_dec(&arms[0].body, "nm"));
    }

    #[test]
    fn a_match_nested_inside_another_expression_is_still_visited() {
        // The pass has to reach every `Match` in the tree, not just one at
        // the root.
        let inner = match_(
            var("s"),
            vec![arm("Named", vec!["nm"], Expr::ArrayLen { array: Box::new(var("nm")) })],
        );
        let e = let_("x", int(1), Expr::Binary(BinOp::Add, Box::new(var("x")), Box::new(inner)));
        let out = release_match_bindings(e, &heap_tags(&[("Named", vec![true])]));
        assert!(expr_mentions_dec(&out, "nm"), "got {out:?}");
    }

    // --- scope-end release for borrow-only bindings ---
    //
    // The gap these close: before them, `RcOp::Dec` was only ever emitted
    // for a binding NOTHING referenced, so every heap value a program
    // actually used leaked. Measured linear at 13.7/47.9/185.1 MB for
    // 250k/1M/4M iterations of `let p = Point { .. }; match p { .. }`.

    /// `drop$<name>`, the synthetic temporary `drop_at_scope_end` binds
    /// the body's result to so the release can happen AFTER it.
    fn drop_tmp(name: &str) -> String {
        format!("drop${name}")
    }

    #[test]
    fn a_match_scrutinee_is_a_borrow_so_its_binding_is_released_at_scope_end() {
        // `let p = Point(..); match p { Point(x) => x }` — the match reads
        // the tag and copies fields out; it does not consume the cell.
        let input = let_(
            "p",
            ctor("Point", vec![int(1)]),
            match_(var("p"), vec![arm("Point", vec!["x"], var("x"))]),
        );
        let out = insert_refcount_ops(input);
        let Expr::Let { name, body, .. } = &out else {
            panic!("expected a Let, got {out:?}");
        };
        assert_eq!(name, "p");
        let Expr::Let { name: tmp, body: inner, .. } = body.as_ref() else {
            panic!("expected the scope-end temporary, got {body:?}");
        };
        assert_eq!(*tmp, drop_tmp("p"));
        assert_eq!(
            **inner,
            dec("p", var(&drop_tmp("p"))),
            "the release must run AFTER the body has produced its value"
        );
    }

    #[test]
    fn array_len_is_a_borrow_slot() {
        let input = let_("a", ctor("Arr", vec![]), Expr::ArrayLen { array: Box::new(var("a")) });
        assert!(
            expr_mentions_dec(&insert_refcount_ops(input), "a"),
            "`.len()` only loads the length word, so `a` should be released"
        );
    }

    #[test]
    fn indexing_is_not_a_borrow_slot() {
        // Deliberately excluded: `codegen_index` hands back the element
        // word with NO increment, so `a[0]` on an array of heap values
        // returns a pointer the array still owns. Releasing `a` would
        // dangle it — this was a real segfault, not a hypothetical.
        let input = let_(
            "a",
            ctor("Arr", vec![]),
            Expr::Index { base: Box::new(var("a")), index: Box::new(int(0)) },
        );
        assert!(!expr_mentions_dec(&insert_refcount_ops(input), "a"));
    }

    #[test]
    fn a_binding_used_twice_is_left_on_the_old_path_with_its_protective_inc() {
        // Two uses means `mark_last_uses` inserts an `Inc`, and that
        // increment is the ONLY thing stopping reuse-in-place from
        // destroying a value still needed later — reuse has no static
        // check, just a runtime `rc == 1` test. So multi-use bindings keep
        // the pre-existing behaviour exactly.
        // Two uses on ONE path (a `Binary`), not two branches — both are
        // genuinely evaluated, so the first is not a last use.
        let input = let_(
            "p",
            ctor("Point", vec![int(1)]),
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::ArrayLen { array: Box::new(var("p")) }),
                Box::new(Expr::ArrayLen { array: Box::new(var("p")) }),
            ),
        );
        let out = insert_refcount_ops(input);
        // `expr_mentions_inc_deep`, not this module's older
        // `expr_mentions_inc` — that one does not descend into
        // `ArrayLen`, so it would report "no Inc" for exactly this shape.
        assert!(expr_mentions_inc_deep(&out, "p"), "the protective Inc must survive");
        assert!(!expr_mentions_dec(&out, "p"), "and no scope-end release is added");
    }

    #[test]
    fn a_binding_with_a_consuming_use_is_left_unchanged() {
        // Storing the value in a `Ctor` transfers ownership, so the
        // pre-existing "last use consumes" handling applies.
        let input = let_("p", ctor("Point", vec![int(1)]), ctor("Wrapper", vec![var("p")]));
        assert!(!expr_mentions_dec(&insert_refcount_ops(input), "p"));
    }

    #[test]
    fn reuse_in_place_wins_over_the_scope_end_release() {
        // Both claim the same reference, so exactly one may happen. Reuse
        // dominates — it avoids the allocation as well as releasing the
        // cell — and doing both would be a double free. `optimize` runs
        // reuse second, which retracts the optimistically-inserted
        // release.
        let input = let_(
            "p",
            ctor("Point", vec![int(1)]),
            match_(var("p"), vec![arm("Point", vec!["x"], ctor("Point", vec![var("x")]))]),
        );
        let released = insert_refcount_ops(input.clone());
        assert!(expr_mentions_dec(&released, "p"), "release inserted optimistically");

        let optimized = optimize(input);
        assert!(
            !expr_mentions_dec(&optimized, "p"),
            "and retracted once reuse claimed the cell: {optimized:?}"
        );
        assert!(expr_mentions_ctor_reuse(&optimized, "p"), "reuse should still fire: {optimized:?}");
    }

    #[test]
    fn a_string_producing_operation_is_releasable_even_though_it_is_not_a_ctor() {
        // `is_syntactically_heap` only recognizes `Ctor`/`Str` literal/
        // `EmptyArray`, so `let s = a.concat(b)` was never tracked at all
        // — 139MB per 1M iterations. `allocates_fresh_heap` covers it, at
        // this one site only.
        let input = let_(
            "s",
            Expr::StrConcat { base: Box::new(Expr::Str("a".into())), other: Box::new(Expr::Str("b".into())) },
            Expr::ArrayLen { array: Box::new(var("s")) },
        );
        assert!(expr_mentions_dec(&insert_refcount_ops(input), "s"));
    }

    #[test]
    fn an_unused_binding_still_gets_the_better_immediate_release() {
        // Zero uses trivially satisfies "all uses are borrows", but
        // releasing immediately beats releasing at scope end, so the
        // pre-existing path must still win.
        let input = let_("p", ctor("Point", vec![]), int(5));
        assert_eq!(insert_refcount_ops(input), let_("p", ctor("Point", vec![]), dec("p", int(5))));
    }

    /// A COMPLETE walk, unlike this module's older `expr_mentions_inc`
    /// (which has its own hand-rolled, partial recursion and misses
    /// annotations nested inside e.g. `ArrayLen`). Built on
    /// `super::for_each_child`, which is exhaustive by construction.
    fn expr_mentions_inc_deep(expr: &Expr, name: &str) -> bool {
        if let Expr::RcAnnotated { op: RcOp::Inc, target, .. } = expr {
            if target == name {
                return true;
            }
        }
        let mut found = false;
        super::for_each_child(expr, &mut |c| {
            if !found && expr_mentions_inc_deep(c, name) {
                found = true;
            }
        });
        found
    }

    fn expr_mentions_dec(expr: &Expr, name: &str) -> bool {
        if let Expr::RcAnnotated { op: RcOp::Dec, target, .. } = expr {
            if target == name {
                return true;
            }
        }
        let mut found = false;
        super::for_each_child(expr, &mut |c| {
            if !found && expr_mentions_dec(c, name) {
                found = true;
            }
        });
        found
    }

    fn expr_mentions_ctor_reuse(expr: &Expr, name: &str) -> bool {
        if let Expr::CtorReuse { reuse_of, .. } = expr {
            if reuse_of == name {
                return true;
            }
        }
        let mut found = false;
        super::for_each_child(expr, &mut |c| {
            if !found && expr_mentions_ctor_reuse(c, name) {
                found = true;
            }
        });
        found
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

    // --- Loop-carried accumulators: `let mut acc = []; for .. { acc =
    // acc.push(x) }`, the idiomatic collection-building shape in this
    // language and what `Array.map`/`Array.filter` themselves lower to.
    // See `mark_last_uses`'s `For` arm for the full safety argument. ---

    #[test]
    fn a_self_rebinding_accumulator_in_a_loop_body_is_a_last_use_not_a_dup() {
        // `acc = acc.push(1)` inside the loop: the rebind carries the
        // loop-carried dependency, so the OLD reference genuinely dies
        // here and the push may consume (and later reuse) it. Under the
        // old blanket-force rule this got an Inc every iteration,
        // pinning refcount >= 2 and forcing `.push()` onto its
        // fresh-allocate-and-copy path — quadratic accumulation.
        let input = let_(
            "acc",
            Expr::EmptyArray(PrimTy::Int),
            for_(
                "i",
                int(0),
                int(5),
                assign_(
                    "acc",
                    Expr::ArrayPush { array: Box::new(var("acc")), value: Box::new(int(1)) },
                    Expr::Unit,
                ),
            ),
        );
        let out = insert_refcount_ops(input);
        assert!(
            !expr_mentions_inc(&out, "acc"),
            "the self-rebinding accumulator must not be dup'd every iteration: {out:?}"
        );
    }

    #[test]
    fn a_self_rebinding_accumulator_is_a_last_use_even_when_used_after_the_loop() {
        // The REALISTIC shape, and the one an earlier version of this
        // rule silently failed: `let mut acc = []; for .. { acc =
        // acc.push(x) }; <use acc>`. You always build a collection in
        // order to use it afterward, so `live_after` at the `For` node
        // is essentially always true — and threading it into the body
        // re-Inc's the consumed read, defeating the whole rule. The
        // earlier version passed the weaker test below (which had
        // nothing after the loop, so `live_after` was already false)
        // while the real compiler still emitted a per-iteration
        // `@plum_rc_inc` + fresh allocation + memcpy. Caught by reading
        // generated LLVM, not by this suite — hence this test.
        let input = let_(
            "acc",
            Expr::EmptyArray(PrimTy::Int),
            let_(
                "_",
                for_(
                    "i",
                    int(0),
                    int(5),
                    assign_(
                        "acc",
                        Expr::ArrayPush { array: Box::new(var("acc")), value: Box::new(int(1)) },
                        Expr::Unit,
                    ),
                ),
                Expr::ArrayLen { array: Box::new(var("acc")) },
            ),
        );
        let out = optimize(input);
        assert!(
            !expr_mentions_inc(&out, "acc"),
            "a post-loop use must not resurrect the consumed in-loop read: {out:?}"
        );
        assert!(
            format!("{out:?}").contains("ArrayPushReuse"),
            "the accumulator should still reach the reuse-in-place push path: {out:?}"
        );
    }

    #[test]
    fn two_rebinds_in_one_loop_body_are_both_last_uses() {
        // The string-join shape: `for .. { if c { acc = acc.concat(" ") };
        // acc = acc.concat(s) }`. Before the `Assign` rebind rule, the
        // FIRST rebind saw the SECOND one's read as still-live and got an
        // Inc, so it took the allocate-and-copy path and the whole join
        // was quadratic (900MB for a 10,000-element join, 4x per
        // doubling). Both rebinds must come out as last uses.
        let input = let_(
            "acc",
            Expr::Str("".to_string()),
            let_(
                "_",
                for_(
                    "i",
                    int(0),
                    int(5),
                    Expr::If {
                        cond: Box::new(Expr::Bool(true)),
                        then_branch: Box::new(assign_(
                            "acc",
                            Expr::StrConcat { base: Box::new(var("acc")), other: Box::new(Expr::Str(" ".to_string())) },
                            Expr::Unit,
                        )),
                        else_branch: Box::new(assign_(
                            "acc",
                            Expr::StrConcat { base: Box::new(var("acc")), other: Box::new(Expr::Str("x".to_string())) },
                            Expr::Unit,
                        )),
                    },
                ),
                var("acc"),
            ),
        );
        let out = optimize(input);
        assert!(!expr_mentions_inc(&out, "acc"), "neither rebind should be dup'd: {out:?}");
        assert!(
            format!("{out:?}").matches("StrConcatReuse").count() == 2,
            "both concats should reuse in place: {out:?}"
        );
    }

    #[test]
    fn an_alias_read_before_a_rebinding_assign_is_still_duped() {
        // The safety counterpart: `snap` aliases `acc` before the rebind
        // and outlives it, so the read into `snap` must still be Inc'd —
        // leaving refcount 2 so the concat's runtime check takes the
        // fresh-allocation path instead of corrupting `snap`.
        let input = let_(
            "acc",
            Expr::Str("".to_string()),
            let_(
                "snap",
                var("acc"),
                assign_(
                    "acc",
                    Expr::StrConcat { base: Box::new(var("acc")), other: Box::new(Expr::Str("x".to_string())) },
                    call(var("use_it"), vec![var("snap")]),
                ),
            ),
        );
        let out = insert_refcount_ops(input);
        assert!(
            expr_mentions_inc(&out, "acc"),
            "an alias read before a rebinding assign must still be dup'd: {out:?}"
        );
    }

    #[test]
    fn a_self_rebinding_accumulator_still_reaches_array_push_reuse() {
        // The end-to-end point of the above: with no forced Inc in the
        // way, `mark_reuse` can turn the push into `ArrayPushReuse`.
        let input = let_(
            "acc",
            Expr::EmptyArray(PrimTy::Int),
            for_(
                "i",
                int(0),
                int(5),
                assign_(
                    "acc",
                    Expr::ArrayPush { array: Box::new(var("acc")), value: Box::new(int(1)) },
                    Expr::Unit,
                ),
            ),
        );
        let out = optimize(input);
        assert!(
            format!("{out:?}").contains("ArrayPushReuse"),
            "a loop accumulator should reach the reuse-in-place push path: {out:?}"
        );
    }

    #[test]
    fn a_loop_body_use_that_is_not_a_self_rebind_is_still_conservatively_duped() {
        // No `Assign` to `xs` anywhere in the body — nothing breaks the
        // loop-carried liveness, so the old conservative rule must still
        // apply, or a consuming op here could recycle a cell the next
        // iteration still needs.
        let input = let_(
            "xs",
            ctor("Pair", vec![int(1), int(2)]),
            for_("i", int(0), int(5), call(var("consume"), vec![var("xs")])),
        );
        let out = insert_refcount_ops(input);
        assert!(
            expr_mentions_inc(&out, "xs"),
            "a non-rebinding loop-body use must still be dup'd: {out:?}"
        );
    }

    #[test]
    fn an_aliasing_read_before_a_self_rebind_is_still_duped() {
        // THE safety test for the exception. `snap` aliases `acc`
        // BEFORE the rebind and is used AFTER it. The backward walk must
        // see that `acc` is still needed (by the later `Assign`) at the
        // point `snap` reads it, and Inc there — so the push observes
        // refcount 2 and takes the fresh-allocate path instead of
        // destructively recycling the buffer `snap` still points at.
        // Without this, the exception would be a silent corruption bug.
        let input = let_(
            "acc",
            Expr::EmptyArray(PrimTy::Int),
            for_(
                "i",
                int(0),
                int(5),
                let_(
                    "snap",
                    var("acc"),
                    assign_(
                        "acc",
                        Expr::ArrayPush { array: Box::new(var("acc")), value: Box::new(int(1)) },
                        call(var("use_it"), vec![var("snap")]),
                    ),
                ),
            ),
        );
        let out = insert_refcount_ops(input);
        assert!(
            expr_mentions_inc(&out, "acc"),
            "an aliasing read before the rebind must be dup'd so the push can't corrupt the alias: {out:?}"
        );
    }

    #[test]
    fn a_shadowed_inner_rebind_does_not_relax_the_outer_names_loop_rule() {
        // The `Assign` targets an INNER `acc` introduced by a `Let`
        // inside the body — it says nothing about the outer `acc`, whose
        // loop-carried liveness is therefore NOT broken. The outer name
        // must stay on the conservative dup'ing path.
        let input = let_(
            "acc",
            ctor("Pair", vec![int(1), int(2)]),
            for_(
                "i",
                int(0),
                int(5),
                let_(
                    "acc", // shadows
                    Expr::EmptyArray(PrimTy::Int),
                    assign_("acc", Expr::EmptyArray(PrimTy::Int), Expr::Unit),
                ),
            ),
        );
        // The OUTER acc is never actually read in the body here, so the
        // meaningful assertion is on `expr_assigns_var` itself: it must
        // not claim the outer name is rebound.
        let Expr::Let { body, .. } = &input else { panic!("shape") };
        let Expr::For { body: loop_body, .. } = body.as_ref() else { panic!("shape") };
        assert!(
            !expr_assigns_var(loop_body, "acc"),
            "a shadowed inner rebind must not count as rebinding the outer name"
        );
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

    // --- `confirmed_array_or_str_params` — currently UNUSED by
    // `optimize_program` (see that function's own doc comment for why
    // the attempted fix built on top of this was reverted), but its own
    // shadowing-awareness is genuinely correct and tested directly
    // here, kept for a future, more careful attempt. ---

    #[test]
    fn confirmed_array_or_str_params_finds_a_parameter_proven_by_its_own_push_call() {
        let body = Expr::ArrayPush {
            array: Box::new(var("acc")),
            value: Box::new(var("v")),
        };
        let found = confirmed_array_or_str_params(&body, &["acc".to_string(), "v".to_string()]);
        assert_eq!(found, known(&["acc"]));
    }

    #[test]
    fn confirmed_array_or_str_params_does_not_attribute_a_shadowed_inner_bindings_use_to_the_outer_param() {
        // `acc` is an OUTER parameter (plainly scalar, per its own use
        // below), but a NESTED closure parameter reuses the SAME name
        // and genuinely IS array-shaped inside that closure's own body.
        // A shadowing-UNAWARE scan would see `acc.push(...)` textually
        // anywhere in the body and wrongly attribute it to the OUTER
        // `acc` — exactly the real bug (a segfault treating an Int as a
        // heap pointer) found via `exec_corpus/closures` when this was
        // first tried without shadowing-awareness.
        let body = let_(
            "make_pusher",
            Expr::Closure {
                params: vec!["acc".to_string()], // shadows the OUTER `acc`
                param_types: None,
                ret_type: None,
                body: Box::new(Expr::ArrayPush {
                    array: Box::new(var("acc")), // the INNER (shadowed) acc
                    value: Box::new(int(1)),
                }),
            },
            Expr::Binary(BinOp::Add, Box::new(var("acc")), Box::new(int(1))), // the OUTER acc, used as a plain Int
        );
        let found = confirmed_array_or_str_params(&body, &["acc".to_string()]);
        assert!(found.is_empty(), "the outer Int parameter must not be attributed the shadowed inner binding's array use: {found:?}");
    }

    #[test]
    fn confirmed_array_or_str_params_finds_nothing_for_a_plain_scalar_parameter() {
        let body = Expr::Binary(BinOp::Add, Box::new(var("n")), Box::new(var("n")));
        let found = confirmed_array_or_str_params(&body, &["n".to_string()]);
        assert!(found.is_empty(), "plain arithmetic is no evidence of array/string shape: {found:?}");
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
