//! Resolves `Type.func(args)` — real, per-type associated-function call
//! syntax (`Option.map`, `Array.reverse`, a user's own `Point.add`) —
//! into an ordinary call against the top-level function LITERALLY
//! named `"Type.func"`.
//!
//! **Why a separate pass, not new `infer.rs`/`lower.rs` logic**: the
//! declaration side (`plum-syntax::parser::parse_let_def`) already
//! stores an associated function's name as ONE combined string,
//! `"Type.func"`, directly in `LetDef.name` — mirroring `plumc::
//! modules::qualify`'s own module-qualification trick exactly (that
//! mechanism is proven end to end: every downstream consumer, from
//! duplicate-name checking through monomorphization to LLVM symbol
//! emission, already treats `LetDef.name`/a function's name as an
//! opaque string key, with a `.` inside it already proven safe all the
//! way to a real compiled binary). So the ONLY thing missing is
//! teaching a CALL SITE (`Point.add(a, b)`, which parses to exactly
//! the same `Field`-wrapped-in-`Call` shape as `p.add(b)` or
//! `Shape.Circle(1.0)`) to resolve back to that same `"Point.add"`
//! string — once this pass rewrites `Field { base: Ident("Point"),
//! name: "add" }` into `Ident("Point.add")`, type inference/lowering/
//! codegen see an utterly ordinary named-function call, identical to
//! what `point_add(a, b)` would have looked like under the old flat-
//! name convention. Zero new IR, zero `infer.rs`/`lower.rs` changes.
//!
//! **Disambiguation from `Type.Variant(args)`** (qualified enum-variant
//! construction, e.g. `Shape.Circle(1.0)` — a completely different,
//! pre-existing feature, resolved later by `infer.rs`/`lower.rs`'s own
//! bare-tag-name lookup) is free and unambiguous: this codebase's own
//! existing convention is that variant tags are always UpperCamelCase
//! (`Some`, `None`, `Ok`, `Circle`); associated functions declared via
//! `let Type.func` are always lowercase (`map`, `add`, `and_then`), the
//! same convention ordinary function names already follow. So this
//! pass only ever matches `Type.lowercase_name` — `Type.UpperCase` is
//! never touched, falling through completely unaffected to the
//! existing variant-construction path.
//!
//! **Local-shadowing guard**: mirrors `plumc::modules::Resolver`'s own
//! `locals.contains(name)` precedent exactly (the identical bug class
//! that resolver once had to be fixed for — see `modules.rs`'s own
//! `a_multi_segment_field_access_off_a_local_shadowing_a_used_module_
//! name_is_ordinary_field_access` regression test) — a function
//! parameter or local variable that happens to share a name with some
//! unrelated declared type is never rewritten; ordinary field access
//! (`p.x`) or builtin method dispatch (`s.trim()`, `arr.push(x)`) on a
//! REAL local value is always left completely alone, since `base` in
//! those cases is either not a bare `Ident` at all, or names a local
//! currently in scope — checked before this pass's own type/function
//! registry lookup ever runs.
//!
//! **Where this runs**: on the FINAL, fully-merged `ast::Program` — for
//! a multi-file project, `with_prelude`/`modules::Resolver`'s own
//! cross-module qualification have ALREADY run by the time this pass
//! sees the program (see the two call sites in `lib.rs`/`codegen_cli.
//! rs`), so it never needs to know or care whether the program came
//! from one file or a whole project; it always sees one flat, already-
//! qualified `Program`.
//!
//! **Known v1 scope limit**: cross-module associated-function calls
//! (`shapes.Point.add(...)`, a 3-segment path combining a module
//! qualifier with a type qualifier) are NOT handled — this pass only
//! ever matches the flat 2-segment `Type.func` shape. Calling a type's
//! associated function from a different module than where the type/
//! function are declared needs the type itself made reachable the same
//! way any other cross-module name already is (root-module declarations
//! are visible everywhere; a non-root module's need `use`).

use plum_syntax::ast;
use plum_syntax::span::Span;
use std::collections::HashSet;

/// Walks every top-level `let` definition's body in `program` in
/// place, rewriting every `Type.func(...)`/bare `Type.func` reference
/// it finds into the equivalent `Ident("Type.func")` — see this
/// module's own doc comment for the full design.
pub fn resolve_associated_calls(program: &mut ast::Program) {
    let mut declared_names: HashSet<String> = HashSet::new();
    let mut known_types: HashSet<String> = HashSet::new();
    for item in &program.items {
        match &item.kind {
            ast::ItemKind::Let(def) => {
                declared_names.insert(def.name.clone());
            }
            ast::ItemKind::Struct(decl) => {
                known_types.insert(decl.name.clone());
            }
            ast::ItemKind::Enum(decl) => {
                known_types.insert(decl.name.clone());
            }
            ast::ItemKind::Extern(block) => {
                for f in &block.fns {
                    declared_names.insert(f.name.clone());
                }
            }
            ast::ItemKind::Use(_) => {}
        }
    }

    let resolver = Resolver { declared_names, known_types };
    for item in &mut program.items {
        if let ast::ItemKind::Let(def) = &mut item.kind {
            let mut locals = HashSet::new();
            param_bindings(&def.params, &mut locals);
            resolver.resolve_expr(&mut def.body, &mut locals);
        }
    }
}

struct Resolver {
    declared_names: HashSet<String>,
    known_types: HashSet<String>,
}

impl Resolver {
    /// If `expr` is exactly `Field { base: Ident(type_name), name:
    /// fn_name }`, `type_name` isn't shadowed by a local, `fn_name`
    /// starts lowercase, `type_name` names a real declared struct/enum,
    /// and `"{type_name}.{fn_name}"` is a real declared top-level
    /// function — returns that qualified name and the node's span.
    /// Anything else (including a genuine field access / method
    /// dispatch on a local value) returns `None`, leaving `expr`
    /// completely untouched for the caller's ordinary recursion.
    fn try_resolve(&self, expr: &ast::Expr, locals: &HashSet<String>) -> Option<(String, Span)> {
        let ast::Expr::Field { base, name: fn_name, span } = expr else {
            return None;
        };
        let ast::Expr::Ident(type_name, _) = base.as_ref() else {
            return None;
        };
        if locals.contains(type_name) {
            return None;
        }
        if !fn_name.chars().next().is_some_and(|c| c.is_lowercase()) {
            return None;
        }
        if !self.known_types.contains(type_name) {
            return None;
        }
        let qualified = format!("{type_name}.{fn_name}");
        if !self.declared_names.contains(&qualified) {
            return None;
        }
        Some((qualified, *span))
    }

    fn resolve_expr(&self, expr: &mut ast::Expr, locals: &mut HashSet<String>) {
        // Checked BEFORE any structural recursion, same "whole chain
        // first" ordering `modules::Resolver::resolve_expr` uses — a
        // `Call { callee: Field{..}, .. }`'s callee is handled here
        // too, since `Call`'s own recursion below calls back into this
        // same function on `callee`.
        if let Some((qualified, span)) = self.try_resolve(expr, locals) {
            *expr = ast::Expr::Ident(qualified, span);
            return;
        }
        match expr {
            ast::Expr::Ident(..) | ast::Expr::Int(..) | ast::Expr::Float(..) | ast::Expr::Str(..) | ast::Expr::Bool(..) => {}
            ast::Expr::Field { base, .. } => self.resolve_expr(base, locals),
            ast::Expr::Tuple(elems, _) | ast::Expr::ArrayLiteral(elems, _) => {
                for e in elems.iter_mut() {
                    self.resolve_expr(e, locals);
                }
            }
            ast::Expr::Unary { expr: inner, .. } => self.resolve_expr(inner, locals),
            ast::Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs, locals);
                self.resolve_expr(rhs, locals);
            }
            ast::Expr::Call { callee, args, .. } => {
                self.resolve_expr(callee, locals);
                for a in args.iter_mut() {
                    self.resolve_expr(a, locals);
                }
            }
            // `args` here are `Type`s, not `Expr`s — a `Type` can never
            // contain a `Type.func` call, nothing to walk.
            ast::Expr::GenericInst { callee, .. } => self.resolve_expr(callee, locals),
            ast::Expr::Index { base, index, .. } => {
                self.resolve_expr(base, locals);
                self.resolve_expr(index, locals);
            }
            ast::Expr::Block(block, _) => self.resolve_block(block, &mut locals.clone()),
            ast::Expr::If { cond, then_branch, else_branch, .. } => {
                self.resolve_expr(cond, locals);
                self.resolve_block(then_branch, &mut locals.clone());
                if let Some(e) = else_branch {
                    self.resolve_expr(e, locals);
                }
            }
            ast::Expr::Match { scrutinee, arms, .. } => {
                self.resolve_expr(scrutinee, locals);
                for arm in arms.iter_mut() {
                    let mut arm_locals = locals.clone();
                    pattern_bindings(&arm.pattern, &mut arm_locals);
                    if let Some(g) = &mut arm.guard {
                        self.resolve_expr(g, &mut arm_locals);
                    }
                    self.resolve_expr(&mut arm.body, &mut arm_locals);
                }
            }
            ast::Expr::For { pattern, iter, body, .. } => {
                self.resolve_expr(iter, locals);
                let mut body_locals = locals.clone();
                pattern_bindings(pattern, &mut body_locals);
                self.resolve_block(body, &mut body_locals);
            }
            ast::Expr::Closure { params, body, .. } => {
                let mut inner = locals.clone();
                for p in params.iter() {
                    inner.insert(p.name.clone());
                }
                self.resolve_expr(body, &mut inner);
            }
            ast::Expr::Unsafe(block, _) => self.resolve_block(block, &mut locals.clone()),
            ast::Expr::Spawn(block, _) => self.resolve_block(block, &mut locals.clone()),
            ast::Expr::StructLiteral { fields, spread, .. } => {
                for f in fields.iter_mut() {
                    self.resolve_expr(&mut f.value, locals);
                }
                if let Some(s) = spread {
                    self.resolve_expr(s, locals);
                }
            }
            ast::Expr::Select { arms, .. } => {
                for arm in arms.iter_mut() {
                    self.resolve_expr(&mut arm.expr, locals);
                    let mut arm_locals = locals.clone();
                    pattern_bindings(&arm.pattern, &mut arm_locals);
                    self.resolve_expr(&mut arm.body, &mut arm_locals);
                }
            }
        }
    }

    fn resolve_block(&self, block: &mut ast::Block, locals: &mut HashSet<String>) {
        for stmt in block.stmts.iter_mut() {
            match stmt {
                ast::Stmt::Let { pattern, value, .. } => {
                    self.resolve_expr(value, locals);
                    pattern_bindings(pattern, locals);
                }
                ast::Stmt::Assign { value, .. } => {
                    self.resolve_expr(value, locals);
                }
                ast::Stmt::Expr(e) => {
                    self.resolve_expr(e, locals);
                }
            }
        }
        if let Some(tail) = &mut block.tail {
            self.resolve_expr(tail, locals);
        }
    }
}

/// Local copy of `modules::pattern_bindings`'s exact logic — small and
/// pure enough that duplicating it here (rather than making the
/// original `pub(crate)`) keeps this pass fully independent of the
/// module-resolution system it's modeled on, matching this crate's own
/// preference for small, focused, independently-reasoned passes.
fn pattern_bindings(pattern: &ast::Pattern, out: &mut HashSet<String>) {
    match pattern {
        ast::Pattern::Ident(name, _) => {
            out.insert(name.clone());
        }
        ast::Pattern::Tuple(pats, _) | ast::Pattern::Or(pats, _) => {
            for p in pats {
                pattern_bindings(p, out);
            }
        }
        ast::Pattern::Variant { args, .. } => {
            for p in args {
                pattern_bindings(p, out);
            }
        }
        ast::Pattern::Struct { fields, .. } => {
            for f in fields {
                pattern_bindings(&f.pattern, out);
            }
        }
        ast::Pattern::Int(..) | ast::Pattern::Float(..) | ast::Pattern::Str(..) | ast::Pattern::Bool(..) | ast::Pattern::Wildcard(_) => {}
    }
}

fn param_bindings(params: &[ast::Param], out: &mut HashSet<String>) {
    for p in params {
        match &p.kind {
            ast::ParamKind::Ident(name) => {
                out.insert(name.clone());
            }
            ast::ParamKind::Pattern(pattern, _) => pattern_bindings(pattern, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    fn parse(src: &str) -> ast::Program {
        let tokens = Lexer::new(src).tokenize();
        Parser::new(tokens).parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"))
    }

    fn render(program: &ast::Program) -> String {
        fn render_expr(e: &ast::Expr) -> String {
            match e {
                ast::Expr::Ident(name, _) => name.clone(),
                ast::Expr::Int(n, _) => n.to_string(),
                ast::Expr::Call { callee, args, .. } => {
                    format!("{}({})", render_expr(callee), args.iter().map(render_expr).collect::<Vec<_>>().join(","))
                }
                ast::Expr::Field { base, name, .. } => format!("{}.{}", render_expr(base), name),
                ast::Expr::Binary { lhs, rhs, .. } => format!("({}+{})", render_expr(lhs), render_expr(rhs)),
                other => format!("{other:?}"),
            }
        }
        program
            .items
            .iter()
            .map(|item| match &item.kind {
                ast::ItemKind::Let(def) => render_expr(&def.body),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn a_qualified_associated_call_is_rewritten_to_the_dotted_name() {
        let mut program = parse("struct Point { x: Int } let Point.add (a: Int) (b: Int): Int = a + b let use_it () = Point.add(1, 2)");
        resolve_associated_calls(&mut program);
        assert_eq!(render(&program), " | (a+b) | Point.add(1,2)");
    }

    #[test]
    fn a_bare_reference_to_an_associated_function_is_also_rewritten() {
        let mut program = parse("struct Point { x: Int } let Point.add (a: Int) (b: Int): Int = a + b let use_it () = Point.add");
        resolve_associated_calls(&mut program);
        assert_eq!(render(&program), " | (a+b) | Point.add");
    }

    #[test]
    fn a_qualified_variant_construction_is_left_completely_untouched() {
        // `Circle` starts uppercase — never matches the (lowercase-only)
        // associated-function shape, regardless of whether `Shape.
        // Circle` happens to also collide with some declared name.
        let mut program = parse("enum Shape { Circle(Int) } let use_it () = Shape.Circle(1)");
        resolve_associated_calls(&mut program);
        assert_eq!(render(&program), " | Shape.Circle(1)");
    }

    #[test]
    fn a_local_variable_shadowing_a_type_name_is_left_as_ordinary_field_access() {
        // `Point` here is a PARAMETER, not the type — `Point.x` must
        // stay ordinary field access, never rewritten, exactly mirroring
        // `modules::Resolver`'s own local-shadowing precedent.
        let mut program = parse(
            "struct Point { x: Int } let Point.x (a: Int): Int = a let use_it (Point: Point): Int = Point.x",
        );
        resolve_associated_calls(&mut program);
        // The SECOND `let` (`use_it`)'s body is left alone (`Point` is
        // a local param there); the FIRST `let` (`Point.x`'s own body,
        // just `a`) is unaffected either way.
        assert_eq!(render(&program), " | a | Point.x");
    }

    #[test]
    fn an_undeclared_function_name_on_a_real_type_is_left_alone() {
        // `Point.subtract` isn't declared anywhere — falls through
        // unchanged, left for ordinary inference to reject with its own
        // (honest, if not maximally specific) error.
        let mut program = parse("struct Point { x: Int } let use_it () = Point.subtract(1, 2)");
        resolve_associated_calls(&mut program);
        assert_eq!(render(&program), " | Point.subtract(1,2)");
    }
}
