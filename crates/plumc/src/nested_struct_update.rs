//! Expands dotted-path struct-literal fields (`Game { ship.position.x:
//! nx, ..g }`) into genuinely nested `StructLiteral`s, BEFORE type
//! inference ever runs:
//!
//! ```text
//! Game { ship.position.x: nx, ship.position.y: ny, score: s, ..g }
//! =>
//! Game {
//!     ship: Ship { position: Vec2 { x: nx, y: ny, ..g.ship.position }, ..g.ship },
//!     score: s,
//!     ..g
//! }
//! ```
//!
//! **Why a separate pass, not new `infer.rs`/`lower.rs` logic**: exactly
//! `plumc::assoc_fns`'s own reasoning — this is pure AST-to-AST syntax
//! sugar. Once every dotted path is expanded into ordinary nested
//! `StructLiteral`s (built from ordinary `Field` accesses off the
//! user's own `..` spread expression), the COMPLETELY UNCHANGED
//! existing struct-literal type-checking, lowering, and FBIP reuse-in-
//! place machinery handles the rest — including, for free, the
//! "unknown field", "missing field", and "duplicate field" errors this
//! sugar can hit (a path colliding with a plain field of the same name
//! becomes two ordinary `FieldInit`s with the same name, which the
//! existing duplicate-field check already rejects).
//!
//! **Why this pass needs a `TypeContext`, unlike `assoc_fns`**: a
//! nested `StructLiteral` needs a concrete TYPE NAME to construct
//! (`Ship { ... }`, not just `{ ... }` — Plum has no anonymous-struct
//! literal syntax) — so resolving `ship.position: ...` requires knowing
//! `Game.ship`'s declared field type is named `"Ship"`. That's a `Type
//! Context::struct_fields` lookup, not something derivable from syntax
//! alone.
//!
//! **Known v1 scope limit**: an intermediate path segment whose
//! declared field type is still a bare generic parameter (`struct
//! Wrapper[T] { inner: T }`, `w.inner.field: x`) can't be resolved to a
//! concrete struct name at this stage — `TypeContext` only has `Wrapper`'s
//! DECLARED field shape (`Type::Param("T")`), not whatever `T` happens
//! to be instantiated to at any particular use site (that needs real
//! per-call-site type inference, a materially bigger change). Reported
//! as a clear compile error rather than silently doing something wrong.
//!
//! **Where this runs**: BEFORE `assoc_fns::resolve_associated_calls`,
//! on the parsed `Program`, using a `TypeContext` built from that same,
//! not-yet-`assoc_fns`-rewritten `Program` — safe because `TypeContext::
//! from_items` only ever reads top-level struct/enum/extern
//! DECLARATIONS, never expression bodies, so `assoc_fns` having not run
//! yet changes nothing it looks at (see the callers' own comments for
//! where the `TypeContext::from_items` call was moved earlier to make
//! this ordering possible).

use plum_syntax::ast;
use plum_syntax::error::CompileError;
use plum_syntax::span::Span;
use plum_types::context::TypeContext;
use plum_types::types::Type;

/// Walks every top-level `let` definition's body in `program` in place,
/// expanding every dotted-path `FieldInit` it finds. See this module's
/// own doc comment for the full design.
pub fn expand_nested_field_updates(program: &mut ast::Program, ctx: &TypeContext) -> Result<(), CompileError> {
    for item in &mut program.items {
        if let ast::ItemKind::Let(def) = &mut item.kind {
            expand_expr(&mut def.body, ctx)?;
        }
    }
    Ok(())
}

fn expand_expr(expr: &mut ast::Expr, ctx: &TypeContext) -> Result<(), CompileError> {
    match expr {
        ast::Expr::StructLiteral { path, fields, spread, span } => {
            for f in fields.iter_mut() {
                expand_expr(&mut f.value, ctx)?;
            }
            if let Some(s) = spread {
                expand_expr(s, ctx)?;
            }
            if fields.iter().any(|f| !f.extra_path.is_empty()) {
                let struct_name = path.last().cloned().expect("a path always has at least one segment");
                let Some(spread_expr) = spread.take() else {
                    return Err(CompileError::new(
                        *span,
                        "a nested field-update path (`a.b: value`) requires this struct literal to also \
                         have a `..` spread — there's nothing else to read the intermediate values from"
                            .to_string(),
                    ));
                };
                *fields = build_nested_fields(&struct_name, std::mem::take(fields), &spread_expr, ctx, *span)?;
                *spread = Some(spread_expr);
            }
            Ok(())
        }
        ast::Expr::Ident(..) | ast::Expr::Int(..) | ast::Expr::Float(..) | ast::Expr::Str(..) | ast::Expr::Bool(..) => Ok(()),
        ast::Expr::Field { base, .. } => expand_expr(base, ctx),
        ast::Expr::Tuple(elems, _) | ast::Expr::ArrayLiteral(elems, _) => {
            for e in elems.iter_mut() {
                expand_expr(e, ctx)?;
            }
            Ok(())
        }
        ast::Expr::Unary { expr: inner, .. } => expand_expr(inner, ctx),
        ast::Expr::Binary { lhs, rhs, .. } => {
            expand_expr(lhs, ctx)?;
            expand_expr(rhs, ctx)
        }
        ast::Expr::Call { callee, args, .. } => {
            expand_expr(callee, ctx)?;
            for a in args.iter_mut() {
                expand_expr(a, ctx)?;
            }
            Ok(())
        }
        // `args` here are `Type`s, not `Expr`s — a `Type` can never
        // contain a struct literal, nothing to walk.
        ast::Expr::GenericInst { callee, .. } => expand_expr(callee, ctx),
        ast::Expr::Index { base, index, .. } => {
            expand_expr(base, ctx)?;
            expand_expr(index, ctx)
        }
        ast::Expr::Block(block, _) => expand_block(block, ctx),
        ast::Expr::If { cond, then_branch, else_branch, .. } => {
            expand_expr(cond, ctx)?;
            expand_block(then_branch, ctx)?;
            if let Some(e) = else_branch {
                expand_expr(e, ctx)?;
            }
            Ok(())
        }
        ast::Expr::Match { scrutinee, arms, .. } => {
            expand_expr(scrutinee, ctx)?;
            for arm in arms.iter_mut() {
                if let Some(g) = &mut arm.guard {
                    expand_expr(g, ctx)?;
                }
                expand_expr(&mut arm.body, ctx)?;
            }
            Ok(())
        }
        ast::Expr::For { iter, body, .. } => {
            expand_expr(iter, ctx)?;
            expand_block(body, ctx)
        }
        ast::Expr::Closure { body, .. } => expand_expr(body, ctx),
        ast::Expr::Unsafe(block, _) => expand_block(block, ctx),
        ast::Expr::Spawn(block, _) => expand_block(block, ctx),
        ast::Expr::Select { arms, .. } => {
            for arm in arms.iter_mut() {
                expand_expr(&mut arm.expr, ctx)?;
                expand_expr(&mut arm.body, ctx)?;
            }
            Ok(())
        }
    }
}

fn expand_block(block: &mut ast::Block, ctx: &TypeContext) -> Result<(), CompileError> {
    for stmt in block.stmts.iter_mut() {
        match stmt {
            ast::Stmt::Let { value, .. } | ast::Stmt::Assign { value, .. } => expand_expr(value, ctx)?,
            ast::Stmt::Expr(e) => expand_expr(e, ctx)?,
        }
    }
    if let Some(tail) = &mut block.tail {
        expand_expr(tail, ctx)?;
    }
    Ok(())
}

/// Splits `fields` (a mix of ordinary and dotted-path `FieldInit`s,
/// already recursively expanded) into the final flat list for
/// `struct_name`'s literal: ordinary fields pass through unchanged;
/// dotted-path fields are grouped by their FIRST segment (so `ship.
/// position.x`/`ship.position.y` merge into ONE nested `ship: Ship {
/// position: Vec2 { x: .., y: .. }, .. }` field, not two independent,
/// mutually-clobbering reconstructions) and recursively expanded, each
/// group's own spread reading `spread_base.segment`.
/// One dotted-path field's remaining segments (after the first, already
/// used to group it — EACH with its own real span, not just the leaf),
/// original value, and the original `FieldInit`'s overall span (kept
/// only as a fallback error location) — named to satisfy clippy's
/// `type_complexity` lint on the grouping `Vec` below.
type PathEntry = (Vec<(String, Span)>, ast::Expr, Span);

fn build_nested_fields(
    struct_name: &str,
    fields: Vec<ast::FieldInit>,
    spread_base: &ast::Expr,
    ctx: &TypeContext,
    span: Span,
) -> Result<Vec<ast::FieldInit>, CompileError> {
    let struct_fields = ctx
        .struct_fields(struct_name)
        .ok_or_else(|| CompileError::new(span, format!("unknown struct type {struct_name:?}")))?;

    let mut out = Vec::new();
    // Grouped by first segment, preserving first-seen order — a plain
    // `Vec` plus linear lookup, not a `HashMap`, since a single struct
    // literal only ever has a handful of fields; no need for a new
    // dependency (or even `std::collections::HashMap`'s unordered
    // iteration, which would reorder the output nondeterministically)
    // to keep this fast.
    let mut groups: Vec<(String, Span, Vec<PathEntry>)> = Vec::new();
    for f in fields {
        if f.extra_path.is_empty() {
            out.push(f);
            continue;
        }
        match groups.iter_mut().find(|(name, _, _)| name == &f.name) {
            Some((_, _, entries)) => entries.push((f.extra_path, f.value, f.span)),
            None => groups.push((f.name, f.name_span, vec![(f.extra_path, f.value, f.span)])),
        }
    }

    for (first_seg, first_seg_span, entries) in groups {
        let field_ty = struct_fields
            .iter()
            .find(|(n, _)| n == &first_seg)
            .map(|(_, t)| t)
            .ok_or_else(|| CompileError::new(span, format!("struct {struct_name:?} has no field {first_seg:?}")))?;
        let inner_struct_name = match field_ty {
            Type::Struct(name, _) => name.clone(),
            Type::Param(name) => {
                return Err(CompileError::new(
                    span,
                    format!(
                        "field `{first_seg}` has generic type {name:?} here — a nested field-update path needs \
                         a concrete struct type, not a generic parameter (write this level out by hand instead)"
                    ),
                ));
            }
            other => {
                return Err(CompileError::new(
                    span,
                    format!(
                        "field `{first_seg}` has type {other:?}, not a struct — every intermediate segment of a \
                         nested field-update path must be a struct field"
                    ),
                ));
            }
        };
        // `inner_spread`'s span comes from `first_seg`'s OWN real
        // parsed span (`name_span` for a top-level segment, or a
        // segment's own span from `extra_path` one level down) —
        // deliberately NOT the outer literal's shared `span`, and NOT
        // reused unchanged at multiple nesting depths, because
        // `field_owners` (the span-keyed `Span -> struct name` side-
        // channel `infer.rs` hands lowering, needed since lowering has
        // no type information of its own) would otherwise silently
        // clobber one entry with another's: `g.ship` and `g.ship.
        // position` are two DIFFERENT `Field` nodes that each need
        // their OWN owner recorded, and a `HashMap` keyed by a span
        // they both shared would only ever keep the later one,
        // corrupting the earlier access at lowering time (a real bug
        // this exact reasoning was needed to catch and fix — see this
        // module's own tests). Per-segment spans from parsing give
        // every level real, unique provenance, not a fabricated one.
        let inner_spread = ast::Expr::Field {
            base: Box::new(spread_base.clone()),
            name: first_seg.clone(),
            span: first_seg_span,
        };
        let inner_fields: Vec<ast::FieldInit> = entries
            .into_iter()
            .map(|(mut rest, value, fspan)| {
                let (name, name_span) = rest.remove(0);
                ast::FieldInit { name, name_span, extra_path: rest, value, span: fspan }
            })
            .collect();
        let nested_fields = build_nested_fields(&inner_struct_name, inner_fields, &inner_spread, ctx, first_seg_span)?;
        out.push(ast::FieldInit {
            name: first_seg,
            name_span: first_seg_span,
            extra_path: Vec::new(),
            value: ast::Expr::StructLiteral {
                path: vec![inner_struct_name],
                fields: nested_fields,
                spread: Some(Box::new(inner_spread)),
                span: first_seg_span,
            },
            span: first_seg_span,
        });
    }
    Ok(out)
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
                ast::Expr::Bool(b, _) => b.to_string(),
                ast::Expr::Field { base, name, .. } => format!("{}.{}", render_expr(base), name),
                ast::Expr::StructLiteral { path, fields, spread, .. } => {
                    let mut parts = vec![path.join(".")];
                    for f in fields {
                        parts.push(format!("{}={}", f.name, render_expr(&f.value)));
                    }
                    if let Some(s) = spread {
                        parts.push(format!("..{}", render_expr(s)));
                    }
                    format!("{{{}}}", parts.join(" "))
                }
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

    fn expand(src: &str) -> Result<ast::Program, CompileError> {
        let mut program = parse(src);
        let ctx = TypeContext::from_items(&program.items).unwrap_or_else(|e| panic!("context error for {src:?}: {e}"));
        expand_nested_field_updates(&mut program, &ctx)?;
        Ok(program)
    }

    #[test]
    fn a_single_level_nested_path_expands_to_a_real_nested_literal() {
        let program = expand(
            "struct Vec2 { x: Int, y: Int }\n\
             struct Ship { position: Vec2, alive: Bool }\n\
             let use_it (s: Ship) (nx: Int) = Ship { position.x: nx, ..s }",
        )
        .unwrap();
        // Each `struct` item renders as an empty segment (`render` only
        // prints `Let` bodies) — 2 struct decls here, so 2 leading
        // empty `" | "` segments before the actual result.
        assert_eq!(
            render(&program),
            " |  | \
             {Ship position={Vec2 x=nx ..s.position} ..s}"
        );
    }

    #[test]
    fn two_paths_sharing_a_prefix_merge_into_one_nested_literal() {
        let program = expand(
            "struct Vec2 { x: Int, y: Int }\n\
             struct Ship { position: Vec2, alive: Bool }\n\
             let use_it (s: Ship) (nx: Int) (ny: Int) = \
                 Ship { position.x: nx, position.y: ny, ..s }",
        )
        .unwrap();
        assert_eq!(render(&program), " |  | {Ship position={Vec2 x=nx y=ny ..s.position} ..s}");
    }

    #[test]
    fn a_two_level_deep_path_expands_recursively() {
        let program = expand(
            "struct Vec2 { x: Int, y: Int }\n\
             struct Ship { position: Vec2 }\n\
             struct Game { ship: Ship, score: Int }\n\
             let use_it (g: Game) (nx: Int) = Game { ship.position.x: nx, ..g }",
        )
        .unwrap();
        assert_eq!(
            render(&program),
            " |  |  | \
             {Game ship={Ship position={Vec2 x=nx ..g.ship.position} ..g.ship} ..g}"
        );
    }

    #[test]
    fn a_plain_field_and_a_nested_path_coexist_at_the_same_level() {
        let program = expand(
            "struct Vec2 { x: Int, y: Int }\n\
             struct Ship { position: Vec2, score: Int }\n\
             let use_it (s: Ship) (nx: Int) = Ship { score: 1, position.x: nx, ..s }",
        )
        .unwrap();
        assert_eq!(render(&program), " |  | {Ship score=1 position={Vec2 x=nx ..s.position} ..s}");
    }

    #[test]
    fn a_nested_path_without_a_spread_is_an_error() {
        let err = expand(
            "struct Vec2 { x: Int, y: Int }\n\
             struct Ship { position: Vec2 }\n\
             let use_it (nx: Int) = Ship { position.x: nx }",
        )
        .expect_err("expected an error: no `..` spread to read the old value from");
        assert!(err.to_string().contains("`..` spread"), "{err}");
    }

    #[test]
    fn a_path_through_a_non_struct_field_is_an_error() {
        let err = expand(
            "struct Ship { alive: Bool }\n\
             let use_it (s: Ship) = Ship { alive.foo: true, ..s }",
        )
        .expect_err("expected an error: `alive` isn't a struct field");
        assert!(err.to_string().contains("not a struct"), "{err}");
    }

    #[test]
    fn a_path_through_a_generic_field_is_a_clear_error_not_a_panic() {
        let err = expand(
            "struct Wrapper[T] { inner: T }\n\
             let use_it (w: Wrapper[Int]) = Wrapper { inner.foo: 1, ..w }",
        )
        .expect_err("expected an error: `inner`'s type is still generic here");
        assert!(err.to_string().contains("generic"), "{err}");
    }

    #[test]
    fn an_unknown_field_in_a_nested_path_is_an_error() {
        let err = expand(
            "struct Vec2 { x: Int, y: Int }\n\
             struct Ship { position: Vec2 }\n\
             let use_it (s: Ship) = Ship { nope.x: 1, ..s }",
        )
        .expect_err("expected an error: `nope` isn't a real field");
        assert!(err.to_string().contains("no field"), "{err}");
    }

    #[test]
    fn a_struct_literal_with_no_nested_paths_is_left_completely_untouched() {
        let program = expand(
            "struct Point { x: Int, y: Int }\n\
             let use_it (p: Point) = Point { x: 1, ..p }",
        )
        .unwrap();
        assert_eq!(render(&program), " | {Point x=1 ..p}");
    }
}


