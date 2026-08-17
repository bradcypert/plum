//! Whole-program dead-function elimination, run AFTER monomorphization
//! and immediately BEFORE codegen.
//!
//! `monomorphize::plan` deliberately seeds its worklist with EVERY
//! non-generic function unconditionally (see its own seeding comment) —
//! it has to, since `MonoPlan::functions` fully replaces `lower_program`'s
//! function list, and a plain function that never touches a generic type
//! would otherwise vanish. The consequence is that only GENERIC prelude
//! functions ever get dropped: every non-generic one survives into the
//! emitted program whether or not anything can reach it. A hello-world
//! program was emitting 256 functions, among them the whole HTTP server —
//! `http_serve_loop` and its `spawn`.
//!
//! That was not merely wasteful. `plum-codegen`'s whole-program
//! closure/task-field rejection (`check_no_closure_or_task_fields`) is
//! gated on "does this program use `spawn` or a channel ANYWHERE", and
//! its own doc comment promises that "a program that never actually
//! spawns anything is completely unaffected, no matter what its struct/
//! enum fields look like". The unreachable prelude `spawn` silently
//! falsified that promise for every program ever compiled: the gate was
//! always open, so declaring a struct with a closure-typed field was
//! rejected universally rather than only in genuinely concurrent
//! programs. Pruning here restores the gate's documented meaning at the
//! root rather than special-casing the check to ignore prelude code —
//! a user program that DOES call `http_serve_loop` still pulls the
//! `spawn` in, and is still correctly subject to the restriction.
//!
//! Deliberately conservative in the retaining direction, matching this
//! crate's established precedent for whole-program shape analyses: the
//! only thing that makes a function live is its name appearing
//! SYNTACTICALLY anywhere in a live body, with no attempt to reason
//! about scoping. A local variable that happens to shadow a top-level
//! function's name therefore keeps that function alive — wasteful, never
//! wrong. Erring the other way would drop a function that codegen still
//! emits a call to, which surfaces as a link failure at best and a
//! miscompile at worst.
//!
//! Not run on the interpreter's path: `plum-interp` can be asked to
//! invoke ANY top-level function by name (its test helpers routinely
//! call something other than `main`), so it has no single entry point to
//! root a reachability walk at. This pass is invoked only by `plumc`'s
//! codegen pipeline, which does.

use crate::ir::{Expr, Program};
use std::collections::HashSet;

/// Drops every function in `program` that no root can reach.
///
/// Roots are `entry_points` (the resolved entry function — possibly
/// several mangled instantiations of one generic name) plus every
/// global's initializer, since globals are always evaluated and are
/// never themselves pruned.
///
/// A name in `entry_points` that doesn't exist in `program.functions` is
/// ignored rather than being an error: `plumc` roots the walk at both an
/// entry's unmangled surface name and its mangled instantiations without
/// knowing which of the two monomorphization actually produced.
pub fn prune_unreachable(program: &mut Program, entry_points: &[String]) {
    let all: HashSet<&str> = program.functions.iter().map(|f| f.name.as_str()).collect();

    let mut live: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = Vec::new();

    let root = |name: &str, live: &mut HashSet<String>, queue: &mut Vec<String>| {
        if all.contains(name) && live.insert(name.to_string()) {
            queue.push(name.to_string());
        }
    };

    for e in entry_points {
        root(e, &mut live, &mut queue);
    }
    // Globals are never pruned, so their initializers are roots, not
    // reachable-from-something bodies.
    for g in &program.globals {
        for name in referenced_names(&g.value) {
            root(name, &mut live, &mut queue);
        }
    }

    while let Some(name) = queue.pop() {
        // Indexing by name each iteration rather than holding a borrow:
        // `live`/`queue` are mutated inside the loop, and the function
        // list is small enough that the lookup cost is irrelevant next
        // to codegen itself.
        let Some(f) = program.functions.iter().find(|f| f.name == name) else {
            continue;
        };
        let referenced: Vec<String> = referenced_names(&f.body).into_iter().map(str::to_string).collect();
        for r in referenced {
            root(&r, &mut live, &mut queue);
        }
    }

    program.functions.retain(|f| live.contains(&f.name));
}

/// Every name mentioned anywhere in `root`'s subtree that could denote a
/// top-level function.
///
/// A top-level function is only ever REFERENCED as `Expr::Var` (a call
/// is `Call { callee: Box<Expr::Var(..)>, .. }`, and a bare function
/// name used as a value is the same node) — but the binder-ish `String`
/// fields (`Assign::name`, and the `reuse_of`/`target` names the FBIP
/// pass introduces) are collected too. Those always denote LOCALS in
/// practice; including them costs only over-retention and removes the
/// whole class of "some future lowering puts a global's name here"
/// bug, which would otherwise prune a live function silently.
fn referenced_names(root: &Expr) -> Vec<&str> {
    let mut names = Vec::new();
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        walk(e, &mut names, &mut stack);
    }
    names
}

/// One node's worth of `referenced_names`: pushes any names it mentions
/// onto `names` and its direct children onto `stack`.
///
/// Exhaustive over `Expr` with no `_` arm, deliberately — a new variant
/// carrying a sub-expression must fail to compile here rather than
/// silently making whatever it references look dead.
fn walk<'a>(e: &'a Expr, names: &mut Vec<&'a str>, stack: &mut Vec<&'a Expr>) {
    match e {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Unit
        | Expr::EmptyArray(_)
        | Expr::Channel
        | Expr::ArgsRaw
        | Expr::RandomRaw => {}

        Expr::Var(name) => names.push(name),

        Expr::Unary(_, a) | Expr::AsCStr(a) | Expr::AsString(a) | Expr::ToIntTrunc(a) | Expr::ToIntRound(a) | Expr::ToFloat(a) => {
            stack.push(a);
        }
        Expr::Binary(_, a, b) => {
            stack.push(a);
            stack.push(b);
        }

        Expr::Let { name, value, body } => {
            names.push(name);
            stack.push(value);
            stack.push(body);
        }
        Expr::If { cond, then_branch, else_branch } => {
            stack.push(cond);
            stack.push(then_branch);
            stack.push(else_branch);
        }
        Expr::Call { callee, args } => {
            stack.push(callee);
            stack.extend(args);
        }
        // `name` is an `extern` declaration's name, never a `Function`'s
        // — collected anyway, since `referenced_names`' consumer only
        // ever intersects against the real function set.
        Expr::ExternCall { name, args } => {
            names.push(name);
            stack.extend(args);
        }
        Expr::Ctor { tag: _, fields } => stack.extend(fields),
        Expr::CtorReuse { reuse_of, tag: _, fields } => {
            names.push(reuse_of);
            stack.extend(fields);
        }
        Expr::Match { scrutinee, arms } => {
            stack.push(scrutinee);
            for arm in arms {
                for b in &arm.bindings {
                    names.push(b);
                }
                if let Some(g) = &arm.guard {
                    stack.push(g);
                }
                stack.push(&arm.body);
            }
        }
        Expr::RcAnnotated { op: _, target, rest } => {
            names.push(target);
            stack.push(rest);
        }
        Expr::For { var, start, end, body } => {
            names.push(var);
            stack.push(start);
            stack.push(end);
            stack.push(body);
        }
        Expr::Closure { params, param_types: _, ret_type: _, body } => {
            for p in params {
                names.push(p);
            }
            stack.push(body);
        }
        Expr::Assign { name, value, rest } => {
            names.push(name);
            stack.push(value);
            stack.push(rest);
        }
        Expr::Spawn { block } => stack.push(block),
        Expr::TaskJoin { task } => stack.push(task),
        Expr::ChannelSend { sender, value } => {
            stack.push(sender);
            stack.push(value);
        }
        Expr::ChannelRecv { receiver } => stack.push(receiver),
        Expr::Select { arms } => {
            for arm in arms {
                stack.push(&arm.receiver);
                stack.push(&arm.body);
            }
        }

        Expr::Index { base, index } => {
            stack.push(base);
            stack.push(index);
        }
        Expr::ArrayLen { array } | Expr::ArrayPop { array } => stack.push(array),
        Expr::ArrayPush { array, value } => {
            stack.push(array);
            stack.push(value);
        }
        Expr::ArraySet { array, index, value } => {
            stack.push(array);
            stack.push(index);
            stack.push(value);
        }
        Expr::ArrayRemove { array, index } => {
            stack.push(array);
            stack.push(index);
        }
        Expr::ArrayPushReuse { reuse_of, value } => {
            names.push(reuse_of);
            stack.push(value);
        }
        Expr::ArrayPopReuse { reuse_of } | Expr::StrTrimReuse { reuse_of } | Expr::StrToUpperReuse { reuse_of } | Expr::StrToLowerReuse { reuse_of } => {
            names.push(reuse_of);
        }
        Expr::ArraySetReuse { reuse_of, index, value } => {
            names.push(reuse_of);
            stack.push(index);
            stack.push(value);
        }
        Expr::ArrayRemoveReuse { reuse_of, index } => {
            names.push(reuse_of);
            stack.push(index);
        }

        Expr::StrConcat { base, other } => {
            stack.push(base);
            stack.push(other);
        }
        Expr::StrConcatReuse { reuse_of, other } => {
            names.push(reuse_of);
            stack.push(other);
        }
        Expr::StrRunes { base }
        | Expr::StrTrim { base }
        | Expr::StrToUpper { base }
        | Expr::StrToLower { base }
        | Expr::ToString { base }
        | Expr::StrHash { base }
        | Expr::RefGet { base } => stack.push(base),
        Expr::StrSplit { base, sep } => {
            stack.push(base);
            stack.push(sep);
        }
        Expr::StrContains { base, needle } => {
            stack.push(base);
            stack.push(needle);
        }
        Expr::StrStartsWith { base, prefix } => {
            stack.push(base);
            stack.push(prefix);
        }
        Expr::StrEndsWith { base, suffix } => {
            stack.push(base);
            stack.push(suffix);
        }
        Expr::StrReplace { base, from, to } => {
            stack.push(base);
            stack.push(from);
            stack.push(to);
        }
        Expr::StrReplaceReuse { reuse_of, from, to } => {
            names.push(reuse_of);
            stack.push(from);
            stack.push(to);
        }

        Expr::RefNew { value } => stack.push(value),
        Expr::RefSet { base, value } => {
            stack.push(base);
            stack.push(value);
        }
        Expr::ReadFileRaw { path } => stack.push(path),
        Expr::WriteFileRaw { path, contents } => {
            stack.push(path);
            stack.push(contents);
        }
        Expr::EnvVarRaw { name } => stack.push(name),
        Expr::PanicRaw { message } => stack.push(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Function, Global};

    fn call(name: &str) -> Expr {
        Expr::Call { callee: Box::new(Expr::Var(name.to_string())), args: vec![] }
    }

    fn func(name: &str, body: Expr) -> Function {
        Function { name: name.to_string(), params: vec![], body }
    }

    fn names(p: &Program) -> Vec<&str> {
        let mut n: Vec<&str> = p.functions.iter().map(|f| f.name.as_str()).collect();
        n.sort();
        n
    }

    fn program(functions: Vec<Function>, globals: Vec<Global>) -> Program {
        Program { functions, globals, externs: vec![] }
    }

    #[test]
    fn keeps_the_entry_point_and_everything_it_transitively_reaches() {
        let mut p = program(
            vec![
                func("main", call("a")),
                func("a", call("b")),
                func("b", Expr::Int(1)),
                func("dead", call("also_dead")),
                func("also_dead", Expr::Int(2)),
            ],
            vec![],
        );
        prune_unreachable(&mut p, &["main".to_string()]);
        assert_eq!(names(&p), vec!["a", "b", "main"]);
    }

    #[test]
    fn a_globals_initializer_is_a_root() {
        // Globals are never pruned and are always evaluated, so anything
        // an initializer reaches is live even with no path from `main`.
        let mut p = program(
            vec![func("main", Expr::Int(0)), func("used_by_global", Expr::Int(1)), func("dead", Expr::Int(2))],
            vec![Global { name: "g".to_string(), value: call("used_by_global") }],
        );
        prune_unreachable(&mut p, &["main".to_string()]);
        assert_eq!(names(&p), vec!["main", "used_by_global"]);
    }

    #[test]
    fn mutual_recursion_terminates_and_is_retained() {
        let mut p = program(vec![func("main", call("ping")), func("ping", call("pong")), func("pong", call("ping"))], vec![]);
        prune_unreachable(&mut p, &["main".to_string()]);
        assert_eq!(names(&p), vec!["main", "ping", "pong"]);
    }

    #[test]
    fn a_function_named_only_as_a_bare_value_is_retained() {
        // A top-level function used as a first-class value is a plain
        // `Var`, with no enclosing `Call` — the reason reachability is
        // computed over every `Var` rather than over callee positions.
        let mut p = program(
            vec![
                func("main", Expr::Let { name: "f".to_string(), value: Box::new(Expr::Var("handler".to_string())), body: Box::new(Expr::Unit) }),
                func("handler", Expr::Int(1)),
            ],
            vec![],
        );
        prune_unreachable(&mut p, &["main".to_string()]);
        assert_eq!(names(&p), vec!["handler", "main"]);
    }

    #[test]
    fn extra_roots_are_kept_alongside_the_entry_point() {
        // `plum test`'s shape: each test function is its own entry point
        // and is reachable from `main` in no way at all.
        let mut p = program(vec![func("main", Expr::Unit), func("test_one", call("helper")), func("helper", Expr::Int(1)), func("dead", Expr::Int(2))], vec![]);
        prune_unreachable(&mut p, &["main".to_string(), "test_one".to_string()]);
        assert_eq!(names(&p), vec!["helper", "main", "test_one"]);
    }

    #[test]
    fn a_root_naming_no_real_function_is_ignored_rather_than_panicking() {
        // `plumc` roots at both an entry's unmangled surface name and
        // its mangled instantiations without knowing which exists.
        let mut p = program(vec![func("main", Expr::Unit)], vec![]);
        prune_unreachable(&mut p, &["main".to_string(), "main$Int".to_string()]);
        assert_eq!(names(&p), vec!["main"]);
    }

    #[test]
    fn a_local_shadowing_a_function_name_retains_that_function() {
        // Documents the deliberate conservatism: no scope analysis, so
        // this keeps `shadowed` even though the `Var` resolves to the
        // `let`. Wasteful, never wrong — see the module doc comment.
        let mut p = program(
            vec![
                func(
                    "main",
                    Expr::Let { name: "shadowed".to_string(), value: Box::new(Expr::Int(1)), body: Box::new(Expr::Var("shadowed".to_string())) },
                ),
                func("shadowed", Expr::Int(2)),
            ],
            vec![],
        );
        prune_unreachable(&mut p, &["main".to_string()]);
        assert_eq!(names(&p), vec!["main", "shadowed"]);
    }
}
