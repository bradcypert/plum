use crate::infer::ast_type_to_type;
use crate::types::Type;
use plum_syntax::ast;
use std::collections::{HashMap, HashSet};

/// Struct/enum declarations' field and variant payload types, resolved
/// once from a program's items before inferring anything that uses
/// them — the type-level counterpart to `lower.rs`'s `LoweringContext`
/// (which resolves field ORDER; this resolves field TYPES).
///
/// Deliberately rejects any struct/enum declared with generics, rather
/// than trying to erase them the way function generics are erased —
/// see types.rs's doc comment for why those two cases aren't
/// analogous.
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
}

impl TypeContext {
    pub fn new() -> Self {
        TypeContext {
            struct_fields: HashMap::new(),
            variants: HashMap::new(),
            struct_names: HashSet::new(),
            enum_names: HashSet::new(),
        }
    }

    pub fn from_items(items: &[ast::Item]) -> Result<Self, String> {
        let mut ctx = Self::new();

        // Phase 1: collect every declared name FIRST — see this
        // struct's doc comment on `struct_names`/`enum_names` for why.
        for item in items {
            match &item.kind {
                ast::ItemKind::Struct(decl) => {
                    ctx.struct_names.insert(decl.name.clone());
                }
                ast::ItemKind::Enum(decl) => {
                    ctx.enum_names.insert(decl.name.clone());
                }
                _ => {}
            }
        }

        // Phase 2: resolve field/payload types, now that EVERY name is
        // known — `ast_type_to_type(_, &ctx)` only ever needs
        // `struct_names`/`enum_names` (already fully populated), never
        // the `struct_fields`/`variants` maps still being filled in
        // here, so reading from `ctx` mid-construction is safe.
        for item in items {
            match &item.kind {
                ast::ItemKind::Struct(decl) => {
                    if !decl.generics.is_empty() {
                        return Err(format!(
                            "type inference not yet implemented for generic structs \
                             (no generic type formers yet) at {:?}",
                            decl.span
                        ));
                    }
                    let mut fields = Vec::with_capacity(decl.fields.len());
                    for f in &decl.fields {
                        fields.push((f.name.clone(), ast_type_to_type(&f.ty, &ctx)?));
                    }
                    ctx.struct_fields.insert(decl.name.clone(), fields);
                }
                ast::ItemKind::Enum(decl) => {
                    if !decl.generics.is_empty() {
                        return Err(format!(
                            "type inference not yet implemented for generic enums \
                             (no generic type formers yet) at {:?}",
                            decl.span
                        ));
                    }
                    for variant in &decl.variants {
                        let payload = variant
                            .payload
                            .iter()
                            .map(|t| ast_type_to_type(t, &ctx))
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
    fn generic_struct_declaration_is_rejected() {
        let err = context_err("struct Pair[T] { first: T, second: T }");
        assert!(err.contains("generic"), "expected a generic-related error, got: {err}");
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
    fn generic_enum_declaration_is_rejected() {
        let err = context_err("enum Option[T] { Some(T), None }");
        assert!(err.contains("generic"), "expected a generic-related error, got: {err}");
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
                ("start".to_string(), Type::Struct("Point".to_string())),
                ("end".to_string(), Type::Struct("Point".to_string())),
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
            ("start".to_string(), Type::Struct("Point".to_string()))
        );
    }

    #[test]
    fn mutually_recursive_struct_fields_resolve() {
        let ctx = context("struct A { b: B }\nstruct B { a: A }");
        assert_eq!(ctx.struct_fields("A").unwrap(), &[("b".to_string(), Type::Struct("B".to_string()))]);
        assert_eq!(ctx.struct_fields("B").unwrap(), &[("a".to_string(), Type::Struct("A".to_string()))]);
    }

    #[test]
    fn enum_payload_referencing_a_struct_resolves() {
        let ctx = context("struct Point { x: Int, y: Int }\nenum Shape { AtOrigin, At(Point) }");
        assert_eq!(ctx.variant("At").unwrap(), &("Shape".to_string(), vec![Type::Struct("Point".to_string())]));
    }

    #[test]
    fn struct_field_referencing_an_enum_resolves() {
        let ctx = context("enum Shape { Circle(Float) }\nstruct Canvas { shape: Shape }");
        assert_eq!(
            ctx.struct_fields("Canvas").unwrap(),
            &[("shape".to_string(), Type::Enum("Shape".to_string()))]
        );
    }

    #[test]
    fn struct_field_referencing_an_undeclared_type_is_still_an_error() {
        context_err("struct Line { start: Undeclared }");
    }
}
