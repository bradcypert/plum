use crate::infer::ast_type_to_type;
use crate::types::Type;
use plum_syntax::ast;
use std::collections::{HashMap, HashSet};

/// Struct/enum declarations' field and variant payload types, resolved
/// once from a program's items before inferring anything that uses
/// them — the type-level counterpart to `lower.rs`'s `LoweringContext`
/// (which resolves field ORDER; this resolves field TYPES).
///
/// Generic struct/enum declarations (`struct Pair[T] { .. }`) ARE
/// supported: a field/payload type that mentions one of the
/// declaration's own type parameters is stored as `Type::Param(name)`
/// — see that variant's doc comment for the full instantiate-at-each-
/// use story. `generic_params` records each declaration's parameter
/// NAMES in order, which is what lets a later use site build the right
/// `Type::Struct(name, args)`/`Type::Enum(name, args)` shape.
#[derive(Debug)]
pub struct TypeContext {
    struct_fields: HashMap<String, Vec<(String, Type)>>,
    // variant tag -> (owning enum name, payload types)
    variants: HashMap<String, (String, Vec<Type>)>,
    // Every declared struct/enum NAME — populated in a first pass,
    // before ANY field/payload type is resolved, so `ast_type_to_type`
    // can turn a field type that's itself a struct/enum reference
    // (`struct Line { start: Point, end: Point }`, even forward or
    // mutually-recursive references like `struct A { b: B } struct B
    // { a: A }`) into `Type::Struct`/`Type::Enum`, regardless of
    // declaration order.
    struct_names: HashSet<String>,
    enum_names: HashSet<String>,
    // struct/enum name -> its OWN declared generic parameter names, in
    // declaration order (`struct Pair[T, U] { .. }` -> `["T", "U"]`).
    // Empty for a non-generic declaration. Populated in the SAME first
    // pass as `struct_names`/`enum_names`, for the same reason:
    // resolving `struct Wrapper[T] { inner: Pair[T] }`'s field needs to
    // know `Pair`'s own arity BEFORE `Pair` itself is necessarily fully
    // resolved.
    generic_params: HashMap<String, Vec<String>>,
    // struct/enum name -> each declared parameter's trait bounds, in
    // the SAME positional order as `generic_params` (`struct Box[T:
    // Num] { .. }` -> `[["Num"]]`; `[T: Num + Eq]` -> `[["Num", "Eq"]]`;
    // an unbounded param -> `[]` at that position). Checked once a
    // construction site's generic arguments are FULLY resolved (see
    // `Infer::check_generic_bounds`) — DESIGN.md's small, closed
    // `Num`/`Eq`/`Show` trait set makes this closed-set membership
    // checking, never general typeclass resolution.
    generic_bounds: HashMap<String, Vec<Vec<String>>>,
}

impl TypeContext {
    pub fn new() -> Self {
        TypeContext {
            struct_fields: HashMap::new(),
            variants: HashMap::new(),
            struct_names: HashSet::new(),
            enum_names: HashSet::new(),
            generic_params: HashMap::new(),
            generic_bounds: HashMap::new(),
        }
    }

    pub fn from_items(items: &[ast::Item]) -> Result<Self, String> {
        let mut ctx = Self::new();

        // Phase 1: collect every declared name AND generic-parameter
        // list FIRST — see this struct's doc comments for why. Also
        // where a struct/enum name collision (with EITHER another
        // struct or an enum — they share one flat type-level namespace,
        // same as `ast_type_to_type` resolves both identically) is
        // caught: checked BEFORE inserting, so redeclaring a prelude
        // type (`enum Option[T] { .. }`, injected by `plumc::
        // with_prelude` ahead of the user's own items) is now a real
        // error too, not silent shadowing.
        for item in items {
            match &item.kind {
                ast::ItemKind::Struct(decl) => {
                    if ctx.struct_names.contains(&decl.name) || ctx.enum_names.contains(&decl.name) {
                        return Err(format!("{:?} is already declared (at {:?})", decl.name, decl.span));
                    }
                    ctx.struct_names.insert(decl.name.clone());
                    let params = decl.generics.iter().map(|g| g.name.clone()).collect();
                    ctx.generic_params.insert(decl.name.clone(), params);
                    let bounds = decl.generics.iter().map(|g| g.bound.clone()).collect();
                    ctx.generic_bounds.insert(decl.name.clone(), bounds);
                }
                ast::ItemKind::Enum(decl) => {
                    if ctx.struct_names.contains(&decl.name) || ctx.enum_names.contains(&decl.name) {
                        return Err(format!("{:?} is already declared (at {:?})", decl.name, decl.span));
                    }
                    ctx.enum_names.insert(decl.name.clone());
                    let params = decl.generics.iter().map(|g| g.name.clone()).collect();
                    ctx.generic_params.insert(decl.name.clone(), params);
                    let bounds = decl.generics.iter().map(|g| g.bound.clone()).collect();
                    ctx.generic_bounds.insert(decl.name.clone(), bounds);
                }
                _ => {}
            }
        }

        // Phase 2: resolve field/payload types, now that EVERY name
        // (and every declaration's generic-parameter list) is known.
        // Each declaration's OWN parameter names are passed as
        // `in_scope_params` so `ast_type_to_type` can turn a bare `T`
        // into `Type::Param("T")` — but ONLY while resolving THAT
        // declaration's own fields/payloads; every other call site
        // (including a DIFFERENT struct's fields) passes `&[]`, since a
        // generic parameter is scoped to its own declaration alone.
        for item in items {
            match &item.kind {
                ast::ItemKind::Struct(decl) => {
                    let params: Vec<String> = decl.generics.iter().map(|g| g.name.clone()).collect();
                    let mut fields = Vec::with_capacity(decl.fields.len());
                    for f in &decl.fields {
                        fields.push((f.name.clone(), ast_type_to_type(&f.ty, &ctx, &params)?));
                    }
                    ctx.struct_fields.insert(decl.name.clone(), fields);
                }
                ast::ItemKind::Enum(decl) => {
                    let params: Vec<String> = decl.generics.iter().map(|g| g.name.clone()).collect();
                    for variant in &decl.variants {
                        let payload = variant
                            .payload
                            .iter()
                            .map(|t| ast_type_to_type(t, &ctx, &params))
                            .collect::<Result<Vec<_>, _>>()?;
                        ctx.variants.insert(variant.name.clone(), (decl.name.clone(), payload));
                    }
                }
                _ => {}
            }
        }
        Ok(ctx)
    }

    pub fn struct_fields(&self, name: &str) -> Option<&[(String, Type)]> {
        self.struct_fields.get(name).map(|v| v.as_slice())
    }

    pub fn variant(&self, tag: &str) -> Option<&(String, Vec<Type>)> {
        self.variants.get(tag)
    }

    pub(crate) fn is_struct(&self, name: &str) -> bool {
        self.struct_names.contains(name)
    }

    pub(crate) fn is_enum(&self, name: &str) -> bool {
        self.enum_names.contains(name)
    }

    /// `name`'s own declared generic parameter names, in order — empty
    /// (not `None`) for a non-generic struct/enum, `None` only if
    /// `name` isn't declared at all.
    pub(crate) fn generic_params(&self, name: &str) -> Option<&[String]> {
        self.generic_params.get(name).map(|v| v.as_slice())
    }

    /// `name`'s own declared parameters' trait bounds, in the SAME
    /// positional order as `generic_params` — see `generic_bounds`'s
    /// doc comment.
    pub(crate) fn generic_bounds(&self, name: &str) -> Option<&[Vec<String>]> {
        self.generic_bounds.get(name).map(|v| v.as_slice())
    }
}

impl Default for TypeContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    fn context(src: &str) -> TypeContext {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        TypeContext::from_items(&program.items).unwrap_or_else(|e| panic!("context error for {src:?}: {e}"))
    }

    fn context_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        TypeContext::from_items(&program.items).expect_err(&format!("expected context building for {src:?} to fail"))
    }

    #[test]
    fn resolves_struct_field_types_in_declared_order() {
        let ctx = context("struct Point { x: Float, y: Float }");
        assert_eq!(
            ctx.struct_fields("Point").unwrap(),
            &[("x".to_string(), Type::Float), ("y".to_string(), Type::Float)]
        );
    }

    #[test]
    fn unknown_struct_name_is_none() {
        let ctx = context("struct Point { x: Float, y: Float }");
        assert!(ctx.struct_fields("Vector").is_none());
    }

    #[test]
    fn generic_struct_fields_resolve_to_param_placeholders() {
        let ctx = context("struct Pair[T] { first: T, second: T }");
        assert_eq!(
            ctx.struct_fields("Pair").unwrap(),
            &[("first".to_string(), Type::Param("T".to_string())), ("second".to_string(), Type::Param("T".to_string()))]
        );
        assert_eq!(ctx.generic_params("Pair").unwrap(), &["T".to_string()]);
    }

    #[test]
    fn non_generic_struct_has_no_generic_params() {
        let ctx = context("struct Point { x: Int, y: Int }");
        assert_eq!(ctx.generic_params("Point").unwrap(), &[] as &[String]);
    }

    #[test]
    fn multi_param_generic_struct_resolves_each_param_independently() {
        let ctx = context("struct Pair[A, B] { first: A, second: B }");
        assert_eq!(
            ctx.struct_fields("Pair").unwrap(),
            &[
                ("first".to_string(), Type::Param("A".to_string())),
                ("second".to_string(), Type::Param("B".to_string())),
            ]
        );
        assert_eq!(ctx.generic_params("Pair").unwrap(), &["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn generic_struct_field_referencing_another_generic_struct_resolves() {
        let ctx = context("struct Pair[T] { first: T, second: T }\nstruct Wrapper[T] { inner: Pair[T] }");
        assert_eq!(
            ctx.struct_fields("Wrapper").unwrap(),
            &[("inner".to_string(), Type::Struct("Pair".to_string(), vec![Type::Param("T".to_string())]))]
        );
    }

    #[test]
    fn generic_instantiation_with_wrong_arity_is_an_error() {
        let err = context_err("struct Pair[T] { first: T, second: T }\nstruct Bad { p: Pair[Int, Bool] }");
        assert!(err.contains("generic argument"), "expected a generic-argument-count error, got: {err}");
    }

    #[test]
    fn generic_param_bound_is_recorded() {
        let ctx = context("struct Box[T: Num] { val: T }");
        assert_eq!(ctx.generic_bounds("Box").unwrap(), &[vec!["Num".to_string()]]);
    }

    #[test]
    fn combined_generic_param_bounds_are_recorded() {
        let ctx = context("struct Box[T: Num + Eq] { val: T }");
        assert_eq!(ctx.generic_bounds("Box").unwrap(), &[vec!["Num".to_string(), "Eq".to_string()]]);
    }

    #[test]
    fn unbounded_generic_param_has_an_empty_bound_list() {
        let ctx = context("struct Pair[T] { first: T, second: T }");
        assert_eq!(ctx.generic_bounds("Pair").unwrap(), &[Vec::<String>::new()]);
    }

    #[test]
    fn multiple_params_bounds_track_independently() {
        let ctx = context("struct Pair[A: Num, B] { first: A, second: B }");
        assert_eq!(
            ctx.generic_bounds("Pair").unwrap(),
            &[vec!["Num".to_string()], Vec::<String>::new()]
        );
    }

    #[test]
    fn resolves_enum_variant_payload_types_and_owning_enum() {
        let ctx = context("enum Shape { Circle(Float), Rectangle(Float, Float), Empty }");
        assert_eq!(
            ctx.variant("Circle").unwrap(),
            &("Shape".to_string(), vec![Type::Float])
        );
        assert_eq!(
            ctx.variant("Rectangle").unwrap(),
            &("Shape".to_string(), vec![Type::Float, Type::Float])
        );
        assert_eq!(ctx.variant("Empty").unwrap(), &("Shape".to_string(), vec![]));
    }

    #[test]
    fn unknown_variant_tag_is_none() {
        let ctx = context("enum Shape { Circle(Float) }");
        assert!(ctx.variant("Triangle").is_none());
    }

    #[test]
    fn generic_enum_payload_resolves_to_param_placeholders() {
        let ctx = context("enum Option[T] { Some(T), None }");
        assert_eq!(ctx.variant("Some").unwrap(), &("Option".to_string(), vec![Type::Param("T".to_string())]));
        assert_eq!(ctx.variant("None").unwrap(), &("Option".to_string(), vec![]));
        assert_eq!(ctx.generic_params("Option").unwrap(), &["T".to_string()]);
    }

    #[test]
    fn generic_enum_referencing_a_generic_struct_field() {
        let ctx = context("struct Point { x: Int, y: Int }\nenum Result[T, E] { Ok(T), Err(E) }");
        assert_eq!(ctx.variant("Ok").unwrap(), &("Result".to_string(), vec![Type::Param("T".to_string())]));
        assert_eq!(ctx.variant("Err").unwrap(), &("Result".to_string(), vec![Type::Param("E".to_string())]));
    }

    #[test]
    fn multiple_structs_and_enums_all_resolve() {
        let ctx = context(
            "struct Point { x: Float, y: Float }\n\
             enum Shape { Circle(Float) }\n\
             struct Color { r: Int, g: Int, b: Int }",
        );
        assert!(ctx.struct_fields("Point").is_some());
        assert!(ctx.struct_fields("Color").is_some());
        assert!(ctx.variant("Circle").is_some());
    }

    // --- A field/payload type that's itself a struct or enum ---

    #[test]
    fn struct_field_referencing_another_struct_resolves() {
        let ctx = context("struct Point { x: Int, y: Int }\nstruct Line { start: Point, end: Point }");
        assert_eq!(
            ctx.struct_fields("Line").unwrap(),
            &[
                ("start".to_string(), Type::Struct("Point".to_string(), vec![])),
                ("end".to_string(), Type::Struct("Point".to_string(), vec![])),
            ]
        );
    }

    #[test]
    fn struct_field_referencing_another_struct_works_regardless_of_declaration_order() {
        // `Line` declared BEFORE `Point` — phase 1 already collected
        // every name before phase 2 resolves any field type.
        let ctx = context("struct Line { start: Point, end: Point }\nstruct Point { x: Int, y: Int }");
        assert_eq!(
            ctx.struct_fields("Line").unwrap()[0],
            ("start".to_string(), Type::Struct("Point".to_string(), vec![]))
        );
    }

    #[test]
    fn mutually_recursive_struct_fields_resolve() {
        let ctx = context("struct A { b: B }\nstruct B { a: A }");
        assert_eq!(ctx.struct_fields("A").unwrap(), &[("b".to_string(), Type::Struct("B".to_string(), vec![]))]);
        assert_eq!(ctx.struct_fields("B").unwrap(), &[("a".to_string(), Type::Struct("A".to_string(), vec![]))]);
    }

    #[test]
    fn enum_payload_referencing_a_struct_resolves() {
        let ctx = context("struct Point { x: Int, y: Int }\nenum Shape { AtOrigin, At(Point) }");
        assert_eq!(ctx.variant("At").unwrap(), &("Shape".to_string(), vec![Type::Struct("Point".to_string(), vec![])]));
    }

    #[test]
    fn struct_field_referencing_an_enum_resolves() {
        let ctx = context("enum Shape { Circle(Float) }\nstruct Canvas { shape: Shape }");
        assert_eq!(
            ctx.struct_fields("Canvas").unwrap(),
            &[("shape".to_string(), Type::Enum("Shape".to_string(), vec![]))]
        );
    }

    #[test]
    fn struct_field_referencing_an_undeclared_type_is_still_an_error() {
        context_err("struct Line { start: Undeclared }");
    }

    // --- Duplicate declarations ---

    #[test]
    fn redeclaring_a_struct_is_an_error() {
        let err = context_err("struct Point { x: Int }\nstruct Point { y: Int }");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }

    #[test]
    fn redeclaring_an_enum_is_an_error() {
        let err = context_err("enum Shape { Circle(Float) }\nenum Shape { Square(Float) }");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }

    #[test]
    fn a_struct_and_an_enum_sharing_a_name_is_an_error() {
        let err = context_err("struct Thing { x: Int }\nenum Thing { A }");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }
}
