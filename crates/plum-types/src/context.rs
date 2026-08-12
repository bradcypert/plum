use crate::infer::ast_type_to_type;
use crate::types::Type;
use plum_syntax::ast;
use plum_syntax::span::Span;
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
    // enum name -> every one of its declared variant tags, in
    // DECLARATION order — the reverse direction from `variants` (which
    // is keyed by tag, not enum name). Exists purely for match
    // exhaustiveness checking (`Infer::infer_match`'s "does every
    // declared variant have a covering arm" check) — nothing else
    // needs to enumerate ALL of an enum's tags at once.
    enum_variant_tags: HashMap<String, Vec<String>>,
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
    // Declared `extern "C"` function names -> (parameter types, return
    // type) — a void return (`ret_ty: None`) is stored as `Type::Unit`,
    // matching how an ordinary Plum function with no meaningful return
    // value is typed, even though `ir::ExternFn::ret_type` itself keeps
    // `None`/`Some` distinct (see that struct's doc comment for why the
    // IR layer needs the distinction and the type layer doesn't).
    // Populated in `from_items`'s Phase 1, alongside struct/enum names —
    // `Infer::infer_program` uses this to pre-declare each extern
    // function's signature into `global_env`, and `Infer::infer_expr`'s
    // `Call` handling consults it to enforce `unsafe`-gating (see
    // `Infer::in_unsafe`'s doc comment).
    extern_fns: HashMap<String, (Vec<Type>, Type)>,
    // Declaration-site spans, kept entirely SEPARATE from the maps
    // above rather than folded into them (e.g. `struct_fields` growing
    // a `Span` per entry) — purely for go-to-definition, consulted by
    // NOTHING else in ordinary type inference. Keeping them apart means
    // every existing `struct_fields`/`variant`/etc. caller (including
    // the ~15 existing tests asserting exact expected values) needed
    // zero changes to accommodate this — a genuinely additive
    // extension, not a signature change forcing a span onto call sites
    // that have no use for one.
    struct_decl_spans: HashMap<String, Span>,
    enum_decl_spans: HashMap<String, Span>,
    // (struct name, field name) -> the field's OWN declaration span
    // (not the whole struct's).
    struct_field_spans: HashMap<(String, String), Span>,
    // variant tag -> its own declaration span (not the whole enum's).
    variant_spans: HashMap<String, Span>,
}

impl TypeContext {
    pub fn new() -> Self {
        TypeContext {
            struct_fields: HashMap::new(),
            variants: HashMap::new(),
            enum_variant_tags: HashMap::new(),
            struct_names: HashSet::new(),
            enum_names: HashSet::new(),
            generic_params: HashMap::new(),
            generic_bounds: HashMap::new(),
            extern_fns: HashMap::new(),
            struct_decl_spans: HashMap::new(),
            enum_decl_spans: HashMap::new(),
            struct_field_spans: HashMap::new(),
            variant_spans: HashMap::new(),
        }
    }

    pub fn from_items(items: &[ast::Item]) -> Result<Self, plum_syntax::error::CompileError> {
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
                        return Err(plum_syntax::error::CompileError::new(
                            decl.span,
                            format!("{:?} is already declared", decl.name),
                        ));
                    }
                    ctx.struct_names.insert(decl.name.clone());
                    ctx.struct_decl_spans.insert(decl.name.clone(), decl.span);
                    let params = decl.generics.iter().map(|g| g.name.clone()).collect();
                    ctx.generic_params.insert(decl.name.clone(), params);
                    let bounds = decl.generics.iter().map(|g| g.bound.clone()).collect();
                    ctx.generic_bounds.insert(decl.name.clone(), bounds);
                }
                ast::ItemKind::Enum(decl) => {
                    if ctx.struct_names.contains(&decl.name) || ctx.enum_names.contains(&decl.name) {
                        return Err(plum_syntax::error::CompileError::new(
                            decl.span,
                            format!("{:?} is already declared", decl.name),
                        ));
                    }
                    ctx.enum_names.insert(decl.name.clone());
                    ctx.enum_decl_spans.insert(decl.name.clone(), decl.span);
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
                        ctx.struct_field_spans.insert((decl.name.clone(), f.name.clone()), f.span);
                    }
                    ctx.struct_fields.insert(decl.name.clone(), fields);
                }
                ast::ItemKind::Enum(decl) => {
                    let params: Vec<String> = decl.generics.iter().map(|g| g.name.clone()).collect();
                    let mut tags = Vec::with_capacity(decl.variants.len());
                    for variant in &decl.variants {
                        let payload = variant
                            .payload
                            .iter()
                            .map(|t| ast_type_to_type(t, &ctx, &params))
                            .collect::<Result<Vec<_>, _>>()?;
                        ctx.variants.insert(variant.name.clone(), (decl.name.clone(), payload));
                        ctx.variant_spans.insert(variant.name.clone(), variant.span);
                        tags.push(variant.name.clone());
                    }
                    ctx.enum_variant_tags.insert(decl.name.clone(), tags);
                }
                _ => {}
            }
        }

        // Phase 3: extern blocks, now that every struct's field TYPES
        // (not just names) are resolved — needed for the struct-FFI-
        // safety check (does every field resolve to Int/Float/Bool/
        // CStr, or another FFI-safe struct?), which Phase 1 couldn't
        // do yet.
        for item in items {
            if let ast::ItemKind::Extern(block) = &item.kind {
                for f in &block.fns {
                    let param_types = f
                        .params
                        .iter()
                        .map(|p| extern_ast_type_to_type(&p.ty, &ctx))
                        .collect::<Result<Vec<_>, _>>()?;
                    let ret_type = match &f.ret_ty {
                        // `-> Unit` written explicitly means exactly
                        // the same thing as omitting the arrow — void
                        // — matching the SAME allowance a callback's
                        // own return position already has (see this
                        // function's `check_ffi_safe` call below, whose
                        // `Type::Function` arm already treats `Unit`
                        // as valid specifically in return position, not
                        // as an FFI-safe scalar in general). Checked
                        // here, before ever calling `extern_ast_type_
                        // to_type` (which would otherwise reject `Unit`
                        // outright — it's not Int/Float/Bool/CStr/a
                        // qualifying struct), so `Unit`/no-arrow are
                        // truly indistinguishable from this point on,
                        // not two paths that happen to agree.
                        Some(ast::Type::Path(segments, _)) if segments.len() == 1 && segments[0] == "Unit" => Type::Unit,
                        Some(t) => extern_ast_type_to_type(t, &ctx)?,
                        None => Type::Unit,
                    };
                    // Calling a C-supplied function pointer FROM Plum
                    // (as opposed to passing a Plum function TO C,
                    // which the `Callback` case above supports) isn't
                    // implemented — a callback type is only meaningful
                    // as a PARAMETER.
                    if matches!(ret_type, Type::Function(..)) {
                        return Err(plum_syntax::error::CompileError::new(
                            f.span,
                            format!(
                                "extern function {:?}: a callback type is only supported as a parameter, not a \
                                 return type (calling a C-supplied function pointer isn't implemented)",
                                f.name
                            ),
                        ));
                    }
                    ctx.extern_fns.insert(f.name.clone(), (param_types, ret_type));
                }
            }
        }
        Ok(ctx)
    }

    pub fn struct_fields(&self, name: &str) -> Option<&[(String, Type)]> {
        self.struct_fields.get(name).map(|v| v.as_slice())
    }

    /// The declaration span of `name` (a struct OR an enum — checked in
    /// that order, though the two namespaces are already guaranteed
    /// disjoint by `from_items`'s own collision check), for go-to-
    /// definition on a type-position reference or a `StructLiteral`/
    /// construction-site path. `None` for a name that isn't a declared
    /// struct/enum at all (a type variable, an unresolvable typo, ...).
    pub fn type_decl_span(&self, name: &str) -> Option<Span> {
        self.struct_decl_spans.get(name).or_else(|| self.enum_decl_spans.get(name)).copied()
    }

    /// The declaration span of struct `struct_name`'s field
    /// `field_name` — for go-to-definition on a `.field` access.
    pub fn struct_field_span(&self, struct_name: &str, field_name: &str) -> Option<Span> {
        self.struct_field_spans.get(&(struct_name.to_string(), field_name.to_string())).copied()
    }

    /// The declaration span of enum variant tag `tag` — for go-to-
    /// definition on a construction site (`Circle(1.0)`) or a bare
    /// variant reference/pattern (`None`, `Circle(r)` in a `match`).
    pub fn variant_span(&self, tag: &str) -> Option<Span> {
        self.variant_spans.get(tag).copied()
    }

    pub fn variant(&self, tag: &str) -> Option<&(String, Vec<Type>)> {
        self.variants.get(tag)
    }

    /// Every one of `name`'s declared variant TAGS, in declaration
    /// order — `None` only if `name` isn't a declared enum at all.
    /// `Some(&[])` is possible too (`enum Empty {}` parses, zero
    /// variants — trivially exhaustive, any match against it needs no
    /// arms at all to be exhaustive).
    pub(crate) fn enum_variant_tags(&self, name: &str) -> Option<&[String]> {
        self.enum_variant_tags.get(name).map(|v| v.as_slice())
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

    /// `name`'s declared `extern "C"` signature, if `name` was declared
    /// in some `extern "C" { .. }` block — see `extern_fns`'s doc
    /// comment.
    pub(crate) fn extern_fn(&self, name: &str) -> Option<&(Vec<Type>, Type)> {
        self.extern_fns.get(name)
    }

    /// `name`'s declared fields, with every occurrence of one of its OWN
    /// generic parameters substituted for the matching entry in
    /// `concrete_args` (positional, same order as `generic_params`) —
    /// the monomorphization-time counterpart to `Infer::instantiate_generic`,
    /// minus the fresh-var-minting half: by the time
    /// `plum_ir::monomorphize::plan` calls this, `concrete_args` are
    /// already fully concrete (or an internal error further up already
    /// caught the alternative), so there's nothing left to instantiate,
    /// just to substitute. `None` if `name` isn't a declared struct, or
    /// if `concrete_args`'s length doesn't match `name`'s own declared
    /// arity.
    pub fn struct_fields_for(&self, name: &str, concrete_args: &[Type]) -> Option<Vec<(String, Type)>> {
        let declared_fields = self.struct_fields(name)?;
        let param_names = self.generic_params(name)?;
        if param_names.len() != concrete_args.len() {
            return None;
        }
        let mapping: HashMap<String, Type> =
            param_names.iter().cloned().zip(concrete_args.iter().cloned()).collect();
        Some(
            declared_fields
                .iter()
                .map(|(field_name, ty)| (field_name.clone(), crate::infer::subst_params(ty, &mapping)))
                .collect(),
        )
    }

    /// `tag`'s owning enum name and declared payload types, with every
    /// occurrence of the OWNING ENUM's generic parameters substituted
    /// for `concrete_args` — see `struct_fields_for`'s doc comment for
    /// the full reasoning (same mechanism, keyed by variant tag instead
    /// of struct name, matching `variant`'s own indexing). `None` if
    /// `tag` isn't a declared variant, or if `concrete_args`'s length
    /// doesn't match the owning enum's declared arity.
    pub fn variant_payload_for(&self, tag: &str, concrete_args: &[Type]) -> Option<(String, Vec<Type>)> {
        let (enum_name, payload) = self.variant(tag)?;
        let param_names = self.generic_params(enum_name)?;
        if param_names.len() != concrete_args.len() {
            return None;
        }
        let mapping: HashMap<String, Type> =
            param_names.iter().cloned().zip(concrete_args.iter().cloned()).collect();
        let substituted = payload.iter().map(|ty| crate::infer::subst_params(ty, &mapping)).collect();
        Some((enum_name.clone(), substituted))
    }
}

// `Int`/`Float`/`Bool`/`CStr`, or a struct made ENTIRELY (recursively)
// of those types (see `ir::ExternType`'s doc comment in plum-ir — this
// mirrors that same scope at the type level, independently — same "no
// shared representation between crates" precedent as everywhere else).
// `CStr` is special-cased BEFORE delegating to `ast_type_to_type`: it's
// only ever meaningful in an extern signature (produced by `.as_cstr()`
// alone, never a real Plum type annotation elsewhere), so it isn't
// something the general resolver knows about. Everything else —
// including struct/enum names — DOES go through `ast_type_to_type`
// (the same resolver struct/enum fields use), reusing its name
// resolution rather than re-implementing it, with `check_ffi_safe`
// afterward rejecting whatever isn't actually FFI-safe.
fn extern_ast_type_to_type(ty: &ast::Type, ctx: &TypeContext) -> Result<Type, plum_syntax::error::CompileError> {
    if let ast::Type::Path(segments, _) = ty {
        if segments.len() == 1 && segments[0] == "CStr" {
            return Ok(Type::CStr);
        }
    }
    let resolved = ast_type_to_type(ty, ctx, &[])?;
    check_ffi_safe(&resolved, ctx, true, true, &mut HashSet::new())
        .map_err(|e| plum_syntax::error::CompileError::new(ty.span(), e))?;
    Ok(resolved)
}

// See `extern_ast_type_to_type`'s doc comment. `in_progress` guards
// against a self-referential struct (`struct Node { next: Node }` —
// perfectly legal ordinary Plum, since every field is a heap-boxed
// `Value`, never inline) recursing forever trying to prove something
// that, for a C by-value layout, can never be true: an infinite type
// has no finite size. `allow_cstr`/`allow_callback` are `true` only at
// the top level (a function's OWN param/return type) — never while
// recursing into a struct's fields or a callback's OWN params/return,
// matching `lower.rs`'s identical, independently-duplicated
// restrictions (a nested `CStr` field would need a nested owning
// `CString` buffer kept alive through marshaling; a nested callback
// has no realistic C ABI use case worth the added complexity — both
// out of `plum-interp`'s v1 scope).
fn check_ffi_safe(
    ty: &Type,
    ctx: &TypeContext,
    allow_cstr: bool,
    allow_callback: bool,
    in_progress: &mut HashSet<String>,
) -> Result<(), String> {
    match ty {
        Type::Int | Type::Float | Type::Bool => Ok(()),
        Type::CStr if allow_cstr => Ok(()),
        Type::CStr => Err("CStr is only supported as a top-level extern parameter/return type, not inside \
             a struct field"
            .to_string()),
        Type::Function(params, ret) if allow_callback => {
            for p in params {
                check_ffi_safe(p, ctx, false, false, in_progress)
                    .map_err(|e| format!("callback parameter: {e}"))?;
            }
            // `Unit` (`-> Unit`) is the callback's spelling of "void",
            // mirroring `lower.rs`'s identical special case — not
            // itself an FFI-safe SCALAR type, but valid specifically in
            // a callback's own return position.
            if matches!(ret.as_ref(), Type::Unit) {
                return Ok(());
            }
            check_ffi_safe(ret, ctx, false, false, in_progress).map_err(|e| format!("callback return: {e}"))
        }
        Type::Function(..) => Err(
            "a callback type is only supported as a top-level extern parameter, not nested inside a struct \
             field or another callback"
                .to_string(),
        ),
        Type::Struct(name, args) if args.is_empty() => {
            if !in_progress.insert(name.clone()) {
                return Err(format!("extern struct type {name:?} is self-referential, which isn't FFI-safe"));
            }
            let fields = ctx
                .struct_fields(name)
                .ok_or_else(|| format!("internal error: extern struct type {name:?} not in context"))?;
            for (_, field_ty) in fields {
                check_ffi_safe(field_ty, ctx, false, false, in_progress)
                    .map_err(|e| format!("extern struct type {name:?}: {e}"))?;
            }
            in_progress.remove(name);
            Ok(())
        }
        other => Err(format!(
            "extern functions only support Int, Float, Bool, CStr, a callback type, or a struct made \
             entirely of those types, found {other:?}"
        )),
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
        TypeContext::from_items(&program.items)
            .expect_err(&format!("expected context building for {src:?} to fail"))
            .to_string()
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
    fn enum_variant_tags_lists_every_tag_in_declaration_order() {
        let ctx = context("enum Shape { Circle(Float), Rectangle(Float, Float), Empty }");
        assert_eq!(
            ctx.enum_variant_tags("Shape").unwrap(),
            &["Circle".to_string(), "Rectangle".to_string(), "Empty".to_string()]
        );
    }

    #[test]
    fn unknown_enum_name_has_no_variant_tags() {
        let ctx = context("enum Shape { Circle(Float) }");
        assert!(ctx.enum_variant_tags("NotAnEnum").is_none());
    }

    #[test]
    fn a_struct_name_has_no_variant_tags() {
        let ctx = context("struct Point { x: Int, y: Int }");
        assert!(ctx.enum_variant_tags("Point").is_none());
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

    #[test]
    fn an_explicit_unit_return_type_means_the_same_thing_as_omitting_the_arrow() {
        let ctx = context("extern \"C\" { fn no_arrow(x: Int); fn explicit_unit(x: Int) -> Unit; }");
        assert_eq!(ctx.extern_fn("no_arrow").unwrap(), &(vec![Type::Int], Type::Unit));
        assert_eq!(ctx.extern_fn("explicit_unit").unwrap(), &(vec![Type::Int], Type::Unit));
    }
}
