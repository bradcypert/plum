use crate::ir;
use plum_syntax::ast;
use std::collections::HashMap;

/// A minimal symbol table, built from a program's `struct` declarations
/// before lowering any expressions that use them. Needed because
/// struct literals are named-field (`Point { y: 2.0, x: 1.0 }`, any
/// order) but the IR's `Ctor` is positional (matching Perceus's own
/// minimal core calculus) — resolving "what position does field `y`
/// go in" requires knowing the struct's DECLARED field order, which a
/// single expression can't know about in isolation. This is the first
/// place lowering needs to be program-aware rather than purely
/// per-expression.
#[derive(Clone)]
pub struct LoweringContext {
    struct_fields: HashMap<String, Vec<String>>,
    // Variant tag -> arity. Needed to lower a variant CONSTRUCTION
    // expression (`Circle(1.0)`, `Shape.Circle(1.0)`, or a bare `None`)
    // into a `Ctor`, the same way `struct_fields` is needed to lower a
    // struct literal — see `lower_expr`'s `Call`/`Ident` handling.
    // Just like pattern lowering already does for `Shape.Circle(r)`
    // (see `lower_tag_pattern`), the qualifier before `.` is never
    // validated against the variant's REAL owning enum — tags are
    // looked up by name alone, matching that established precedent.
    variants: HashMap<String, usize>,
    // `Expr::Field` span -> owning struct name, exactly as
    // `plum_types::Infer::field_owners` recorded it during inference.
    // Lowering has NO type information of its own (see this struct's
    // own doc comment on why it only ever resolves field ORDER, not
    // field TYPES) — a plain `p.x` doesn't say what struct `p` is, so
    // without this side-channel there'd be no way to know which
    // struct's declared field order to index into. Empty by default
    // (`new`/`from_items`): only `plumc`'s real pipeline — which runs
    // type inference BEFORE lowering — ever populates it via
    // `with_field_owners`. A program with no field access at all never
    // needs it; a lowering-only test that DOES use `p.x` without
    // populating this map gets a clear "unresolved field access"
    // error, not a wrong/guessed answer.
    field_owners: HashMap<plum_syntax::span::Span, String>,
    // `for` loops (keyed by the `Expr::For` node's own span) whose
    // iterand is `Array[T]` rather than `Range`, exactly as
    // `plum_types::Infer::array_for_loops` recorded it during
    // inference — see that field's doc comment. Empty by default: a
    // lowering-only test that never populates this (and never uses
    // `for x in arr`) is unaffected, since the literal-range fast path
    // and the ordinary Range-Match-unwrap fallback don't consult it.
    array_for_loops: std::collections::HashSet<plum_syntax::span::Span>,
    // `Expr::ArrayLiteral` span -> resolved element type, but ONLY for
    // EMPTY literals (`[]`) — exactly as `plum_types::Infer` resolved it
    // after inference completes. Every OTHER array-shaped expression
    // (a non-empty literal, any array op with a value operand, reading
    // an already-known array) derives its element type structurally at
    // CODEGEN time, with no lowering-level side channel needed at all —
    // see `plum-codegen`'s module doc comment for the full "how array
    // element CgType is determined" story. `[]` is the one genuine gap:
    // there's no field for codegen to derive a type FROM, so lowering
    // needs this table (mirroring `field_owners`/`array_for_loops`
    // exactly) to bake the resolved type into a dedicated `ir::Expr::
    // EmptyArray(PrimTy)` node instead of an ordinary, type-erased
    // `Ctor`. Empty by default: a lowering-only test that never
    // populates this (and never lowers an empty array literal) is
    // unaffected — see `lower_expr`'s `ArrayLiteral` arm for the clear
    // error an unpopulated lookup produces instead of guessing.
    empty_array_elem_types: HashMap<plum_syntax::span::Span, plum_types::types::Type>,
    // `Expr::Closure` span -> its resolved (param types, return type),
    // exactly as `plum_types::Infer::resolve_closure_types` computed
    // it — the closure-literal counterpart to `empty_array_elem_types`
    // above, needed for the SAME reason: unlike a `Ctor` or ordinary
    // arithmetic expression, a closure's own PARAMETERS have nothing
    // for `plum-codegen` to derive a type from structurally (a bare
    // `|x| x + 1` never reveals `x`'s type from the AST alone), so
    // lowering has to bake inference's answer into the IR node itself.
    // Empty by default: a lowering-only test that never populates this
    // (this crate's own unit tests, which construct expected `ir::Expr::
    // Closure` values directly against a bare `LoweringContext::new()`)
    // just gets `None` on both fields of the produced node — harmless,
    // since `plum-interp` (which lowers WITHOUT ever needing static
    // types) never reads them, and only `plum-codegen` requires `Some`.
    closure_types: HashMap<plum_syntax::span::Span, (Vec<plum_types::types::Type>, plum_types::types::Type)>,
    // Enum variant tag -> its declared payload types, resolved via
    // `plum_types::context::TypeContext::variant` — needed ONLY for a
    // bare non-zero-arity variant reference used as a VALUE (`let f =
    // Circle; f(1.0)`), which `lower_expr`'s `Ident` arm eta-expands
    // into a synthetic `Closure` (see that arm's own doc comment): that
    // synthetic closure's params need real types too, for exactly the
    // same reason an ordinary user-written closure literal does, but
    // there's no `Expr::Closure` AST SPAN to key `closure_types` by
    // (the eta-expansion happens for a bare `Ident`, not a real closure
    // literal) — so this is keyed by variant NAME instead. Empty by
    // default, same harmless-`None`-fallback story as `closure_types`.
    variant_payload_types: HashMap<String, Vec<plum_types::types::Type>>,
    // Declared `extern "C"` function names -> their signatures, gathered
    // from every `ItemKind::Extern` block up front (same "program-aware
    // pass before lowering expressions" reasoning as `struct_fields`).
    // Used two ways: `lower_program` turns each entry into an
    // `ir::ExternFn` in `Program.externs`, and `lower_expr`'s `Call`
    // handling consults just the name set to recognize `sqrt(2.0)` as
    // an `ExternCall` rather than an ordinary `Call` — the same
    // "shape-detected, not a real grammar production" precedent as
    // `variants`.
    extern_fns: HashMap<String, ir::ExternFn>,
    // A struct's field types, in DECLARATION order — a separate table
    // from `struct_fields` (which only ever needed NAMES, for literal
    // field-order resolution). Needed ONLY for resolving a struct-typed
    // extern parameter/return recursively (does every field resolve to
    // an FFI-safe type?) — see `resolve_extern_type`'s doc comment. No
    // other lowering path needs a struct's field TYPES at all, which is
    // why this wasn't already part of `struct_fields`.
    struct_field_types: HashMap<String, Vec<ast::Type>>,
}

impl LoweringContext {
    pub fn new() -> Self {
        LoweringContext {
            struct_fields: HashMap::new(),
            variants: HashMap::new(),
            field_owners: HashMap::new(),
            array_for_loops: std::collections::HashSet::new(),
            empty_array_elem_types: HashMap::new(),
            closure_types: HashMap::new(),
            variant_payload_types: HashMap::new(),
            extern_fns: HashMap::new(),
            struct_field_types: HashMap::new(),
        }
    }

    /// Attaches the span -> owning-struct-name map `plum-types`
    /// computed during inference — see `field_owners`'s doc comment.
    pub fn with_field_owners(mut self, field_owners: HashMap<plum_syntax::span::Span, String>) -> Self {
        self.field_owners = field_owners;
        self
    }

    /// Attaches the set of array-typed `for` loops `plum-types`
    /// computed during inference — see `array_for_loops`'s doc comment.
    pub fn with_array_for_loops(mut self, array_for_loops: std::collections::HashSet<plum_syntax::span::Span>) -> Self {
        self.array_for_loops = array_for_loops;
        self
    }

    /// Attaches the span -> resolved-element-type map for EMPTY array
    /// literals `plum-types` computed during inference — see
    /// `empty_array_elem_types`'s doc comment.
    pub fn with_empty_array_elem_types(
        mut self,
        empty_array_elem_types: HashMap<plum_syntax::span::Span, plum_types::types::Type>,
    ) -> Self {
        self.empty_array_elem_types = empty_array_elem_types;
        self
    }

    /// Attaches the span -> resolved (param types, return type) map for
    /// closure LITERALS `plum-types` computed during inference — see
    /// `closure_types`'s doc comment.
    pub fn with_closure_types(
        mut self,
        closure_types: HashMap<plum_syntax::span::Span, (Vec<plum_types::types::Type>, plum_types::types::Type)>,
    ) -> Self {
        self.closure_types = closure_types;
        self
    }

    /// Attaches the variant-tag -> payload-types map used to type a
    /// bare variant reference's eta-expanded synthetic closure — see
    /// `variant_payload_types`'s doc comment.
    pub fn with_variant_payload_types(mut self, variant_payload_types: HashMap<String, Vec<plum_types::types::Type>>) -> Self {
        self.variant_payload_types = variant_payload_types;
        self
    }

    /// Adds extra variant-tag -> arity entries on top of whatever
    /// `from_items` already collected — `plum_ir::monomorphize` uses
    /// this to register each MANGLED variant tag it produces (`Cons` ->
    /// `Cons$Int`) at the SAME arity as the original, unmangled tag, so
    /// `lower_expr`/`lower_tag_pattern`'s ordinary `ctx.variants` lookup
    /// (which only ever sees the mangled name after monomorphization's
    /// own AST rewrite) resolves correctly without lower.rs itself
    /// needing to know anything about mangling.
    pub fn with_extra_variants(mut self, extra: HashMap<String, usize>) -> Self {
        self.variants.extend(extra);
        self
    }

    /// Same as `with_extra_variants`, for mangled STRUCT tags — see that
    /// method's doc comment. Field NAMES (not types — lowering only
    /// ever needs field ORDER) are identical between a struct and every
    /// one of its monomorphized instantiations, so `monomorphize`
    /// registers the SAME name list under each mangled tag it produces.
    pub fn with_extra_struct_fields(mut self, extra: HashMap<String, Vec<String>>) -> Self {
        self.struct_fields.extend(extra);
        self
    }

    /// Every struct tag's field names, in DECLARED order — the same
    /// table `lower_struct_literal` already uses to put a literal's
    /// fields into positional `Ctor` order, exposed read-only for
    /// callers (`plum-interp`'s `.to_string()` rendering) that need
    /// struct field NAMES at runtime, which the lowered `ir::Program`
    /// itself never carries (see `ir.rs`'s `ToString` doc comment).
    pub fn struct_fields(&self) -> &HashMap<String, Vec<String>> {
        &self.struct_fields
    }

    pub fn from_items(items: &[ast::Item]) -> Self {
        let mut ctx = Self::new();
        // Pass 1: struct/enum names, fields, and field TYPES — done
        // first and unconditionally, since Pass 2's extern-signature
        // resolution (specifically the struct-FFI-safety check) needs
        // every struct's field types already known, regardless of
        // declaration order relative to the `extern` block that uses
        // one.
        for item in items {
            match &item.kind {
                ast::ItemKind::Struct(decl) => {
                    let fields = decl.fields.iter().map(|f| f.name.clone()).collect();
                    ctx.struct_fields.insert(decl.name.clone(), fields);
                    let field_types = decl.fields.iter().map(|f| f.ty.clone()).collect();
                    ctx.struct_field_types.insert(decl.name.clone(), field_types);
                }
                ast::ItemKind::Enum(decl) => {
                    for variant in &decl.variants {
                        ctx.variants.insert(variant.name.clone(), variant.payload.len());
                    }
                }
                _ => {}
            }
        }
        // Pass 2: extern blocks, now that every struct's field types are
        // known.
        for item in items {
            match &item.kind {
                ast::ItemKind::Extern(block) => {
                    for f in &block.fns {
                        // Resolution errors (an unsupported param/return
                        // type) surface later, from `lower_program`'s own
                        // pass over the same blocks — see its doc
                        // comment. `from_items` only builds LOOKUP
                        // tables, it never returns `Result`, so a bad
                        // type here is silently skipped rather than
                        // reported twice.
                        if let Ok(extern_fn) = resolve_extern_fn(f, &ctx) {
                            ctx.extern_fns.insert(f.name.clone(), extern_fn);
                        }
                    }
                }
                _ => {}
            }
        }
        ctx
    }
}

// `Int`/`Float`/`Bool`/`CStr`, or a struct made ENTIRELY (recursively)
// of those types — see ir.rs's `ExternType` doc comment. Anything else,
// including an unrecognized path, a generic type, an enum, or a
// self-referential struct, is rejected with a clear error rather than
// silently guessed at. `in_progress` is the self-reference guard: a
// struct field can never point back to a struct already being
// resolved higher up this same recursive call chain — without this,
// `struct Node { next: Node }` (perfectly legal ordinary Plum, since
// every field is a heap `Value` indirection, never inline) would
// recurse forever trying to flatten an infinitely-nested C layout.
fn resolve_extern_type(ty: &ast::Type, ctx: &LoweringContext) -> Result<ir::ExternType, plum_syntax::error::CompileError> {
    resolve_extern_type_inner(ty, ctx, true, true, &mut std::collections::HashSet::new())
}

// `allow_cstr` is `true` only for a function's OWN param/return type,
// never while recursing into a struct's fields — a `CStr` field inside
// a by-value C struct would mean keeping a nested owning `CString`
// buffer alive for the duration of a call nested inside another
// marshaling step, which `plum-interp`'s marshaling code doesn't
// support (a struct field is scoped to Int/Float/Bool/nested-struct
// only). This mirrors `plum-types::context::check_ffi_safe`'s
// identical, independently-duplicated restriction.
// `allow_callback` is `true` only for a function's OWN param/return
// type, never while recursing into a struct's fields OR a callback's
// OWN params/return — no nested callbacks (a callback param/return is
// scoped to Int/Float/Bool only, same narrow set as a struct field, for
// the same "keep marshaling tractable" reason).
fn resolve_extern_type_inner(
    ty: &ast::Type,
    ctx: &LoweringContext,
    allow_cstr: bool,
    allow_callback: bool,
    in_progress: &mut std::collections::HashSet<String>,
) -> Result<ir::ExternType, plum_syntax::error::CompileError> {
    if let ast::Type::Function { params, ret, span } = ty {
        if !allow_callback {
            return Err(plum_syntax::error::CompileError::new(
                *span,
                "a callback type is only supported as a top-level extern parameter, not nested inside a \
                 struct field or another callback",
            ));
        }
        let param_types = params
            .iter()
            .map(|p| resolve_extern_type_inner(p, ctx, false, false, in_progress))
            .collect::<Result<Vec<_>, _>>()?;
        let ret_type = match ret.as_ref() {
            ast::Type::Path(segments, _) if segments.len() == 1 && segments[0] == "Unit" => None,
            other => Some(Box::new(resolve_extern_type_inner(other, ctx, false, false, in_progress)?)),
        };
        return Ok(ir::ExternType::Callback {
            params: param_types,
            ret: ret_type,
        });
    }
    let ast::Type::Path(segments, span) = ty else {
        return Err(plum_syntax::error::CompileError::new(
            ty.span(),
            format!("extern functions only support Int, Float, Bool, CStr, or a struct made entirely of those types, found {ty:?}"),
        ));
    };
    if segments.len() != 1 {
        return Err(plum_syntax::error::CompileError::new(
            *span,
            format!("extern functions only support Int, Float, Bool, CStr, or a struct made entirely of those types, found {ty:?}"),
        ));
    }
    match segments[0].as_str() {
        "Int" => Ok(ir::ExternType::Int),
        "Float" => Ok(ir::ExternType::Float),
        "Bool" => Ok(ir::ExternType::Bool),
        "CStr" if allow_cstr => Ok(ir::ExternType::Str),
        "CStr" => Err(plum_syntax::error::CompileError::new(
            *span,
            "CStr is only supported as a top-level extern parameter/return type, not inside a struct field",
        )),
        name => {
            let Some(field_types) = ctx.struct_field_types.get(name) else {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    format!("extern functions only support Int, Float, Bool, CStr, or a struct made entirely of those types, found {name:?}"),
                ));
            };
            if !in_progress.insert(name.to_string()) {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    format!("extern struct type {name:?} is self-referential, which isn't FFI-safe"),
                ));
            }
            let resolved = field_types
                .iter()
                .map(|t| resolve_extern_type_inner(t, ctx, false, false, in_progress))
                .collect::<Result<Vec<_>, _>>();
            in_progress.remove(name);
            Ok(ir::ExternType::Struct(name.to_string(), resolved?))
        }
    }
}

fn resolve_extern_fn(f: &ast::ExternFn, ctx: &LoweringContext) -> Result<ir::ExternFn, plum_syntax::error::CompileError> {
    let param_types = f
        .params
        .iter()
        .map(|p| resolve_extern_type(&p.ty, ctx))
        .collect::<Result<_, _>>()?;
    let ret_type = f.ret_ty.as_ref().map(|t| resolve_extern_type(t, ctx)).transpose()?;
    // Calling a C-supplied function pointer FROM Plum isn't
    // implemented — a callback type is only meaningful as a PARAMETER
    // (Plum passing a function TO C), matching `plum-types`'
    // independently-duplicated identical restriction.
    if matches!(ret_type, Some(ir::ExternType::Callback { .. })) {
        return Err(plum_syntax::error::CompileError::new(
            f.span,
            format!(
                "extern function {:?}: a callback type is only supported as a parameter, not a return type \
                 (calling a C-supplied function pointer isn't implemented)",
                f.name
            ),
        ));
    }
    Ok(ir::ExternFn {
        name: f.name.clone(),
        param_types,
        ret_type,
    })
}

impl Default for LoweringContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Lowers a whole program's `let`-defined items: 1+ parameters becomes
/// an `ir::Function`, zero parameters becomes an `ir::Global` (a plain,
/// eagerly-evaluated value — see ir.rs's `Global` doc comment). A
/// top-level `let` always binds a single NAME (`ast::LetDef.name` is a
/// bare `String`, never a general `Pattern`), so — unlike block-level
/// `let` or function params — there's no destructuring case to reject
/// here for globals specifically; destructuring FUNCTION params is
/// still its own, separate restriction (see `lower_params`). Generics
/// are simply IGNORED, not rejected — see ir.rs's `Function` doc
/// comment: a type parameter has no runtime effect without a type
/// checker, so this is deliberate erasure.
pub fn lower_program(program: &ast::Program, ctx: &LoweringContext) -> Result<ir::Program, plum_syntax::error::CompileError> {
    let mut functions = Vec::new();
    let mut globals = Vec::new();
    let mut externs = Vec::new();
    for item in &program.items {
        if let ast::ItemKind::Extern(block) = &item.kind {
            for f in &block.fns {
                externs.push(resolve_extern_fn(f, ctx)?);
            }
        }
    }
    for item in &program.items {
        if let ast::ItemKind::Let(def) = &item.kind {
            if def.params.is_empty() {
                // A "global" — see ir.rs's `Global` doc comment for why
                // order among globals matters and functions don't need
                // any special ordering relative to them.
                globals.push(ir::Global {
                    name: def.name.clone(),
                    value: lower_expr(&def.body, ctx)?,
                });
                continue;
            }
            let (params, destructures) = lower_params(&def.params)?;
            let mut body = lower_expr(&def.body, ctx)?;
            for (synthetic, pattern) in destructures.into_iter().rev() {
                body = wrap_destructure(synthetic, &pattern, ctx, body)?;
            }
            functions.push(ir::Function {
                name: def.name.clone(),
                params,
                body,
            });
        }
        // struct/enum/extern/use declarations don't produce runtime
        // functions — they're consumed elsewhere (LoweringContext) or
        // not consumed at all yet (extern, use).
    }
    Ok(ir::Program { functions, globals, externs })
}

// A tag no user struct/enum declaration can ever produce (identifiers
// can't start with a digit — see plum-syntax's `is_ident_start`), so a
// tuple's synthetic tag can never collide with a real type name.
fn tuple_tag(arity: usize) -> String {
    format!("{arity}Tuple")
}

// A `start..end` Range's synthetic tag, used the same way `tuple_tag`
// is: a leading digit makes it unreachable by any real identifier
// (`is_ident_start` forbids one), so it can never collide with a
// user's own `struct Range { .. }` or `enum Range { .. }` — unlike
// `"Range"` alone, which genuinely could.
const RANGE_TAG: &str = "0Range";

// An `Array[T]` literal's synthetic tag — same leading-digit,
// unreachable-by-any-real-identifier trick as `RANGE_TAG`/`tuple_tag`.
const ARRAY_TAG: &str = "0Array";

// Converts a fully-resolved `plum_types::Type` into `ir::PrimTy` — used
// ONLY for an empty array literal's element type (see `ir::Expr::
// EmptyArray`'s doc comment): every other array/heap-typed site in this
// IR stays type-erased (`Ctor`'s tag alone is enough, `plum-codegen`
// derives everything else structurally), so this is deliberately the
// ONE place lowering ever needs to look inside a `plum_types::Type` at
// all. Any struct/enum instantiation (generic or not) collapses to
// `PrimTy::Heap`, mirroring `CgType::Heap`'s own "opaque ptr" treatment
// — an empty array can't reveal a specific tag to preserve even if this
// tried to. A type this can't represent (a bare type variable that
// somehow survived resolution, `Unit`, `CStr`, a function type, ...) is
// a clear error, never a silent guess.
/// Whether `ty` still contains a `Type::Param` TEMPLATE anywhere inside
/// it — see this function's one call site (the `Closure` arm below) for
/// why that needs different handling than every other still-unresolved
/// shape `type_to_prim_ty` already rejects outright.
fn type_contains_param(ty: &plum_types::types::Type) -> bool {
    use plum_types::types::Type;
    match ty {
        Type::Param(_) => true,
        Type::Function(params, ret) => params.iter().any(type_contains_param) || type_contains_param(ret),
        Type::Tuple(elems) => elems.iter().any(type_contains_param),
        Type::Struct(_, args) | Type::Enum(_, args) => args.iter().any(type_contains_param),
        _ => false,
    }
}

fn type_to_prim_ty(ty: &plum_types::types::Type) -> Result<ir::PrimTy, plum_syntax::error::CompileError> {
    use plum_types::types::Type;
    match ty {
        Type::Int => Ok(ir::PrimTy::Int),
        Type::Float => Ok(ir::PrimTy::Float),
        Type::Bool => Ok(ir::PrimTy::Bool),
        // `Unit` is a real, representable `PrimTy` now (added alongside
        // this function's SECOND caller — closure literal/eta-expansion
        // param and return types — which, unlike an empty array's
        // element type, can legitimately be Unit).
        Type::Unit => Ok(ir::PrimTy::Unit),
        Type::Str => Ok(ir::PrimTy::Str),
        Type::Struct(name, args) if name == "Array" && args.len() == 1 => {
            Ok(ir::PrimTy::Array(Box::new(type_to_prim_ty(&args[0])?)))
        }
        Type::Struct(..) | Type::Enum(..) => Ok(ir::PrimTy::Heap),
        // A closure-typed param/return (`|f: (Int) -> Int| ...`) —
        // only reachable via this function's closure-typing caller, an
        // empty array literal can never itself hold a function value's
        // worth of element type here in practice, but there's nothing
        // unsound about representing it either way.
        Type::Function(params, ret) => Ok(ir::PrimTy::Closure(
            params.iter().map(type_to_prim_ty).collect::<Result<Vec<_>, _>>()?,
            Box::new(type_to_prim_ty(ret)?),
        )),
        other => Err(plum_syntax::error::CompileError::spanless(format!(
            "resolved to {other:?}, which codegen can't represent as an array element / closure param or return type"
        ))),
    }
}

// The `MatchArm.tag` used for a catch-all arm (`_`/bare-ident) MIXED
// into an otherwise Ctor-tag-shaped match (`Circle(r) => .., _ => ..`)
// — same leading-digit, unreachable-by-any-real-identifier trick as
// `RANGE_TAG`/`ARRAY_TAG`/`tuple_tag`, but used differently: those are
// all CONSTRUCTED with this tag directly; this one is instead
// RECOGNIZED specially at runtime (`Interpreter::eval`'s `Match` case)
// as "matches unconditionally, regardless of the scrutinee's real
// tag" — see that eval case's own doc comment. Chosen over adding a
// genuinely new `MatchArm` field (e.g. `is_default: bool`) or a new IR
// node specifically to keep `ir::Expr::Match`'s shape completely
// unchanged — every OTHER pass that already walks `MatchArm.tag` as an
// opaque string (fbip.rs's traversals, for instance) needs zero
// changes to keep working correctly for this new case.
const DEFAULT_ARM_TAG: &str = "0Default";

// Lowers a param list into (top-level flat param names, body-wrapping
// destructures). A tuple-pattern param becomes a synthetic positional
// name (`__param0`, ...) PLUS a destructure to apply around the
// function body — IR functions only ever have a flat `Vec<String>` of
// params (see ir::Function), so there's nowhere else for the
// destructuring to live except as a `Match` wrapped around the body,
// reusing the exact same tag-based mechanism `Ctor`/`Match` already
// give every other heap-shaped value.
pub(crate) fn lower_params(params: &[ast::Param]) -> Result<(Vec<String>, Vec<(String, ast::Pattern)>), plum_syntax::error::CompileError> {
    let mut names = Vec::with_capacity(params.len());
    let mut destructures = Vec::new();
    for (i, param) in params.iter().enumerate() {
        match &param.kind {
            ast::ParamKind::Ident(name) => names.push(name.clone()),
            ast::ParamKind::Pattern(ast::Pattern::Ident(name, _), _) => names.push(name.clone()),
            // `()` — the Unit pattern (an empty tuple pattern). There's
            // nothing to bind (a Unit value has no fields), so this
            // needs no destructuring `Match` at all — just a synthetic,
            // never-referenced param name to keep arity correct
            // positionally. `lower_tag_pattern`'s `Ctor`-based
            // destructuring doesn't apply here anyway: `Unit` isn't a
            // heap `Ctor` with a tag to match against.
            ast::ParamKind::Pattern(ast::Pattern::Tuple(elems, _), _) if elems.is_empty() => {
                names.push(format!("__unit_param{i}"));
            }
            ast::ParamKind::Pattern(pattern @ (ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. }), _) => {
                let synthetic = format!("__param{i}");
                names.push(synthetic.clone());
                destructures.push((synthetic, pattern.clone()));
            }
            _ => {
                return Err(plum_syntax::error::CompileError::new(
                    param.span,
                    "lowering not yet implemented for destructuring function parameters of \
                     this shape (only tuples, structs, and plain identifiers so far)",
                ));
            }
        }
    }
    Ok((names, destructures))
}

// Wraps `rest` in a `Match` that destructures `scrutinee_name` (a
// synthetic param name — see `lower_params`) according to `pattern`.
// Variant, tuple, and struct patterns are supported, INCLUDING nested
// ones (a struct pattern inside a tuple, etc. — see `lower_tag_pattern`
// and `wrap_nested_destructures`).
pub(crate) fn wrap_destructure(
    scrutinee_name: String,
    pattern: &ast::Pattern,
    ctx: &LoweringContext,
    rest: ir::Expr,
) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let (tag, bindings, nested) = lower_tag_pattern(pattern, ctx)?;
    let body = wrap_nested_destructures(nested, ctx, rest)?;
    Ok(ir::Expr::Match {
        scrutinee: Box::new(ir::Expr::Var(scrutinee_name)),
        arms: vec![ir::MatchArm { tag, bindings, guard: None, body }],
    })
}

// Wraps `body` in a chain of `Match`es, one per (synthetic name, sub-
// pattern) pair `lower_tag_pattern` deferred — this is what makes
// NESTED patterns work (`(Point { x, y }, z)`, `Some(Point { x, y })`,
// ...): a nested position gets a synthetic binding at its OWN level,
// then this function destructures THAT synthetic binding further,
// recursively, exactly the same way `wrap_destructure` already
// destructures a synthetic function-param name. Order among sibling
// nested patterns doesn't matter (each becomes its own independent
// `Match`, scoped only around what comes after it).
fn wrap_nested_destructures(
    nested: Vec<(String, ast::Pattern)>,
    ctx: &LoweringContext,
    body: ir::Expr,
) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let mut result = body;
    for (name, pattern) in nested.into_iter().rev() {
        let (tag, bindings, more_nested) = lower_tag_pattern(&pattern, ctx)?;
        let inner_body = wrap_nested_destructures(more_nested, ctx, result)?;
        result = ir::Expr::Match {
            scrutinee: Box::new(ir::Expr::Var(name)),
            arms: vec![ir::MatchArm {
                tag,
                bindings,
                guard: None,
                body: inner_body,
            }],
        };
    }
    Ok(result)
}

// Classifies one sub-pattern position (a variant arg, tuple element, or
// struct field's sub-pattern) into `bindings`/`nested`: a plain
// identifier or wildcard binds directly; a variant/tuple/struct pattern
// gets a fresh synthetic name pushed into `bindings` AND queued in
// `nested` for `wrap_nested_destructures` to destructure further.
// Anything else (literal patterns, or-patterns, ...) is still rejected
// — nesting THOSE is a separate, unrelated gap.
fn classify_subpattern(
    pattern: &ast::Pattern,
    next_synthetic: &mut usize,
    bindings: &mut Vec<String>,
    nested: &mut Vec<(String, ast::Pattern)>,
) -> Result<(), plum_syntax::error::CompileError> {
    match pattern {
        ast::Pattern::Ident(name, _) => bindings.push(name.clone()),
        // `_` can never collide with a real user binding — the lexer
        // treats it as a distinct Underscore token, not a valid Ident,
        // so no genuine Plum variable can ever be named "_".
        ast::Pattern::Wildcard(_) => bindings.push("_".to_string()),
        p @ (ast::Pattern::Variant { .. } | ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. }) => {
            let synthetic = format!("__nested{next_synthetic}");
            *next_synthetic += 1;
            bindings.push(synthetic.clone());
            nested.push((synthetic, p.clone()));
        }
        other => {
            return Err(plum_syntax::error::CompileError::new(
                other.span(),
                "lowering not yet implemented for this pattern shape nested inside another pattern",
            ));
        }
    }
    Ok(())
}

// The shared core of "resolve a tag-based pattern (variant, tuple, or
// struct) into a tag plus positional bindings" — used for match arms,
// function params, and block-level `let` alike, so all three accept
// exactly the same shapes. A binding is either a real name (or `"_"`)
// or a SYNTHETIC name standing in for a nested sub-pattern still
// waiting to be destructured — see `classify_subpattern` and
// `wrap_nested_destructures`.
fn lower_tag_pattern(
    pattern: &ast::Pattern,
    ctx: &LoweringContext,
) -> Result<(String, Vec<String>, Vec<(String, ast::Pattern)>), plum_syntax::error::CompileError> {
    let mut next_synthetic = 0;
    match pattern {
        ast::Pattern::Variant { path, args, .. } => {
            let tag = path.last().cloned().expect("a path always has at least one segment");
            let mut bindings = Vec::with_capacity(args.len());
            let mut nested = Vec::new();
            for arg in args {
                classify_subpattern(arg, &mut next_synthetic, &mut bindings, &mut nested)?;
            }
            Ok((tag, bindings, nested))
        }
        ast::Pattern::Tuple(elems, span) => {
            if elems.is_empty() {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    "lowering not yet implemented for destructuring against the empty tuple \
                     pattern (there's nothing to bind — match against `()` instead)",
                ));
            }
            let mut bindings = Vec::with_capacity(elems.len());
            let mut nested = Vec::new();
            for e in elems {
                classify_subpattern(e, &mut next_synthetic, &mut bindings, &mut nested)?;
            }
            Ok((tuple_tag(bindings.len()), bindings, nested))
        }
        // `Point { x, y }` / `Point { x: px, .. }` — named fields need
        // the struct's DECLARED order (same reason struct literals need
        // `ctx`; see LoweringContext's doc comment), not the order
        // they're written in the pattern. `has_rest` (`..`) means "I
        // don't care about the fields I didn't mention" — the omitted
        // DECLARED positions still need SOME binding slot (Match
        // requires exactly one binding per declared field), so they
        // get `"_"`, same as an explicit wildcard. `has_rest` does NOT
        // relax the unknown-field check: naming a field the struct
        // doesn't have is still always an error.
        ast::Pattern::Struct {
            path,
            fields,
            has_rest,
            span,
        } => {
            let tag = path.last().cloned().expect("a path always has at least one segment");
            let Some(declared_fields) = ctx.struct_fields.get(&tag) else {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    format!("unknown struct type {tag:?} (no declaration found in this lowering context)"),
                ));
            };
            let mut by_name: HashMap<&str, &ast::Pattern> = HashMap::new();
            for f in fields {
                if by_name.insert(f.name.as_str(), &f.pattern).is_some() {
                    return Err(plum_syntax::error::CompileError::new(f.span, format!("field {:?} specified more than once", f.name)));
                }
            }
            let mut bindings = Vec::with_capacity(declared_fields.len());
            let mut nested = Vec::new();
            for declared_name in declared_fields {
                match by_name.remove(declared_name.as_str()) {
                    Some(sub_pattern) => {
                        classify_subpattern(sub_pattern, &mut next_synthetic, &mut bindings, &mut nested)?;
                    }
                    None if *has_rest => bindings.push("_".to_string()),
                    None => {
                        return Err(plum_syntax::error::CompileError::new(
                            *span,
                            format!("missing field {declared_name:?} for struct {tag:?} pattern (add `..` to ignore it)"),
                        ));
                    }
                }
            }
            if let Some((extra_name, _)) = by_name.into_iter().next() {
                return Err(plum_syntax::error::CompileError::new(*span, format!("struct {tag:?} has no field named {extra_name:?}")));
            }
            Ok((tag, bindings, nested))
        }
        other => Err(plum_syntax::error::CompileError::new(
            other.span(),
            "lowering not yet implemented for this pattern shape as a destructure",
        )),
    }
}

pub fn lower_expr(expr: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    match expr {
        ast::Expr::Int(n, _) => Ok(ir::Expr::Int(*n)),
        ast::Expr::Float(f, _) => Ok(ir::Expr::Float(*f)),
        ast::Expr::Str(s, _) => Ok(ir::Expr::Str(s.clone())),
        ast::Expr::Bool(b, _) => Ok(ir::Expr::Bool(*b)),
        // A bare capitalized name referencing a zero-arity variant
        // (`None`, not `None()`) constructs it directly — there's no
        // other surface syntax for a nullary variant.
        ast::Expr::Ident(name, _) if ctx.variants.get(name) == Some(&0) => Ok(ir::Expr::Ctor {
            tag: name.clone(),
            fields: vec![],
        }),
        // A non-zero-arity variant referenced BARE (not called) is its
        // constructor as a function value — eta-expanded into a real
        // `Closure` (`Circle` alone lowers as if it had been written
        // `|__ctor_arg0| Circle(__ctor_arg0)`), reusing the SAME
        // `Ctor`/`Closure` IR nodes everything else in this file does
        // rather than inventing a new "constructor value" IR node.
        // Mirrors `infer.rs`'s identical `Ident` case at the type level.
        ast::Expr::Ident(name, _) if ctx.variants.get(name).is_some_and(|&arity| arity > 0) => {
            let arity = ctx.variants[name];
            let params: Vec<String> = (0..arity).map(|i| format!("__ctor_arg{i}")).collect();
            let fields = params.iter().cloned().map(ir::Expr::Var).collect();
            // See `LoweringContext::variant_payload_types`'s doc
            // comment: `None` (falls through here) only when this
            // `LoweringContext` wasn't built from a real, type-checked
            // pipeline (a lowering-only test, or a caller that never
            // populated it) — harmless, since only `plum-codegen`
            // requires `Some`.
            let param_types = ctx
                .variant_payload_types
                .get(name)
                .map(|tys| tys.iter().map(type_to_prim_ty).collect::<Result<Vec<_>, _>>())
                .transpose()?;
            let ret_type = param_types.is_some().then_some(ir::PrimTy::Heap);
            Ok(ir::Expr::Closure {
                params,
                param_types,
                ret_type,
                body: Box::new(ir::Expr::Ctor {
                    tag: name.clone(),
                    fields,
                }),
            })
        }
        ast::Expr::Ident(name, _) => Ok(ir::Expr::Var(name.clone())),
        ast::Expr::Tuple(elems, _) if elems.is_empty() => Ok(ir::Expr::Unit),
        // A non-empty tuple is heap-allocated, positional, same as a
        // struct — `tuple_tag` picks a tag no user identifier can ever
        // spell (`is_ident_start` forbids a leading digit), so it can
        // never collide with a real struct/enum name.
        ast::Expr::Tuple(elems, _) => {
            let fields = elems.iter().map(|e| lower_expr(e, ctx)).collect::<Result<_, _>>()?;
            Ok(ir::Expr::Ctor {
                tag: tuple_tag(elems.len()),
                fields,
            })
        }
        // `[e1, e2, ...]` — heap-allocated exactly like a tuple/struct,
        // via the SAME `Ctor` node, just with a synthetic tag no
        // struct/enum declaration can ever spell (see `ARRAY_TAG`). An
        // EMPTY literal has no field to derive an element type from
        // later (see `plum-codegen`'s "how array element CgType is
        // determined" story), so — WHEN `plum_types::Infer` has resolved
        // one for this exact span (see `empty_array_elem_types`'s own
        // doc comment) — it lowers to the dedicated `EmptyArray` node
        // instead, carrying that type through. A lowering-only caller
        // that never ran type inference at all (this crate's own unit
        // tests included — see this file's precedent of exercising
        // `lower_expr` directly against a bare `LoweringContext::new()`)
        // has no such entry; falling back to the SAME untyped
        // `Ctor{ARRAY_TAG, []}` every other array literal produces
        // keeps that usage working exactly as before — only codegen
        // (which always runs after a real `plum_types::Infer` pass)
        // actually needs the `EmptyArray` distinction at all.
        ast::Expr::ArrayLiteral(elements, span) if elements.is_empty() => {
            // See the `Closure` arm's identical treatment (`resolved =
            // ... .filter(|ty| !type_contains_param(ty))`) for why a
            // resolved entry that still contains a `Type::Param`
            // TEMPLATE is treated the SAME as "no info available" here
            // too: `plumc::codegen_cli`'s eager, discarded `lower_
            // program` call (before `monomorphize::plan` runs, which
            // fully replaces this function's own output with a
            // per-instantiation-concrete substitution) hits an empty
            // array literal inside a still-generic function's own body
            // with exactly this still-templated type — falling back to
            // the untyped `Ctor{ARRAY_TAG, []}` (below) here too keeps
            // that eager pass from erroring on a type it was never
            // going to use anyway.
            let resolved = ctx.empty_array_elem_types.get(span).filter(|ty| !type_contains_param(ty));
            match resolved {
                Some(ty) => Ok(ir::Expr::EmptyArray(type_to_prim_ty(ty)?)),
                None => Ok(ir::Expr::Ctor {
                    tag: ARRAY_TAG.to_string(),
                    fields: vec![],
                }),
            }
        }
        ast::Expr::ArrayLiteral(elements, _) => {
            let fields = elements.iter().map(|e| lower_expr(e, ctx)).collect::<Result<_, _>>()?;
            Ok(ir::Expr::Ctor {
                tag: ARRAY_TAG.to_string(),
                fields,
            })
        }
        ast::Expr::Index { base, index, .. } => Ok(ir::Expr::Index {
            base: Box::new(lower_expr(base, ctx)?),
            index: Box::new(lower_expr(index, ctx)?),
        }),
        ast::Expr::Unary { op, expr, .. } => {
            let ir_op = match op {
                ast::UnaryOp::Neg => ir::UnOp::Neg,
                ast::UnaryOp::Not => ir::UnOp::Not,
            };
            Ok(ir::Expr::Unary(ir_op, Box::new(lower_expr(expr, ctx)?)))
        }
        ast::Expr::Binary {
            op: ast::BinaryOp::Pipe,
            lhs,
            rhs,
            ..
        } => lower_pipe(lhs, rhs, ctx),
        // A first-class `start..end` value — used outside `for`'s
        // iterand position (which still gets its own zero-allocation
        // fast path, see `lower_for`), represented as a heap `Ctor`
        // exactly like tuples/structs are, so nothing downstream
        // (FBIP, the interpreter's heap) needs a whole new value kind
        // just for this.
        ast::Expr::Binary {
            op: ast::BinaryOp::Range,
            lhs,
            rhs,
            ..
        } => Ok(ir::Expr::Ctor {
            tag: RANGE_TAG.to_string(),
            fields: vec![lower_expr(lhs, ctx)?, lower_expr(rhs, ctx)?],
        }),
        ast::Expr::Binary { op, lhs, rhs, .. } => Ok(ir::Expr::Binary(
            lower_binop(op),
            Box::new(lower_expr(lhs, ctx)?),
            Box::new(lower_expr(rhs, ctx)?),
        )),
        ast::Expr::Block(block, _) => lower_block(block, ctx),
        ast::Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let ir_cond = lower_expr(cond, ctx)?;
            let ir_then = lower_block(then_branch, ctx)?;
            let ir_else = match else_branch {
                Some(e) => lower_expr(e, ctx)?,
                None => ir::Expr::Unit,
            };
            Ok(ir::Expr::If {
                cond: Box::new(ir_cond),
                then_branch: Box::new(ir_then),
                else_branch: Box::new(ir_else),
            })
        }
        // `t.join()` — checked BEFORE the general Call/Field handling
        // below, the same "check the callee's SHAPE, not its type"
        // precedent already established for variant-tag detection
        // (lowering has no type information to confirm `t` is really a
        // `Task`). `join` isn't validated against anything else either
        // — a struct that happens to have a zero-arg callable field
        // literally named `join` would collide with this, an accepted,
        // narrow ambiguity matching that same precedent, not something
        // worth a whole side-channel (like `field_owners`) to resolve.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "join") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::TaskJoin {
                task: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `tx.send(v)` — same shape-only precedent as `.join()` above,
        // one arg instead of zero.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "send") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ChannelSend {
                sender: Box::new(lower_expr(base, ctx)?),
                value: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `rx.recv()` — same shape-only precedent, zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "recv") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ChannelRecv {
                receiver: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `channel[T]()` — a generic-instantiation callee named
        // `channel` with exactly one type argument, called with zero
        // value args. `T` is erased entirely, matching every other
        // generic in the language; nothing about it is checked here
        // (lowering never checks types) — `plum-types` is what
        // actually validates the arity/shape at the type level.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty()
                && matches!(
                    callee.as_ref(),
                    ast::Expr::GenericInst { callee, args, .. }
                        if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Ident(name, _) if name == "channel")
                ) =>
        {
            Ok(ir::Expr::Channel)
        }
        // `ref(v)` — a bare-Ident callee named `ref`, called with
        // exactly one value arg. Checked BEFORE the general variant-
        // construction fallback further below (same precedent as
        // `channel[T]()`'s own special-case): a bare capitalized-or-
        // lowercase Ident-call is otherwise assumed to name a declared
        // enum variant, so a real user-declared variant named `ref`
        // (variant names aren't required to be capitalized in this
        // grammar) would collide — an accepted, documented precedent-
        // matching risk, not a new one.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Ident(name, _) if name == "ref") =>
        {
            Ok(ir::Expr::RefNew {
                value: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `read_file_raw(path)`/`write_file_raw(path, contents)` — same
        // shape-only precedent as `ref(v)` immediately above.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Ident(name, _) if name == "read_file_raw") =>
        {
            Ok(ir::Expr::ReadFileRaw {
                path: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        ast::Expr::Call { callee, args, .. }
            if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Ident(name, _) if name == "write_file_raw") =>
        {
            Ok(ir::Expr::WriteFileRaw {
                path: Box::new(lower_expr(&args[0], ctx)?),
                contents: Box::new(lower_expr(&args[1], ctx)?),
            })
        }
        // `r.get()` — same shape-only precedent, zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "get") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::RefGet {
                base: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `r.set(v)` — same shape-only precedent, one arg. Note this
        // name COULD collide with `Array`'s own `.set(i, v)` (two
        // args) — but arity alone (`args.len() == 1` here vs `== 2`
        // for arrays) already disambiguates them at the pattern-match
        // level, so no further check is needed.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "set") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::RefSet {
                base: Box::new(lower_expr(base, ctx)?),
                value: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `arr.len()` — same shape-only precedent, zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "len") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ArrayLen {
                array: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `arr.push(v)` — same shape-only precedent, one arg.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "push") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ArrayPush {
                array: Box::new(lower_expr(base, ctx)?),
                value: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `arr.pop()` — same shape-only precedent, zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "pop") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ArrayPop {
                array: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `arr.set(i, v)` — same shape-only precedent, two args.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "set") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ArraySet {
                array: Box::new(lower_expr(base, ctx)?),
                index: Box::new(lower_expr(&args[0], ctx)?),
                value: Box::new(lower_expr(&args[1], ctx)?),
            })
        }
        // `arr.remove(i)` — same shape-only precedent, one arg.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "remove") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ArrayRemove {
                array: Box::new(lower_expr(base, ctx)?),
                index: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `s.concat(other)` — same shape-only precedent, one arg. Note
        // `.len()` (below, shared with arrays as `ArrayLen`) needs NO
        // new case here at all: lowering can't tell an array from a
        // string apart (no type info), so it stays the SAME node for
        // both, and `Interpreter::eval` dispatches on the actual heap
        // cell kind at runtime instead — see `ArrayLen`'s eval case.
        // `.concat()` doesn't have that ambiguity (arrays have no
        // `.concat()`), so it gets its own dedicated node.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "concat") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrConcat {
                base: Box::new(lower_expr(base, ctx)?),
                other: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `s.runes()` — same shape-only precedent, zero args. Also no
        // ambiguity with anything array-related, so its own dedicated
        // node too.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "runes") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrRunes {
                base: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `s.as_cstr()` — same shape-only precedent, zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "as_cstr") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::AsCStr(Box::new(lower_expr(base, ctx)?)))
        }
        // `s.trim()` — same shape-only precedent, zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "trim") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrTrim {
                base: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `s.split(sep)` — same shape-only precedent, one arg.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "split") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrSplit {
                base: Box::new(lower_expr(base, ctx)?),
                sep: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `s.to_upper()` / `s.to_lower()` — same shape-only precedent,
        // zero args.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "to_upper") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrToUpper {
                base: Box::new(lower_expr(base, ctx)?),
            })
        }
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "to_lower") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrToLower {
                base: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `s.contains(needle)` / `s.starts_with(prefix)` / `s.ends_with(
        // suffix)` — same shape-only precedent, one arg.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "contains") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrContains {
                base: Box::new(lower_expr(base, ctx)?),
                needle: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "starts_with") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrStartsWith {
                base: Box::new(lower_expr(base, ctx)?),
                prefix: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "ends_with") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrEndsWith {
                base: Box::new(lower_expr(base, ctx)?),
                suffix: Box::new(lower_expr(&args[0], ctx)?),
            })
        }
        // `s.replace(from, to)` — same shape-only precedent, two args.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "replace") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::StrReplace {
                base: Box::new(lower_expr(base, ctx)?),
                from: Box::new(lower_expr(&args[0], ctx)?),
                to: Box::new(lower_expr(&args[1], ctx)?),
            })
        }
        // `x.to_string()` — same shape-only precedent, zero args.
        // Unlike every other `.method()` shape above, `base` isn't
        // required to be ANY particular type at lowering time (there's
        // no type info here regardless) — see `ir::Expr::ToString`'s
        // doc comment for the runtime dispatch this implies.
        ast::Expr::Call { callee, args, .. }
            if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "to_string") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            Ok(ir::Expr::ToString {
                base: Box::new(lower_expr(base, ctx)?),
            })
        }
        // `arr.map(f)` — desugars into an index-based loop reusing only
        // EXISTING IR nodes (`Let`, `For`, `ArrayLen`, `Index`,
        // `ArrayPush`, `Assign`), same convention as `for x in arr`'s
        // own desugaring: build a fresh output array, push `f(elem)`
        // for each element in order.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "map") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            lower_array_map(base, &args[0], ctx)
        }
        // `arr.filter(f)` — same desugaring shape as `.map()`, but only
        // pushes an element when `f(elem)` is true.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "filter") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            lower_array_filter(base, &args[0], ctx)
        }
        // `arr.fold(init, f)` — same desugaring family, but accumulates
        // into a scalar (`f(acc, elem)`) instead of building a new
        // array.
        ast::Expr::Call { callee, args, .. }
            if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "fold") =>
        {
            let ast::Expr::Field { base, .. } = callee.as_ref() else {
                unreachable!("just matched this shape above");
            };
            lower_array_fold(base, &args[0], &args[1], ctx)
        }
        ast::Expr::Call { callee, args, span } => {
            // `Circle(1.0)` or `Shape.Circle(1.0)` constructs a variant
            // if the callee names one — checked BEFORE falling back to
            // an ordinary call, mirroring how struct literals already
            // get their own construction syntax. `Shape.Circle(...)`'s
            // qualifier (`Shape`) is parsed generically as
            // `Expr::Field` (there's no dedicated "qualified path"
            // node); it's never actually validated against the
            // variant's real owning enum here, matching the same
            // established precedent as pattern lowering (see
            // `LoweringContext::variants`' doc comment).
            let variant_tag = match callee.as_ref() {
                ast::Expr::Ident(name, _) => Some(name.as_str()),
                ast::Expr::Field { name, .. } => Some(name.as_str()),
                _ => None,
            };
            if let Some(tag) = variant_tag {
                if let Some(&arity) = ctx.variants.get(tag) {
                    if args.len() != arity {
                        return Err(plum_syntax::error::CompileError::new(
                            *span,
                            format!("variant {tag:?} expects {arity} field(s), found {}", args.len()),
                        ));
                    }
                    let fields = args.iter().map(|a| lower_expr(a, ctx)).collect::<Result<_, _>>()?;
                    return Ok(ir::Expr::Ctor {
                        tag: tag.to_string(),
                        fields,
                    });
                }
            }
            // `sqrt(2.0)` where `sqrt` names a declared `extern "C"`
            // function — a genuinely separate IR node, not a value call
            // (see `ir::Expr::ExternCall`'s doc comment). Only this
            // direct call-by-name shape is recognized; an extern
            // function referenced bare (`let f = sqrt`) falls through to
            // the ordinary `Var` case above and is a real name, not a
            // function value, at every later stage.
            if let ast::Expr::Ident(name, _) = callee.as_ref() {
                if ctx.extern_fns.contains_key(name) {
                    let ir_args = args.iter().map(|a| lower_expr(a, ctx)).collect::<Result<_, _>>()?;
                    return Ok(ir::Expr::ExternCall {
                        name: name.clone(),
                        args: ir_args,
                    });
                }
            }
            let ir_callee = lower_expr(callee, ctx)?;
            let ir_args = args.iter().map(|a| lower_expr(a, ctx)).collect::<Result<_, _>>()?;
            Ok(ir::Expr::Call {
                callee: Box::new(ir_callee),
                args: ir_args,
            })
        }
        ast::Expr::StructLiteral {
            path,
            fields,
            spread,
            span,
        } => lower_struct_literal(path, fields, spread, *span, ctx),
        ast::Expr::Match { scrutinee, arms, .. } => lower_match(scrutinee, arms, ctx),
        ast::Expr::Select { arms, .. } => lower_select(arms, ctx),
        ast::Expr::For { pattern, iter, body, span } => lower_for(pattern, iter, body, *span, ctx),
        // `unsafe` has nothing to mark yet — no IR operation is
        // unsafe-only (no raw pointers, no unchecked ops), so the block
        // lowers exactly as if the keyword weren't there. When the
        // language grows something `unsafe` actually gates, THAT'S
        // what needs an IR-level marker, not the block itself.
        ast::Expr::Unsafe(block, _) => lower_block(block, ctx),
        // DESIGN.md's "Implementation blocker: heap ownership across
        // tasks" is now Decided: deep-copy on crossing (see ir.rs's
        // `Spawn` doc comment) — `plum-interp` does the actual copying
        // at runtime, so lowering just needs to carry the block
        // through unchanged.
        ast::Expr::Spawn(block, _) => Ok(ir::Expr::Spawn {
            block: Box::new(lower_block(block, ctx)?),
        }),
        // Unlike function params, a closure param is ALWAYS a plain
        // identifier at the AST level (`ClosureParam` has no Pattern
        // case) — no destructuring restriction to enforce here.
        // Annotations are ignored, same as everywhere else lowering
        // erases them.
        ast::Expr::Closure { params, body, span } => {
            // See `LoweringContext::closure_types`'s doc comment —
            // `None` here only for a lowering-only caller with no real
            // `plum_types::Infer` pass behind it. A resolved entry that
            // still contains a `Type::Param` TEMPLATE (a closure nested
            // inside a still-generic function's own body — see
            // `plum_types::Infer::resolve_closure_types`'s doc comment)
            // is treated the SAME as "no info available": this happens
            // whenever a still-generic function's own body gets lowered
            // directly, UN-instantiated — `plumc::codegen_cli`'s eager
            // `lower_program` call before `monomorphize::plan` runs
            // (whose output for exactly this function gets discarded
            // and replaced wholesale, per-instantiation, with a fully
            // concrete substitution — see `monomorphize::plan`'s own
            // doc comment) and `plum-interp`'s lowering (which never
            // needs static closure types at all — dynamically typed)
            // both hit this path harmlessly.
            let resolved = ctx
                .closure_types
                .get(span)
                .filter(|(ptys, rty)| !ptys.iter().any(type_contains_param) && !type_contains_param(rty));
            let param_types = resolved
                .map(|(ptys, _)| ptys.iter().map(type_to_prim_ty).collect::<Result<Vec<_>, _>>())
                .transpose()?;
            let ret_type = resolved.map(|(_, rty)| type_to_prim_ty(rty)).transpose()?;
            Ok(ir::Expr::Closure {
                params: params.iter().map(|p| p.name.clone()).collect(),
                param_types,
                ret_type,
                body: Box::new(lower_expr(body, ctx)?),
            })
        }
        // `p.x` — there's no field-access IR node (see `ir.rs`'s scope
        // note), so this reuses the exact SAME construct-then-match
        // shape struct spread already established: destructure `base`
        // by tag, binding only the wanted field to a real name and
        // everything else to `"_"`, then the arm body is just that one
        // binding. `ctx.field_owners` (populated from `plum-types`'
        // inference pass — see its doc comment) is what says WHICH
        // struct `base` is, since the bare syntax alone doesn't.
        ast::Expr::Field { base, name, span } => {
            let Some(struct_name) = ctx.field_owners.get(span) else {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    "lowering not yet implemented for field access without a resolved owner \
                     (internal: type inference must run before lowering to populate \
                     `LoweringContext::field_owners`)",
                ));
            };
            let Some(declared_fields) = ctx.struct_fields.get(struct_name) else {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    format!("unknown struct type {struct_name:?} (no declaration found in this lowering context)"),
                ));
            };
            if !declared_fields.iter().any(|f| f == name) {
                return Err(plum_syntax::error::CompileError::new(*span, format!("struct {struct_name:?} has no field named {name:?}")));
            }
            let result_name = "__field_result".to_string();
            let bindings = declared_fields
                .iter()
                .map(|f| if f == name { result_name.clone() } else { "_".to_string() })
                .collect();
            Ok(ir::Expr::Match {
                scrutinee: Box::new(lower_expr(base, ctx)?),
                arms: vec![ir::MatchArm {
                    tag: struct_name.clone(),
                    bindings,
                    guard: None,
                    body: ir::Expr::Var(result_name),
                }],
            })
        }
        // Generic instantiation and indexing are still deferred — not
        // needed to validate struct/match lowering.
        other => Err(plum_syntax::error::CompileError::new(
            other.span(),
            "lowering not yet implemented for this expression form",
        )),
    }
}

fn lower_struct_literal(
    path: &[String],
    fields: &[ast::FieldInit],
    spread: &Option<Box<ast::Expr>>,
    span: plum_syntax::span::Span,
    ctx: &LoweringContext,
) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let tag = path.last().cloned().expect("a path always has at least one segment");
    let Some(declared_fields) = ctx.struct_fields.get(&tag) else {
        return Err(plum_syntax::error::CompileError::new(
            span,
            format!("unknown struct type {tag:?} (no declaration found in this lowering context)"),
        ));
    };

    let mut by_name: HashMap<&str, &ast::Expr> = HashMap::new();
    for f in fields {
        if by_name.insert(f.name.as_str(), &f.value).is_some() {
            return Err(plum_syntax::error::CompileError::new(f.span, format!("field {:?} specified more than once", f.name)));
        }
    }

    // No `..expr`: every field must be given explicitly.
    let Some(spread_expr) = spread else {
        let mut ir_fields = Vec::with_capacity(declared_fields.len());
        for declared_name in declared_fields {
            let Some(value_expr) = by_name.remove(declared_name.as_str()) else {
                return Err(plum_syntax::error::CompileError::new(span, format!("missing field {declared_name:?} for struct {tag:?}")));
            };
            ir_fields.push(lower_expr(value_expr, ctx)?);
        }
        if let Some((extra_name, _)) = by_name.into_iter().next() {
            return Err(plum_syntax::error::CompileError::new(span, format!("struct {tag:?} has no field named {extra_name:?}")));
        }
        return Ok(ir::Expr::Ctor { tag, fields: ir_fields });
    };

    // `Point { x: 1, ..other }`: fields not given explicitly come from
    // `other`, which must itself be a `Point`. There's no field-access
    // IR node — the IR only knows how to pull fields out of a heap
    // value by tag via `Match` — so this reuses the SAME
    // construct-then-match-then-reconstruct shape lowering already
    // produces for ordinary nested destructuring, binding every
    // declared field of `other` positionally and reconstructing with
    // the explicit fields substituted in.
    let ir_spread = lower_expr(spread_expr, ctx)?;
    let mut bindings = Vec::with_capacity(declared_fields.len());
    let mut ir_fields = Vec::with_capacity(declared_fields.len());
    for (i, declared_name) in declared_fields.iter().enumerate() {
        let synthetic = format!("__spread{i}");
        if let Some(value_expr) = by_name.remove(declared_name.as_str()) {
            ir_fields.push(lower_expr(value_expr, ctx)?);
        } else {
            ir_fields.push(ir::Expr::Var(synthetic.clone()));
        }
        bindings.push(synthetic);
    }
    if let Some((extra_name, _)) = by_name.into_iter().next() {
        return Err(plum_syntax::error::CompileError::new(span, format!("struct {tag:?} has no field named {extra_name:?}")));
    }

    Ok(ir::Expr::Match {
        scrutinee: Box::new(ir_spread),
        arms: vec![ir::MatchArm {
            tag: tag.clone(),
            bindings,
            guard: None,
            body: ir::Expr::Ctor { tag, fields: ir_fields },
        }],
    })
}

// A catch-all pattern — matches ANY value unconditionally (no tag
// inspection needed at all), used by both special cases below.
fn is_catchall_pattern(pattern: &ast::Pattern) -> bool {
    matches!(pattern, ast::Pattern::Wildcard(_) | ast::Pattern::Ident(..))
}

// A `match` whose arms are ALL literal patterns (`Int`/`Float`/`Bool`/
// `Str`) except a REQUIRED trailing catch-all — see `lower_literal_
// match`'s own doc comment for why this shape gets a totally different
// desugaring than the ordinary Ctor-tag path below.
fn is_literal_match(arms: &[ast::MatchArm]) -> bool {
    match arms.split_last() {
        Some((last, rest)) => {
            is_catchall_pattern(&last.pattern)
                && rest
                    .iter()
                    .all(|arm| matches!(arm.pattern, ast::Pattern::Int(..) | ast::Pattern::Float(..) | ast::Pattern::Bool(..) | ast::Pattern::Str(..)))
        }
        None => false,
    }
}

// `match anything { _ => body }` / `match anything { x => body }` — a
// SINGLE catch-all arm works for ANY scrutinee type at all (no tag
// inspection needed), so this doesn't need to go through either the
// literal-chain desugaring OR the Ctor-tag path. Desugars directly to
// an ordinary `Let` (or a discarded-binding `Let` for `_`, matching
// this file's established "expression-statement" convention).
// Deliberately excludes a GUARDED sole arm: a guard could still fail
// at runtime with nothing left to fall back to, which is exactly the
// exhaustiveness gap this whole match-literals feature was designed to
// close at compile time instead — see `lower_literal_match`'s
// identical restriction on its own trailing arm.
fn lower_catchall_only_match(scrutinee: &ast::Expr, arm: &ast::MatchArm, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let ir_scrutinee = lower_expr(scrutinee, ctx)?;
    let body = lower_expr(&arm.body, ctx)?;
    let name = match &arm.pattern {
        ast::Pattern::Ident(name, _) => name.clone(),
        ast::Pattern::Wildcard(_) => "_".to_string(),
        _ => unreachable!("caller already checked this is a catch-all pattern"),
    };
    Ok(ir::Expr::Let {
        name,
        value: Box::new(ir_scrutinee),
        body: Box::new(body),
    })
}

// `match x { 1 => .., 2 => .., _ => .. }` — Plum's `Match` IR node
// (below) is fundamentally Ctor-TAG-based: it reads a heap cell's tag
// and dispatches on it. That works great for enums/structs/tuples/
// arrays (all `Ctor`-shaped), but `Int`/`Float`/`Bool` aren't heap
// values at all, and even heap-backed `Str` isn't `Ctor`-shaped either
// (see heap.rs's `CellData::Str`) — none of them have a "tag" to
// dispatch on. So a literal match desugars into something completely
// different: an ordinary `If`/`Binary(Eq)` chain, reusing existing IR
// nodes end to end (no new node needed).
//
// The exhaustiveness problem this creates — literal domains are
// infinite (or, for `Str`, at least not enumerable), so listing a
// finite set of literals can NEVER be exhaustive the way a finite
// enum's tag set can — is closed at COMPILE time, not deferred to a
// runtime "no arm matched" error the way Ctor-matching's own
// (separate) exhaustiveness gap already is: `is_literal_match` (the
// caller) already required a trailing catch-all arm, and this function
// additionally rejects that arm having its OWN guard (a guard that
// evaluates false would reopen exactly the gap the trailing arm exists
// to close, and there is deliberately no new "runtime match failure"
// IR node to fall back to here).
fn lower_literal_match(scrutinee: &ast::Expr, arms: &[ast::MatchArm], ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let (last, rest) = arms.split_last().expect("is_literal_match already checked arms is non-empty");
    if last.guard.is_some() {
        return Err(plum_syntax::error::CompileError::new(
            last.span,
            "the trailing wildcard/identifier arm of a literal `match` cannot have a guard \
             (it must be able to unconditionally catch anything earlier arms didn't)",
        ));
    }

    let scrutinee_name = "__match_scrutinee".to_string();
    let ir_scrutinee = lower_expr(scrutinee, ctx)?;

    let last_body = lower_expr(&last.body, ctx)?;
    let mut tail = match &last.pattern {
        ast::Pattern::Ident(name, _) => ir::Expr::Let {
            name: name.clone(),
            value: Box::new(ir::Expr::Var(scrutinee_name.clone())),
            body: Box::new(last_body),
        },
        ast::Pattern::Wildcard(_) => last_body,
        _ => unreachable!("is_literal_match already checked the last arm is a catch-all"),
    };

    for arm in rest.iter().rev() {
        let literal_expr = match &arm.pattern {
            ast::Pattern::Int(n, _) => ir::Expr::Int(*n),
            ast::Pattern::Float(f, _) => ir::Expr::Float(*f),
            ast::Pattern::Bool(b, _) => ir::Expr::Bool(*b),
            ast::Pattern::Str(s, _) => ir::Expr::Str(s.clone()),
            _ => unreachable!("is_literal_match already checked every non-last arm is a literal"),
        };
        let eq_cond = ir::Expr::Binary(
            ir::BinOp::Eq,
            Box::new(ir::Expr::Var(scrutinee_name.clone())),
            Box::new(literal_expr),
        );
        let cond = match &arm.guard {
            Some(g) => ir::Expr::Binary(ir::BinOp::And, Box::new(eq_cond), Box::new(lower_expr(g, ctx)?)),
            None => eq_cond,
        };
        let body = lower_expr(&arm.body, ctx)?;
        tail = ir::Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(body),
            else_branch: Box::new(tail),
        };
    }

    Ok(ir::Expr::Let {
        name: scrutinee_name,
        value: Box::new(ir_scrutinee),
        body: Box::new(tail),
    })
}

fn lower_match(scrutinee: &ast::Expr, arms: &[ast::MatchArm], ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    if arms.len() == 1 && is_catchall_pattern(&arms[0].pattern) && arms[0].guard.is_none() {
        return lower_catchall_only_match(scrutinee, &arms[0], ctx);
    }
    if is_literal_match(arms) {
        return lower_literal_match(scrutinee, arms, ctx);
    }
    let ir_scrutinee = lower_expr(scrutinee, ctx)?;
    let mut ir_arms = Vec::with_capacity(arms.len());
    for (i, arm) in arms.iter().enumerate() {
        // A catch-all in the LAST position, mixed among otherwise
        // Ctor-tag-shaped arms, gets the `DEFAULT_ARM_TAG` sentinel
        // instead of going through `lower_tag_pattern` (which has no
        // case for Wildcard/Ident and would reject it) — see that
        // constant's own doc comment for the full "why a sentinel tag"
        // reasoning. Unlike the pure-literal-match case (`is_literal_
        // match`, handled entirely separately above), this does NOT
        // require the trailing arm to be guard-free: a guard
        // evaluating false here just falls through to the SAME "no
        // match arm for tag" runtime error ordinary Ctor-tag matching
        // already accepts as a possibility (see the existing
        // `no_matching_guarded_arm_is_a_runtime_error` precedent in
        // plum-interp) — there's no NEW exhaustiveness gap being
        // opened here, unlike the literal-match case, which
        // deliberately avoids relying on any runtime fallback at all.
        if i == arms.len() - 1 && is_catchall_pattern(&arm.pattern) {
            let bindings = match &arm.pattern {
                ast::Pattern::Ident(name, _) => vec![name.clone()],
                ast::Pattern::Wildcard(_) => vec![],
                _ => unreachable!("just checked this is a catch-all pattern"),
            };
            let guard = match &arm.guard {
                Some(g) => Some(Box::new(lower_expr(g, ctx)?)),
                None => None,
            };
            let body = lower_expr(&arm.body, ctx)?;
            ir_arms.push(ir::MatchArm {
                tag: DEFAULT_ARM_TAG.to_string(),
                bindings,
                guard,
                body,
            });
            continue;
        }
        // `A(v) | B(v) => body` — one arm covering multiple tags, all
        // of which must bind the SAME names in the SAME order (a
        // real, checked requirement: the shared `body` only makes
        // sense typed once, against one consistent binding
        // environment — same rule Rust's own or-patterns enforce).
        // Desugars into MULTIPLE `MatchArm`s (one per alternative tag),
        // all sharing the SAME `guard`/`body` IR (lowered once, cloned
        // per alternative) — `ir::Expr::Match` needed no new concept
        // for this at all, unlike the DEFAULT_ARM_TAG case above,
        // which needed a sentinel: ordinary tag matching ALREADY
        // supports "more than one arm sharing a tag" (see `MatchArm`'s
        // own doc comment on `two_arms_may_share_a_tag`-style tests),
        // and nothing stops the REVERSE — one logical arm expanding to
        // several tags. Scoped to Variant/Tuple/Struct alternatives
        // with NO nested sub-patterns (same restriction the guard-vs-
        // nested check below has, for the same reason: a guard/shared
        // body needs real, already-bound names, not synthetic
        // placeholders that only resolve deep inside a destructure
        // chain) — literal alternatives (`1 | 2 => ..`) are a separate,
        // still-unimplemented extension.
        if let ast::Pattern::Or(alts, span) = &arm.pattern {
            let mut alt_shapes: Vec<(String, Vec<String>)> = Vec::with_capacity(alts.len());
            for alt in alts {
                let (alt_tag, alt_bindings, alt_nested) = lower_tag_pattern(alt, ctx)?;
                if !alt_nested.is_empty() {
                    return Err(plum_syntax::error::CompileError::new(
                        alt.span(),
                        "lowering not yet implemented for a nested pattern inside an \
                         or-pattern alternative",
                    ));
                }
                alt_shapes.push((alt_tag, alt_bindings));
            }
            let first_bindings = alt_shapes[0].1.clone();
            if alt_shapes[1..].iter().any(|(_, b)| *b != first_bindings) {
                return Err(plum_syntax::error::CompileError::new(
                    *span,
                    "every alternative of an or-pattern must bind the same names in the same order",
                ));
            }
            let guard = match &arm.guard {
                Some(g) => Some(lower_expr(g, ctx)?),
                None => None,
            };
            let body = lower_expr(&arm.body, ctx)?;
            for (alt_tag, alt_bindings) in alt_shapes {
                ir_arms.push(ir::MatchArm {
                    tag: alt_tag,
                    bindings: alt_bindings,
                    guard: guard.clone().map(Box::new),
                    body: body.clone(),
                });
            }
            continue;
        }
        let (tag, bindings, nested) = lower_tag_pattern(&arm.pattern, ctx)?;
        // A guard is only supported on an arm whose pattern needs NO
        // nested destructuring — `nested` non-empty means some of
        // `bindings` are still synthetic placeholders at this point
        // (real names only exist further down, inside the
        // `wrap_nested_destructures` chain wrapped around the BODY),
        // so a guard referencing them here couldn't see the real
        // bindings yet. Lifting this restriction would mean wrapping
        // the guard in the same destructure chain as the body, which
        // is real, separate follow-up work.
        let guard = match &arm.guard {
            Some(g) if !nested.is_empty() => {
                return Err(plum_syntax::error::CompileError::new(
                    arm.span,
                    "lowering not yet implemented for a match guard combined with a nested pattern",
                ));
            }
            Some(g) => Some(Box::new(lower_expr(g, ctx)?)),
            None => None,
        };
        let arm_body = lower_expr(&arm.body, ctx)?;
        let body = wrap_nested_destructures(nested, ctx, arm_body)?;
        ir_arms.push(ir::MatchArm { tag, bindings, guard, body });
    }
    Ok(ir::Expr::Match {
        scrutinee: Box::new(ir_scrutinee),
        arms: ir_arms,
    })
}

// `pattern = expr` in a `select` arm: binds the received value the
// SAME way `lower_params` binds a function parameter — a plain `Ident`/
// `Wildcard` needs no `Match` at all (direct `Let`, or nothing);
// anything tag-shaped (`Variant`/`Tuple`/`Struct`) reuses the exact
// same `wrap_destructure` a struct-destructuring function param does.
// `"__select_recv"` is the FIXED synthetic name `Interpreter::eval`'s
// `Select` case binds to the actually-received value before
// evaluating this wrapped body — see ir.rs's `Select` doc comment.
fn wrap_select_arm_pattern(pattern: &ast::Pattern, ctx: &LoweringContext, body: ir::Expr) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    match pattern {
        ast::Pattern::Ident(name, _) => Ok(ir::Expr::Let {
            name: name.clone(),
            value: Box::new(ir::Expr::Var("__select_recv".to_string())),
            body: Box::new(body),
        }),
        ast::Pattern::Wildcard(_) => Ok(body),
        ast::Pattern::Variant { .. } | ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. } => {
            wrap_destructure("__select_recv".to_string(), pattern, ctx, body)
        }
        other => Err(plum_syntax::error::CompileError::new(
            other.span(),
            "lowering not yet implemented for this pattern shape in a `select` arm",
        )),
    }
}

// `select { pattern = expr => body, ... }` — `expr` is required to be
// an `X.recv()` call SHAPE (checked here, not by the parser — see
// ast.rs's `Select` doc comment); `X` becomes the arm's `receiver`.
fn lower_select(arms: &[ast::SelectArm], ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let mut ir_arms = Vec::with_capacity(arms.len());
    for arm in arms {
        let ast::Expr::Call { callee, args, span } = &arm.expr else {
            return Err(plum_syntax::error::CompileError::new(arm.expr.span(), "`select` arm requires an `expr.recv()` call"));
        };
        let ast::Expr::Field { base, name, .. } = callee.as_ref() else {
            return Err(plum_syntax::error::CompileError::new(*span, "`select` arm requires an `expr.recv()` call"));
        };
        if name != "recv" || !args.is_empty() {
            return Err(plum_syntax::error::CompileError::new(*span, "`select` arm requires an `expr.recv()` call"));
        }
        let receiver = lower_expr(base, ctx)?;
        let arm_body = lower_expr(&arm.body, ctx)?;
        let body = wrap_select_arm_pattern(&arm.pattern, ctx, arm_body)?;
        ir_arms.push(ir::SelectArm { receiver, body });
    }
    Ok(ir::Expr::Select { arms: ir_arms })
}

// Looks up `f`'s already-resolved closure param/return types via
// `ctx.closure_types` (populated by `plum_types::Infer::
// resolve_closure_types` for every closure LITERAL the real pipeline
// saw — see that field's own doc comment) — used by `lower_array_map`/
// `lower_array_filter` to give their EMPTY output accumulator a real
// `PrimTy` (see `EmptyArray`'s doc comment for why `Ctor{ARRAY_TAG, []}`
// isn't a legal empty-array representation). Only works when `f` is
// written as a closure literal directly at the call site — `f`'s span
// only ends up in `closure_types` when `plum_types::Infer` walked an
// `Expr::Closure` node at exactly that span; a named-function-valued
// `f` (`arr.map(someFn)`) isn't resolvable this way and is a clear
// error instead of a silent guess.
// `.map()`'s output elem type is `f`'s RETURN type; `.filter()`'s is
// `f`'s single PARAMETER type (filter never changes element type, so
// the kept elements are exactly `f`'s own argument type). Both go
// through this one lookup: `Some` when `f`'s span was resolved by a
// real `plum_types::Infer` pass (the normal, type-checked pipeline);
// `None` in TWO different situations that need different treatment at
// the call site — see `closure_elem_ty`'s doc comment.
fn closure_result_types(f: &ast::Expr, ctx: &LoweringContext) -> Option<(Vec<plum_types::types::Type>, plum_types::types::Type)> {
    ctx.closure_types.get(&f.span()).cloned()
}

/// Picks the empty-output-accumulator element `PrimTy` for `.map()`/
/// `.filter()` (see `EmptyArray`'s doc comment for why this precision
/// is needed at all) — `pick` extracts the relevant half of `f`'s
/// resolved `(param_types, ret_type)` (map wants the return type,
/// filter wants `param_types[0]`).
///
/// `ctx.closure_types` is EMPTY for every lowering-only caller with no
/// real `plum_types::Infer` pass behind it at all (`plum-interp`'s own
/// dynamically-typed evaluation path, and this crate's own hand-built
/// lowering unit tests that construct a bare `LoweringContext::new()`)
/// — in that case there's no resolved type to source from ANYWHERE, so
/// this falls back to `PrimTy::Heap` (the same "we genuinely don't
/// know, and don't need to" placeholder `type_to_prim_ty` already uses
/// for an unresolvable struct/enum instantiation): harmless, since
/// `plum-interp`'s own `Expr::EmptyArray` evaluation ignores its
/// `PrimTy` payload entirely (see that match arm's doc comment) — it
/// only matters to codegen, which is never reached from that path.
///
/// A NON-empty `ctx.closure_types` that still has no entry for `f`'s
/// span, by contrast, means a REAL `Infer` pass ran but couldn't
/// resolve `f`'s own type — either `f` wasn't written as a closure
/// literal directly at the call site (a named function value,
/// `arr.map(someFn)`, isn't resolvable this way — not supported yet)
/// or a genuine internal inconsistency; either way, that's a clear
/// error, never a silent guess.
fn closure_elem_ty(
    f: &ast::Expr,
    ctx: &LoweringContext,
    what: &str,
    pick: impl FnOnce(Vec<plum_types::types::Type>, plum_types::types::Type) -> Option<plum_types::types::Type>,
) -> Result<ir::PrimTy, plum_syntax::error::CompileError> {
    match closure_result_types(f, ctx) {
        Some((param_tys, ret_ty)) => {
            let ty = pick(param_tys, ret_ty).ok_or_else(|| {
                plum_syntax::error::CompileError::new(f.span(), format!("codegen: `.{what}()`'s function argument must take exactly one parameter"))
            })?;
            type_to_prim_ty(&ty)
        }
        None if ctx.closure_types.is_empty() => Ok(ir::PrimTy::Heap),
        None => Err(plum_syntax::error::CompileError::new(
            f.span(),
            format!(
                "codegen: `.{what}()`'s function argument must be a closure literal written directly at the call site \
                 (its resolved type couldn't be found) — passing an already-named function value here isn't supported yet"
            ),
        )),
    }
}

// `arr.map(f)` — desugars to:
//   let __map_arr = <arr> in
//     let __map_out = [] in
//       let _ = for __map_i in 0..__map_arr.len() {
//         __map_out = __map_out.push(f(__map_arr[__map_i]));
//       } in
//       __map_out
// Built directly as `ir::Expr` (not via `ast`/`lower_expr` on a
// synthesized AST) — the same approach `lower_for`'s own array-loop
// desugaring below already takes, since there's no surface syntax this
// shape corresponds to.
fn lower_array_map(base: &ast::Expr, f: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let arr_name = "__map_arr".to_string();
    let out_name = "__map_out".to_string();
    let idx_name = "__map_i".to_string();
    let ir_base = lower_expr(base, ctx)?;
    let out_elem_ty = closure_elem_ty(f, ctx, "map", |_params, ret| Some(ret))?;
    let ir_f = lower_expr(f, ctx)?;
    Ok(ir::Expr::Let {
        name: arr_name.clone(),
        value: Box::new(ir_base),
        body: Box::new(ir::Expr::Let {
            name: out_name.clone(),
            value: Box::new(ir::Expr::EmptyArray(out_elem_ty)),
            body: Box::new(ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(ir::Expr::For {
                    var: idx_name.clone(),
                    start: Box::new(ir::Expr::Int(0)),
                    end: Box::new(ir::Expr::ArrayLen {
                        array: Box::new(ir::Expr::Var(arr_name.clone())),
                    }),
                    body: Box::new(ir::Expr::Assign {
                        name: out_name.clone(),
                        value: Box::new(ir::Expr::ArrayPush {
                            array: Box::new(ir::Expr::Var(out_name.clone())),
                            value: Box::new(ir::Expr::Call {
                                callee: Box::new(ir_f),
                                args: vec![ir::Expr::Index {
                                    base: Box::new(ir::Expr::Var(arr_name.clone())),
                                    index: Box::new(ir::Expr::Var(idx_name)),
                                }],
                            }),
                        }),
                        rest: Box::new(ir::Expr::Unit),
                    }),
                }),
                body: Box::new(ir::Expr::Var(out_name)),
            }),
        }),
    })
}

// `arr.filter(f)` — same shape as `lower_array_map`, but only pushes
// an element when `f(elem)` evaluates to `true`:
//   let __filter_arr = <arr> in
//     let __filter_out = [] in
//       let _ = for __filter_i in 0..__filter_arr.len() {
//         let __filter_elem = __filter_arr[__filter_i] in
//           if f(__filter_elem) {
//             __filter_out = __filter_out.push(__filter_elem);
//           } else { () }
//       } in
//       __filter_out
fn lower_array_filter(base: &ast::Expr, f: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let arr_name = "__filter_arr".to_string();
    let out_name = "__filter_out".to_string();
    let idx_name = "__filter_i".to_string();
    let elem_name = "__filter_elem".to_string();
    let ir_base = lower_expr(base, ctx)?;
    let out_elem_ty = closure_elem_ty(f, ctx, "filter", |mut params, _ret| if params.is_empty() { None } else { Some(params.remove(0)) })?;
    let ir_f = lower_expr(f, ctx)?;
    Ok(ir::Expr::Let {
        name: arr_name.clone(),
        value: Box::new(ir_base),
        body: Box::new(ir::Expr::Let {
            name: out_name.clone(),
            value: Box::new(ir::Expr::EmptyArray(out_elem_ty)),
            body: Box::new(ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(ir::Expr::For {
                    var: idx_name.clone(),
                    start: Box::new(ir::Expr::Int(0)),
                    end: Box::new(ir::Expr::ArrayLen {
                        array: Box::new(ir::Expr::Var(arr_name.clone())),
                    }),
                    body: Box::new(ir::Expr::Let {
                        name: elem_name.clone(),
                        value: Box::new(ir::Expr::Index {
                            base: Box::new(ir::Expr::Var(arr_name.clone())),
                            index: Box::new(ir::Expr::Var(idx_name)),
                        }),
                        body: Box::new(ir::Expr::If {
                            cond: Box::new(ir::Expr::Call {
                                callee: Box::new(ir_f),
                                args: vec![ir::Expr::Var(elem_name.clone())],
                            }),
                            then_branch: Box::new(ir::Expr::Assign {
                                name: out_name.clone(),
                                value: Box::new(ir::Expr::ArrayPush {
                                    array: Box::new(ir::Expr::Var(out_name.clone())),
                                    value: Box::new(ir::Expr::Var(elem_name)),
                                }),
                                rest: Box::new(ir::Expr::Unit),
                            }),
                            else_branch: Box::new(ir::Expr::Unit),
                        }),
                    }),
                }),
                body: Box::new(ir::Expr::Var(out_name)),
            }),
        }),
    })
}

// `arr.fold(init, f)` — same desugaring family, accumulating a scalar
// instead of building a new array:
//   let __fold_arr = <arr> in
//     let __fold_acc = <init> in
//       let _ = for __fold_i in 0..__fold_arr.len() {
//         __fold_acc = f(__fold_acc, __fold_arr[__fold_i]);
//       } in
//       __fold_acc
fn lower_array_fold(base: &ast::Expr, init: &ast::Expr, f: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let arr_name = "__fold_arr".to_string();
    let acc_name = "__fold_acc".to_string();
    let idx_name = "__fold_i".to_string();
    let ir_base = lower_expr(base, ctx)?;
    let ir_init = lower_expr(init, ctx)?;
    let ir_f = lower_expr(f, ctx)?;
    Ok(ir::Expr::Let {
        name: arr_name.clone(),
        value: Box::new(ir_base),
        body: Box::new(ir::Expr::Let {
            name: acc_name.clone(),
            value: Box::new(ir_init),
            body: Box::new(ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(ir::Expr::For {
                    var: idx_name.clone(),
                    start: Box::new(ir::Expr::Int(0)),
                    end: Box::new(ir::Expr::ArrayLen {
                        array: Box::new(ir::Expr::Var(arr_name.clone())),
                    }),
                    body: Box::new(ir::Expr::Assign {
                        name: acc_name.clone(),
                        value: Box::new(ir::Expr::Call {
                            callee: Box::new(ir_f),
                            args: vec![
                                ir::Expr::Var(acc_name.clone()),
                                ir::Expr::Index {
                                    base: Box::new(ir::Expr::Var(arr_name.clone())),
                                    index: Box::new(ir::Expr::Var(idx_name)),
                                },
                            ],
                        }),
                        rest: Box::new(ir::Expr::Unit),
                    }),
                }),
                body: Box::new(ir::Expr::Var(acc_name)),
            }),
        }),
    })
}

// `for pattern in iter { body }`. Only two things are supported so far,
// both erroring loudly rather than silently otherwise:
//   - `pattern` must be a plain identifier — same restriction as
//     function/let-binding patterns elsewhere in this file, for the
//     same reason (destructuring needs its own pass).
//   - `iter` must be a literal Range (`start..end`) written directly —
//     no array/list/collection type exists yet at the IR level to
//     iterate over otherwise, so anything else (a variable, a call
//     result, even a variable that HOLDS a range) can't be lowered
//     until one does.
fn lower_for(
    pattern: &ast::Pattern,
    iter: &ast::Expr,
    body: &ast::Block,
    span: plum_syntax::span::Span,
    ctx: &LoweringContext,
) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let var = match pattern {
        ast::Pattern::Ident(name, _) => name.clone(),
        other => {
            return Err(plum_syntax::error::CompileError::new(
                other.span(),
                "lowering not yet implemented for destructuring `for` patterns",
            ));
        }
    };
    // The literal shape (`for i in 0..n`) skips constructing a heap
    // Range value at all — `start`/`end` go straight into `ir::Expr::
    // For`, no allocation, no Match. Anything else that TYPE-CHECKS as
    // a Range (a variable, a call result, ...) still has no
    // array/list/collection type to iterate — the only other thing
    // `for` can mean is "destructure the Range value I was handed,"
    // via the exact same construct-then-match shape used everywhere
    // else in this file: evaluate `iter`, `Match` it against
    // `RANGE_TAG` to pull out `start`/`end`, then loop between those.
    if let ast::Expr::Binary {
        op: ast::BinaryOp::Range,
        lhs,
        rhs,
        ..
    } = iter
    {
        return Ok(ir::Expr::For {
            var,
            start: Box::new(lower_expr(lhs, ctx)?),
            end: Box::new(lower_expr(rhs, ctx)?),
            body: Box::new(lower_block(body, ctx)?),
        });
    }
    // `for x in arr` — `plum_types::Infer::array_for_loops` (threaded
    // in via `LoweringContext::with_array_for_loops`) already decided,
    // during inference, that this loop's iterand is `Array[T]` rather
    // than `Range`. Desugar into an index-based loop reusing only
    // EXISTING IR nodes (`Let`, `For`, `ArrayLen`, `Index`) — no new IR
    // node needed, matching this file's established "reuse what
    // already exists" convention: evaluate the array once into a
    // synthetic binding, loop an index from 0 to its length, and bind
    // the user's variable to `arr[i]` each iteration.
    if ctx.array_for_loops.contains(&span) {
        let arr_name = "__for_arr".to_string();
        let idx_name = "__for_i".to_string();
        return Ok(ir::Expr::Let {
            name: arr_name.clone(),
            value: Box::new(lower_expr(iter, ctx)?),
            body: Box::new(ir::Expr::For {
                var: idx_name.clone(),
                start: Box::new(ir::Expr::Int(0)),
                end: Box::new(ir::Expr::ArrayLen {
                    array: Box::new(ir::Expr::Var(arr_name.clone())),
                }),
                body: Box::new(ir::Expr::Let {
                    name: var,
                    value: Box::new(ir::Expr::Index {
                        base: Box::new(ir::Expr::Var(arr_name)),
                        index: Box::new(ir::Expr::Var(idx_name)),
                    }),
                    body: Box::new(lower_block(body, ctx)?),
                }),
            }),
        });
    }
    let ir_iter = lower_expr(iter, ctx)?;
    let start_name = "__range_start".to_string();
    let end_name = "__range_end".to_string();
    Ok(ir::Expr::Match {
        scrutinee: Box::new(ir_iter),
        arms: vec![ir::MatchArm {
            tag: RANGE_TAG.to_string(),
            bindings: vec![start_name.clone(), end_name.clone()],
            guard: None,
            body: ir::Expr::For {
                var,
                start: Box::new(ir::Expr::Var(start_name)),
                end: Box::new(ir::Expr::Var(end_name)),
                body: Box::new(lower_block(body, ctx)?),
            },
        }],
    })
}

// `x |> rhs` inserts `x` as the LAST argument of the call `rhs`
// denotes; a bare identifier with no parens is treated as a
// zero-argument call before insertion. This is DESIGN.md's pipe
// desugaring rule, and it's a compile-time rewrite, not a runtime
// capability — it doesn't need currying to work, see DESIGN.md.
fn lower_pipe(lhs: &ast::Expr, rhs: &ast::Expr, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let ir_lhs = lower_expr(lhs, ctx)?;
    match rhs {
        ast::Expr::Call { callee, args, .. } => {
            let mut ir_args: Vec<ir::Expr> =
                args.iter().map(|a| lower_expr(a, ctx)).collect::<Result<_, _>>()?;
            ir_args.push(ir_lhs);
            Ok(ir::Expr::Call {
                callee: Box::new(lower_expr(callee, ctx)?),
                args: ir_args,
            })
        }
        other => Ok(ir::Expr::Call {
            callee: Box::new(lower_expr(other, ctx)?),
            args: vec![ir_lhs],
        }),
    }
}

fn lower_binop(op: &ast::BinaryOp) -> ir::BinOp {
    match op {
        ast::BinaryOp::Add => ir::BinOp::Add,
        ast::BinaryOp::Sub => ir::BinOp::Sub,
        ast::BinaryOp::Mul => ir::BinOp::Mul,
        ast::BinaryOp::Div => ir::BinOp::Div,
        ast::BinaryOp::Rem => ir::BinOp::Rem,
        ast::BinaryOp::Eq => ir::BinOp::Eq,
        ast::BinaryOp::Ne => ir::BinOp::Ne,
        ast::BinaryOp::Lt => ir::BinOp::Lt,
        ast::BinaryOp::Gt => ir::BinOp::Gt,
        ast::BinaryOp::Le => ir::BinOp::Le,
        ast::BinaryOp::Ge => ir::BinOp::Ge,
        ast::BinaryOp::And => ir::BinOp::And,
        ast::BinaryOp::Or => ir::BinOp::Or,
        ast::BinaryOp::Range | ast::BinaryOp::Pipe => {
            unreachable!("Range and Pipe are handled before lower_binop is called")
        }
    }
}

// Folds a block's statement list into nested `let`s, right to left — a
// discarded expression-statement becomes `let _ = expr in rest`, the
// standard way to represent sequencing without a dedicated IR node.
fn lower_block(block: &ast::Block, ctx: &LoweringContext) -> Result<ir::Expr, plum_syntax::error::CompileError> {
    let mut result = match &block.tail {
        Some(t) => lower_expr(t, ctx)?,
        None => ir::Expr::Unit,
    };
    for stmt in block.stmts.iter().rev() {
        result = match stmt {
            ast::Stmt::Let {
                pattern: ast::Pattern::Ident(name, _),
                value,
                ..
            } => ir::Expr::Let {
                name: name.clone(),
                value: Box::new(lower_expr(value, ctx)?),
                body: Box::new(result),
            },
            // `let (a, b) = expr;` / `let Point { x, y } = expr;`
            // destructure directly against the VALUE, no synthetic name
            // needed — unlike a function param (which needs a flat name
            // to seed the initial env before any destructuring can
            // run), a block-level `let`'s value is just an ordinary
            // expression a `Match` can scrutinize directly.
            ast::Stmt::Let {
                pattern: p @ (ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. }),
                value,
                ..
            } => {
                let (tag, bindings, nested) = lower_tag_pattern(p, ctx)?;
                let body = wrap_nested_destructures(nested, ctx, result)?;
                ir::Expr::Match {
                    scrutinee: Box::new(lower_expr(value, ctx)?),
                    arms: vec![ir::MatchArm {
                        tag,
                        bindings,
                        guard: None,
                        body,
                    }],
                }
            }
            ast::Stmt::Let { pattern, .. } => {
                return Err(plum_syntax::error::CompileError::new(
                    pattern.span(),
                    "lowering not yet implemented for destructuring let-bindings of this \
                     shape (only tuples, structs, and plain identifiers so far)",
                ));
            }
            ast::Stmt::Expr(e) => ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(lower_expr(e, ctx)?),
                body: Box::new(result),
            },
            // Nothing checks `name` was actually declared `let mut` —
            // that's a static mutability check that doesn't exist yet
            // at this layer (or any layer); see ir.rs's `Assign` doc
            // comment.
            ast::Stmt::Assign { name, value, .. } => ir::Expr::Assign {
                name: name.clone(),
                value: Box::new(lower_expr(value, ctx)?),
                rest: Box::new(result),
            },
        };
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    fn lower(src: &str) -> ir::Expr {
        lower_with(src, &LoweringContext::new())
    }

    fn lower_err(src: &str) -> String {
        lower_with_err(src, &LoweringContext::new())
    }

    fn lower_with(src: &str, ctx: &LoweringContext) -> ir::Expr {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        lower_expr(&ast, ctx).unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"))
    }

    /// `lower`, but first stamps `ctx.closure_types` with `(param_tys,
    /// ret_ty)` at the span of `src`'s ONE closure literal (`src` must
    /// be shaped like `<base>.<map|filter|fold>(..., |...| ..., ...)`
    /// with exactly one closure-literal ARGUMENT) — the hand-built
    /// stand-in for what `plum_types::Infer::resolve_closure_types`
    /// would have already populated by the time real lowering runs (see
    /// `closure_result_types`'s doc comment). Needed because `.map()`/
    /// `.filter()` now require a resolved closure type to pick their
    /// empty-accumulator's element `PrimTy`.
    fn lower_array_op_with_closure_types(src: &str, param_tys: Vec<plum_types::types::Type>, ret_ty: plum_types::types::Type) -> ir::Expr {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ast::Expr::Call { args, .. } = &ast else {
            panic!("expected a `.map`/`.filter`/`.fold` call expression, got {ast:?}");
        };
        let closure_span = args
            .iter()
            .find_map(|a| match a {
                ast::Expr::Closure { span, .. } => Some(*span),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a closure-literal argument in {src:?}"));
        let mut closure_types = HashMap::new();
        closure_types.insert(closure_span, (param_tys, ret_ty));
        let ctx = LoweringContext::new().with_closure_types(closure_types);
        lower_expr(&ast, &ctx).unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"))
    }

    fn lower_with_err(src: &str, ctx: &LoweringContext) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        lower_expr(&ast, ctx).expect_err(&format!("expected lowering of {src:?} to fail")).to_string()
    }

    fn context_from_program(src: &str) -> LoweringContext {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        LoweringContext::from_items(&program.items)
    }

    // Field access needs `LoweringContext::field_owners` populated —
    // normally `plum-types` inference does this by walking the WHOLE
    // program and recording every `Expr::Field`'s span. These tests
    // only ever contain ONE field access, always as the outermost
    // expression, so its span is just the parsed expression's own span
    // — a real caller (`plumc`) has no such luxury and needs the real
    // inference pass instead (see the `plumc` end-to-end tests for that
    // proof).
    fn lower_field_access(struct_decls: &str, expr_src: &str, struct_name: &str) -> ir::Expr {
        let ctx = context_from_program(struct_decls);
        let tokens = Lexer::new(expr_src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {expr_src:?}: {e}"));
        let mut field_owners = HashMap::new();
        field_owners.insert(ast.span(), struct_name.to_string());
        let ctx = ctx.with_field_owners(field_owners);
        lower_expr(&ast, &ctx).unwrap_or_else(|e| panic!("lowering error for {expr_src:?}: {e}"))
    }

    #[test]
    fn literals() {
        assert_eq!(lower("5"), ir::Expr::Int(5));
        assert_eq!(lower("3.14"), ir::Expr::Float(3.14));
        assert_eq!(lower("\"hi\""), ir::Expr::Str("hi".to_string()));
        assert_eq!(lower("true"), ir::Expr::Bool(true));
        assert_eq!(lower("false"), ir::Expr::Bool(false));
    }

    #[test]
    fn empty_tuple_is_unit() {
        assert_eq!(lower("()"), ir::Expr::Unit);
    }

    #[test]
    fn variable() {
        assert_eq!(lower("x"), ir::Expr::Var("x".to_string()));
    }

    #[test]
    fn unary_ops() {
        assert_eq!(
            lower("-5"),
            ir::Expr::Unary(ir::UnOp::Neg, Box::new(ir::Expr::Int(5)))
        );
        assert_eq!(
            lower("!flag"),
            ir::Expr::Unary(ir::UnOp::Not, Box::new(ir::Expr::Var("flag".to_string())))
        );
    }

    #[test]
    fn binary_arithmetic() {
        assert_eq!(
            lower("1 + 2"),
            ir::Expr::Binary(
                ir::BinOp::Add,
                Box::new(ir::Expr::Int(1)),
                Box::new(ir::Expr::Int(2))
            )
        );
    }

    #[test]
    fn binary_all_operators_map_correctly() {
        let cases = [
            ("a - b", ir::BinOp::Sub),
            ("a * b", ir::BinOp::Mul),
            ("a / b", ir::BinOp::Div),
            ("a % b", ir::BinOp::Rem),
            ("a == b", ir::BinOp::Eq),
            ("a != b", ir::BinOp::Ne),
            ("a < b", ir::BinOp::Lt),
            ("a > b", ir::BinOp::Gt),
            ("a <= b", ir::BinOp::Le),
            ("a >= b", ir::BinOp::Ge),
            ("a && b", ir::BinOp::And),
            ("a || b", ir::BinOp::Or),
        ];
        for (src, expected_op) in cases {
            let expected = ir::Expr::Binary(
                expected_op,
                Box::new(ir::Expr::Var("a".to_string())),
                Box::new(ir::Expr::Var("b".to_string())),
            );
            assert_eq!(lower(src), expected, "mismatch lowering {src:?}");
        }
    }

    #[test]
    fn if_with_else() {
        assert_eq!(
            lower("if true { 1 } else { 2 }"),
            ir::Expr::If {
                cond: Box::new(ir::Expr::Bool(true)),
                then_branch: Box::new(ir::Expr::Int(1)),
                else_branch: Box::new(ir::Expr::Int(2)),
            }
        );
    }

    #[test]
    fn if_without_else_defaults_to_unit() {
        assert_eq!(
            lower("if true { 1 }"),
            ir::Expr::If {
                cond: Box::new(ir::Expr::Bool(true)),
                then_branch: Box::new(ir::Expr::Int(1)),
                else_branch: Box::new(ir::Expr::Unit),
            }
        );
    }

    #[test]
    fn call() {
        assert_eq!(
            lower("f(1, 2)"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![ir::Expr::Int(1), ir::Expr::Int(2)],
            }
        );
    }

    #[test]
    fn call_no_args() {
        assert_eq!(
            lower("f()"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![],
            }
        );
    }

    // --- Pipe desugaring: DESIGN.md's "insert as last argument" rule,
    // implemented for the first time here — the parser deliberately
    // never bakes this in (see ast.rs's BinaryOp::Pipe comment).

    #[test]
    fn pipe_into_bare_identifier() {
        assert_eq!(
            lower("x |> f"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![ir::Expr::Var("x".to_string())],
            }
        );
    }

    #[test]
    fn pipe_into_explicit_call_appends_last() {
        assert_eq!(
            lower("x |> f(a, b)"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![
                    ir::Expr::Var("a".to_string()),
                    ir::Expr::Var("b".to_string()),
                    ir::Expr::Var("x".to_string()),
                ],
            }
        );
    }

    #[test]
    fn pipe_chain_is_nested_calls() {
        assert_eq!(
            lower("x |> f |> g"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("g".to_string())),
                args: vec![ir::Expr::Call {
                    callee: Box::new(ir::Expr::Var("f".to_string())),
                    args: vec![ir::Expr::Var("x".to_string())],
                }],
            }
        );
    }

    #[test]
    fn pipe_lhs_can_be_a_compound_expression() {
        assert_eq!(
            lower("a + b |> f"),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("f".to_string())),
                args: vec![ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("a".to_string())),
                    Box::new(ir::Expr::Var("b".to_string())),
                )],
            }
        );
    }

    // --- Blocks fold into nested lets; a discarded expression-
    // statement is `let _ = expr in rest`, the standard trick for
    // representing sequencing without a dedicated IR node.

    #[test]
    fn empty_block_is_unit() {
        assert_eq!(lower("{}"), ir::Expr::Unit);
    }

    #[test]
    fn block_with_only_tail_has_no_extra_wrapping() {
        assert_eq!(lower("{ 5 }"), ir::Expr::Int(5));
    }

    #[test]
    fn block_let_statement() {
        assert_eq!(
            lower("{ let x = 5; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("x".to_string())),
            }
        );
    }

    #[test]
    fn block_discarded_expression_statement() {
        assert_eq!(
            lower("{ 5; 6 }"),
            ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Int(6)),
            }
        );
    }

    #[test]
    fn block_multiple_lets_nest_in_order() {
        assert_eq!(
            lower("{ let x = 1; let y = 2; x + y }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(1)),
                body: Box::new(ir::Expr::Let {
                    name: "y".to_string(),
                    value: Box::new(ir::Expr::Int(2)),
                    body: Box::new(ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Var("y".to_string())),
                    )),
                }),
            }
        );
    }

    #[test]
    fn block_let_mut_lowers_like_plain_let() {
        // `let mut` itself still just introduces an ordinary binding —
        // `Assign` (below) is the new node, not a different flavor of
        // `Let`. Nothing at this layer distinguishes a `let mut`
        // binding from a plain one; see ir.rs's `Assign` doc comment
        // for why that's a deliberate, documented gap for now.
        assert_eq!(
            lower("{ let mut x = 5; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("x".to_string())),
            }
        );
    }

    #[test]
    fn block_assign_lowers_to_ir_assign() {
        assert_eq!(
            lower("{ let mut x = 5; x = 6; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Assign {
                    name: "x".to_string(),
                    value: Box::new(ir::Expr::Int(6)),
                    rest: Box::new(ir::Expr::Var("x".to_string())),
                }),
            }
        );
    }

    #[test]
    fn block_multiple_assigns_nest_in_order() {
        assert_eq!(
            lower("{ let mut x = 0; x = 1; x = 2; x }"),
            ir::Expr::Let {
                name: "x".to_string(),
                value: Box::new(ir::Expr::Int(0)),
                body: Box::new(ir::Expr::Assign {
                    name: "x".to_string(),
                    value: Box::new(ir::Expr::Int(1)),
                    rest: Box::new(ir::Expr::Assign {
                        name: "x".to_string(),
                        value: Box::new(ir::Expr::Int(2)),
                        rest: Box::new(ir::Expr::Var("x".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn assign_value_can_reference_the_current_binding() {
        // The classic accumulator shape: `sum = sum + i`.
        assert_eq!(
            lower("{ let mut sum = 0; sum = sum + 1; sum }"),
            ir::Expr::Let {
                name: "sum".to_string(),
                value: Box::new(ir::Expr::Int(0)),
                body: Box::new(ir::Expr::Assign {
                    name: "sum".to_string(),
                    value: Box::new(ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("sum".to_string())),
                        Box::new(ir::Expr::Int(1)),
                    )),
                    rest: Box::new(ir::Expr::Var("sum".to_string())),
                }),
            }
        );
    }

    // --- Struct literals: need a LoweringContext to resolve declared
    // field order, since a literal can specify fields in any order but
    // the IR's Ctor is positional.

    #[test]
    fn struct_literal_basic() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        assert_eq!(
            lower_with("Point { x: 1.0, y: 2.0 }", &ctx),
            ir::Expr::Ctor {
                tag: "Point".to_string(),
                fields: vec![ir::Expr::Float(1.0), ir::Expr::Float(2.0)],
            }
        );
    }

    #[test]
    fn struct_literal_field_order_is_independent_of_declared_order() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        // Fields written in the OPPOSITE order from the declaration —
        // the resulting Ctor must still put x before y, since that's
        // Ctor's positional slot 0 and 1 by declaration, not by
        // whatever order the programmer happened to write them in.
        assert_eq!(
            lower_with("Point { y: 2.0, x: 1.0 }", &ctx),
            ir::Expr::Ctor {
                tag: "Point".to_string(),
                fields: vec![ir::Expr::Float(1.0), ir::Expr::Float(2.0)],
            }
        );
    }

    #[test]
    fn struct_literal_unknown_type_is_an_error() {
        lower_with_err("Foo { x: 1.0 }", &LoweringContext::new());
    }

    #[test]
    fn struct_literal_missing_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0 }", &ctx);
    }

    #[test]
    fn struct_literal_unknown_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0, y: 2.0, z: 3.0 }", &ctx);
    }

    #[test]
    fn struct_literal_duplicate_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { x: 1.0, x: 2.0 }", &ctx);
    }

    // --- Variant construction: `Circle(1.0)`, `Shape.Circle(1.0)`,
    // and a bare zero-arity `None` all construct a `Ctor`, the
    // expression-side counterpart to variant PATTERNS.

    #[test]
    fn bare_variant_call_constructs_a_ctor() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("Circle(1.0)", &ctx),
            ir::Expr::Ctor {
                tag: "Circle".to_string(),
                fields: vec![ir::Expr::Float(1.0)],
            }
        );
    }

    #[test]
    fn qualified_variant_call_constructs_a_ctor() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("Shape.Circle(1.0)", &ctx),
            ir::Expr::Ctor {
                tag: "Circle".to_string(),
                fields: vec![ir::Expr::Float(1.0)],
            }
        );
    }

    #[test]
    fn multi_field_variant_call() {
        let ctx = context_from_program("enum Shape { Rectangle(Float, Float) }");
        assert_eq!(
            lower_with("Rectangle(1.0, 2.0)", &ctx),
            ir::Expr::Ctor {
                tag: "Rectangle".to_string(),
                fields: vec![ir::Expr::Float(1.0), ir::Expr::Float(2.0)],
            }
        );
    }

    #[test]
    fn bare_zero_arity_variant_constructs_a_ctor() {
        let ctx = context_from_program("enum Shape { Empty }");
        assert_eq!(
            lower_with("Empty", &ctx),
            ir::Expr::Ctor {
                tag: "Empty".to_string(),
                fields: vec![],
            }
        );
    }

    #[test]
    fn variant_call_wrong_arity_is_an_error() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        lower_with_err("Circle(1.0, 2.0)", &ctx);
    }

    #[test]
    fn bare_non_zero_arity_variant_eta_expands_into_a_closure() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("Circle", &ctx),
            ir::Expr::Closure {
                params: vec!["__ctor_arg0".to_string()],
                param_types: None,
                ret_type: None,
                body: Box::new(ir::Expr::Ctor {
                    tag: "Circle".to_string(),
                    fields: vec![ir::Expr::Var("__ctor_arg0".to_string())],
                }),
            }
        );
    }

    #[test]
    fn bare_multi_field_variant_eta_expands_with_one_param_per_field() {
        let ctx = context_from_program("enum Shape { Rectangle(Float, Float) }");
        assert_eq!(
            lower_with("Rectangle", &ctx),
            ir::Expr::Closure {
                params: vec!["__ctor_arg0".to_string(), "__ctor_arg1".to_string()],
                param_types: None,
                ret_type: None,
                body: Box::new(ir::Expr::Ctor {
                    tag: "Rectangle".to_string(),
                    fields: vec![
                        ir::Expr::Var("__ctor_arg0".to_string()),
                        ir::Expr::Var("__ctor_arg1".to_string()),
                    ],
                }),
            }
        );
    }

    #[test]
    fn ordinary_function_calls_are_unaffected_by_variant_lowering() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("double(5)", &ctx),
            ir::Expr::Call {
                callee: Box::new(ir::Expr::Var("double".to_string())),
                args: vec![ir::Expr::Int(5)],
            }
        );
    }

    #[test]
    fn variant_construction_and_pattern_round_trip() {
        // Construct via the expression side, immediately destructure
        // via the pattern side — proves both halves agree on the tag.
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("match Circle(1.0) { Circle(r) => r }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Ctor {
                    tag: "Circle".to_string(),
                    fields: vec![ir::Expr::Float(1.0)],
                }),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Circle".to_string(),
                    bindings: vec!["r".to_string()],
                    body: ir::Expr::Var("r".to_string()),
                }],
            }
        );
    }

    #[test]
    fn struct_literal_spread_lowers_to_a_match_that_rebuilds_the_ctor() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        assert_eq!(
            lower_with("Point { x: 1.0, ..other }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("other".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["__spread0".to_string(), "__spread1".to_string()],
                    body: ir::Expr::Ctor {
                        tag: "Point".to_string(),
                        fields: vec![ir::Expr::Float(1.0), ir::Expr::Var("__spread1".to_string())],
                    },
                }],
            }
        );
    }

    #[test]
    fn struct_literal_spread_with_every_field_overridden_still_binds_but_ignores_them() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        assert_eq!(
            lower_with("Point { x: 1.0, y: 2.0, ..other }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("other".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["__spread0".to_string(), "__spread1".to_string()],
                    body: ir::Expr::Ctor {
                        tag: "Point".to_string(),
                        fields: vec![ir::Expr::Float(1.0), ir::Expr::Float(2.0)],
                    },
                }],
            }
        );
    }

    #[test]
    fn struct_literal_spread_with_unknown_field_is_still_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("Point { z: 1.0, ..other }", &ctx);
    }

    // --- Field access (`p.x`) ---

    #[test]
    fn field_access_lowers_to_a_match_that_extracts_one_binding() {
        assert_eq!(
            lower_field_access("struct Point { x: Float, y: Float }", "p.x", "Point"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    tag: "Point".to_string(),
                    bindings: vec!["__field_result".to_string(), "_".to_string()],
                    guard: None,
                    body: ir::Expr::Var("__field_result".to_string()),
                }],
            }
        );
    }

    #[test]
    fn field_access_binds_the_wanted_field_at_its_declared_position() {
        assert_eq!(
            lower_field_access("struct Point { x: Float, y: Float }", "p.y", "Point"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    tag: "Point".to_string(),
                    bindings: vec!["_".to_string(), "__field_result".to_string()],
                    guard: None,
                    body: ir::Expr::Var("__field_result".to_string()),
                }],
            }
        );
    }

    #[test]
    fn field_access_on_an_unknown_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        let mut field_owners = HashMap::new();
        let tokens = Lexer::new("p.z").tokenize();
        let ast = Parser::new(tokens).parse_expr().unwrap();
        field_owners.insert(ast.span(), "Point".to_string());
        let ctx = ctx.with_field_owners(field_owners);
        lower_expr(&ast, &ctx).expect_err("expected lowering of \"p.z\" to fail");
    }

    #[test]
    fn field_access_without_a_resolved_owner_is_an_error() {
        // No `field_owners` entry at all — simulates lowering running
        // WITHOUT type inference first, which is exactly the case this
        // guards against (`plumc`'s real pipeline always runs inference
        // first; this is what protects a future caller that forgets to).
        let ctx = context_from_program("struct Point { x: Float, y: Float }");
        lower_with_err("p.x", &ctx);
    }

    // --- Match: variant patterns lower to tag + positional bindings.

    #[test]
    fn match_variant_arms() {
        assert_eq!(
            lower("match shape { Shape.Circle(r) => r, Shape.Rectangle(w, h) => w * h }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![
                    ir::MatchArm {
                        guard: None,
                        tag: "Circle".to_string(),
                        bindings: vec!["r".to_string()],
                        body: ir::Expr::Var("r".to_string()),
                    },
                    ir::MatchArm {
                        guard: None,
                        tag: "Rectangle".to_string(),
                        bindings: vec!["w".to_string(), "h".to_string()],
                        body: ir::Expr::Binary(
                            ir::BinOp::Mul,
                            Box::new(ir::Expr::Var("w".to_string())),
                            Box::new(ir::Expr::Var("h".to_string())),
                        ),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_zero_arg_variant() {
        assert_eq!(
            lower("match x { None => 0, Some(v) => v }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        guard: None,
                        tag: "None".to_string(),
                        bindings: vec![],
                        body: ir::Expr::Int(0),
                    },
                    ir::MatchArm {
                        guard: None,
                        tag: "Some".to_string(),
                        bindings: vec!["v".to_string()],
                        body: ir::Expr::Var("v".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_variant_with_wildcard_args() {
        assert_eq!(
            lower("match shape { Shape.Rectangle(_, _) => true }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Rectangle".to_string(),
                    bindings: vec!["_".to_string(), "_".to_string()],
                    body: ir::Expr::Bool(true),
                }],
            }
        );
    }

    #[test]
    fn match_bare_wildcard_arm_desugars_to_a_discarding_let() {
        // A SINGLE catch-all arm (as opposed to `_` used inside a
        // variant's args, which already worked — see
        // match_variant_with_wildcard_args) now works for ANY
        // scrutinee type, since it needs no tag inspection at all.
        assert_eq!(
            lower("match x { _ => 1 }"),
            ir::Expr::Let {
                name: "_".to_string(),
                value: Box::new(ir::Expr::Var("x".to_string())),
                body: Box::new(ir::Expr::Int(1)),
            }
        );
    }

    #[test]
    fn match_bare_ident_arm_binds_the_whole_scrutinee() {
        assert_eq!(
            lower("match x { y => y + 1 }"),
            ir::Expr::Let {
                name: "y".to_string(),
                value: Box::new(ir::Expr::Var("x".to_string())),
                body: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("y".to_string())),
                    Box::new(ir::Expr::Int(1)),
                )),
            }
        );
    }

    #[test]
    fn match_or_pattern_expands_to_one_arm_per_alternative() {
        assert_eq!(
            lower("match x { A(v) | B(v) => v }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Var("v".to_string()),
                    },
                    ir::MatchArm {
                        tag: "B".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Var("v".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_or_pattern_with_three_alternatives_expands_to_three_arms() {
        assert_eq!(
            lower("match x { A(v) | B(v) | C(v) => v }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Var("v".to_string()),
                    },
                    ir::MatchArm {
                        tag: "B".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Var("v".to_string()),
                    },
                    ir::MatchArm {
                        tag: "C".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Var("v".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_or_pattern_with_a_guard_applies_it_to_every_alternative() {
        assert_eq!(
            lower("match x { A(v) | B(v) if v > 0 => v }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: Some(Box::new(ir::Expr::Binary(
                            ir::BinOp::Gt,
                            Box::new(ir::Expr::Var("v".to_string())),
                            Box::new(ir::Expr::Int(0)),
                        ))),
                        body: ir::Expr::Var("v".to_string()),
                    },
                    ir::MatchArm {
                        tag: "B".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: Some(Box::new(ir::Expr::Binary(
                            ir::BinOp::Gt,
                            Box::new(ir::Expr::Var("v".to_string())),
                            Box::new(ir::Expr::Int(0)),
                        ))),
                        body: ir::Expr::Var("v".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_or_pattern_zero_arity_alternatives_works() {
        assert_eq!(
            lower("match x { A | B => 1 }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec![],
                        guard: None,
                        body: ir::Expr::Int(1),
                    },
                    ir::MatchArm {
                        tag: "B".to_string(),
                        bindings: vec![],
                        guard: None,
                        body: ir::Expr::Int(1),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_or_pattern_with_mismatched_bindings_is_an_error() {
        lower_err("match x { A(v) | B(w) => v }");
    }

    #[test]
    fn match_or_pattern_with_mismatched_arity_is_an_error() {
        lower_err("match x { A(v) | B(v, w) => v }");
    }

    #[test]
    fn match_or_pattern_with_a_nested_sub_pattern_is_not_yet_supported() {
        lower_err("match x { A((v, w)) | B((v, w)) => v }");
    }

    #[test]
    fn literal_match_desugars_to_an_if_else_chain() {
        assert_eq!(
            lower("match x { 1 => \"one\", 2 => \"two\", _ => \"many\" }"),
            ir::Expr::Let {
                name: "__match_scrutinee".to_string(),
                value: Box::new(ir::Expr::Var("x".to_string())),
                body: Box::new(ir::Expr::If {
                    cond: Box::new(ir::Expr::Binary(
                        ir::BinOp::Eq,
                        Box::new(ir::Expr::Var("__match_scrutinee".to_string())),
                        Box::new(ir::Expr::Int(1)),
                    )),
                    then_branch: Box::new(ir::Expr::Str("one".to_string())),
                    else_branch: Box::new(ir::Expr::If {
                        cond: Box::new(ir::Expr::Binary(
                            ir::BinOp::Eq,
                            Box::new(ir::Expr::Var("__match_scrutinee".to_string())),
                            Box::new(ir::Expr::Int(2)),
                        )),
                        then_branch: Box::new(ir::Expr::Str("two".to_string())),
                        else_branch: Box::new(ir::Expr::Str("many".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn literal_match_with_a_trailing_ident_binds_the_scrutinee() {
        assert_eq!(
            lower("match x { 1 => 10, other => other }"),
            ir::Expr::Let {
                name: "__match_scrutinee".to_string(),
                value: Box::new(ir::Expr::Var("x".to_string())),
                body: Box::new(ir::Expr::If {
                    cond: Box::new(ir::Expr::Binary(
                        ir::BinOp::Eq,
                        Box::new(ir::Expr::Var("__match_scrutinee".to_string())),
                        Box::new(ir::Expr::Int(1)),
                    )),
                    then_branch: Box::new(ir::Expr::Int(10)),
                    else_branch: Box::new(ir::Expr::Let {
                        name: "other".to_string(),
                        value: Box::new(ir::Expr::Var("__match_scrutinee".to_string())),
                        body: Box::new(ir::Expr::Var("other".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn literal_match_arm_guard_combines_with_the_equality_check_via_and() {
        assert_eq!(
            lower("match x { 1 if flag => 10, _ => 0 }"),
            ir::Expr::Let {
                name: "__match_scrutinee".to_string(),
                value: Box::new(ir::Expr::Var("x".to_string())),
                body: Box::new(ir::Expr::If {
                    cond: Box::new(ir::Expr::Binary(
                        ir::BinOp::And,
                        Box::new(ir::Expr::Binary(
                            ir::BinOp::Eq,
                            Box::new(ir::Expr::Var("__match_scrutinee".to_string())),
                            Box::new(ir::Expr::Int(1)),
                        )),
                        Box::new(ir::Expr::Var("flag".to_string())),
                    )),
                    then_branch: Box::new(ir::Expr::Int(10)),
                    else_branch: Box::new(ir::Expr::Int(0)),
                }),
            }
        );
    }

    #[test]
    fn literal_match_without_a_trailing_catchall_is_an_error() {
        lower_err("match x { 1 => 10, 2 => 20 }");
    }

    #[test]
    fn literal_match_with_a_guarded_trailing_arm_is_an_error() {
        lower_err("match x { 1 => 10, n if n > 0 => 1 }");
    }

    #[test]
    fn string_literal_match_desugars_correctly() {
        assert_eq!(
            lower("match s { \"a\" => 1, _ => 0 }"),
            ir::Expr::Let {
                name: "__match_scrutinee".to_string(),
                value: Box::new(ir::Expr::Var("s".to_string())),
                body: Box::new(ir::Expr::If {
                    cond: Box::new(ir::Expr::Binary(
                        ir::BinOp::Eq,
                        Box::new(ir::Expr::Var("__match_scrutinee".to_string())),
                        Box::new(ir::Expr::Str("a".to_string())),
                    )),
                    then_branch: Box::new(ir::Expr::Int(1)),
                    else_branch: Box::new(ir::Expr::Int(0)),
                }),
            }
        );
    }

    #[test]
    fn a_trailing_wildcard_mixed_into_a_tag_shaped_match_gets_the_default_arm_tag() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("match shape { Circle(r) => r, _ => 0.0 }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "Circle".to_string(),
                        bindings: vec!["r".to_string()],
                        guard: None,
                        body: ir::Expr::Var("r".to_string()),
                    },
                    ir::MatchArm {
                        tag: "0Default".to_string(),
                        bindings: vec![],
                        guard: None,
                        body: ir::Expr::Float(0.0),
                    },
                ],
            }
        );
    }

    #[test]
    fn a_trailing_ident_mixed_into_a_tag_shaped_match_binds_via_the_default_arm() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("match shape { Circle(r) => r, other => 0.0 }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "Circle".to_string(),
                        bindings: vec!["r".to_string()],
                        guard: None,
                        body: ir::Expr::Var("r".to_string()),
                    },
                    ir::MatchArm {
                        tag: "0Default".to_string(),
                        bindings: vec!["other".to_string()],
                        guard: None,
                        body: ir::Expr::Float(0.0),
                    },
                ],
            }
        );
    }

    #[test]
    fn a_guarded_default_arm_is_allowed_unlike_a_guarded_literal_match_catchall() {
        // Deliberately different from `literal_match_with_a_guarded_
        // trailing_arm_is_an_error`: the mixed-Ctor case already relies
        // on the SAME "no match arm for tag" runtime fallback ordinary
        // Ctor matching has always had, so a guard on the default arm
        // doesn't open any NEW exhaustiveness gap.
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        assert_eq!(
            lower_with("match shape { Circle(r) => r, other if other == shape => 0.0 }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("shape".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "Circle".to_string(),
                        bindings: vec!["r".to_string()],
                        guard: None,
                        body: ir::Expr::Var("r".to_string()),
                    },
                    ir::MatchArm {
                        tag: "0Default".to_string(),
                        bindings: vec!["other".to_string()],
                        guard: Some(Box::new(ir::Expr::Binary(
                            ir::BinOp::Eq,
                            Box::new(ir::Expr::Var("other".to_string())),
                            Box::new(ir::Expr::Var("shape".to_string())),
                        ))),
                        body: ir::Expr::Float(0.0),
                    },
                ],
            }
        );
    }

    #[test]
    fn a_non_last_wildcard_mixed_into_a_tag_shaped_match_is_still_an_error() {
        let ctx = context_from_program("enum Shape { Circle(Float) }");
        lower_with_err("match shape { _ => 0.0, Circle(r) => r }", &ctx);
    }

    #[test]
    fn match_guard_lowers_to_an_arm_with_a_guard_expression() {
        assert_eq!(
            lower("match x { A(v) if v > 0 => v, B(v) => v }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: Some(Box::new(ir::Expr::Binary(
                            ir::BinOp::Gt,
                            Box::new(ir::Expr::Var("v".to_string())),
                            Box::new(ir::Expr::Int(0)),
                        ))),
                        body: ir::Expr::Var("v".to_string()),
                    },
                    ir::MatchArm {
                        tag: "B".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Var("v".to_string()),
                    },
                ],
            }
        );
    }

    #[test]
    fn two_arms_may_share_a_tag_when_only_the_guarded_one_comes_first() {
        assert_eq!(
            lower("match x { A(v) if v > 0 => 1, A(v) => 0 }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("x".to_string())),
                arms: vec![
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: Some(Box::new(ir::Expr::Binary(
                            ir::BinOp::Gt,
                            Box::new(ir::Expr::Var("v".to_string())),
                            Box::new(ir::Expr::Int(0)),
                        ))),
                        body: ir::Expr::Int(1),
                    },
                    ir::MatchArm {
                        tag: "A".to_string(),
                        bindings: vec!["v".to_string()],
                        guard: None,
                        body: ir::Expr::Int(0),
                    },
                ],
            }
        );
    }

    #[test]
    fn match_guard_combined_with_a_nested_pattern_is_not_yet_supported() {
        lower_err("match p { (A(v), n) if v > 0 => n, (B(v), n) => n }");
    }

    // --- End to end: real parsed program source, not a synthetic
    // one-off expression — proves a struct declaration and its use
    // genuinely connect through a full parse_program() call.

    #[test]
    fn end_to_end_struct_from_real_program_source() {
        let src = "struct Point { x: Float, y: Float }\nlet origin = Point { x: 0.0, y: 0.0 }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);

        let ast::ItemKind::Let(def) = &program.items[1].kind else {
            panic!("expected the second item to be a let definition");
        };
        assert_eq!(
            lower_expr(&def.body, &ctx).unwrap(),
            ir::Expr::Ctor {
                tag: "Point".to_string(),
                fields: vec![ir::Expr::Float(0.0), ir::Expr::Float(0.0)],
            }
        );
    }

    // --- Explicit, honest gaps — not yet supported ---

    #[test]
    fn non_empty_tuple_lowers_to_a_ctor_with_a_synthetic_tag() {
        assert_eq!(
            lower("(1, 2)"),
            ir::Expr::Ctor {
                tag: "2Tuple".to_string(),
                fields: vec![ir::Expr::Int(1), ir::Expr::Int(2)],
            }
        );
    }

    #[test]
    fn three_element_tuple_uses_its_own_arity_tag() {
        assert_eq!(
            lower("(1, 2, 3)"),
            ir::Expr::Ctor {
                tag: "3Tuple".to_string(),
                fields: vec![ir::Expr::Int(1), ir::Expr::Int(2), ir::Expr::Int(3)],
            }
        );
    }

    #[test]
    fn block_let_tuple_destructure() {
        assert_eq!(
            lower("{ let (a, b) = (1, 2); a + b }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Ctor {
                    tag: "2Tuple".to_string(),
                    fields: vec![ir::Expr::Int(1), ir::Expr::Int(2)],
                }),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "2Tuple".to_string(),
                    bindings: vec!["a".to_string(), "b".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("a".to_string())),
                        Box::new(ir::Expr::Var("b".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn match_arm_tuple_pattern() {
        assert_eq!(
            lower("match p { (a, b) => a + b }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "2Tuple".to_string(),
                    bindings: vec!["a".to_string(), "b".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("a".to_string())),
                        Box::new(ir::Expr::Var("b".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn nested_pattern_inside_a_tuple_pattern_lowers_to_a_nested_match() {
        assert_eq!(
            lower("match p { (Point(x, y), b) => b }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "2Tuple".to_string(),
                    bindings: vec!["__nested0".to_string(), "b".to_string()],
                    body: ir::Expr::Match {
                        scrutinee: Box::new(ir::Expr::Var("__nested0".to_string())),
                        arms: vec![ir::MatchArm {
                            tag: "Point".to_string(),
                            bindings: vec!["x".to_string(), "y".to_string()],
                            guard: None,
                            body: ir::Expr::Var("b".to_string()),
                        }],
                    },
                }],
            }
        );
    }

    #[test]
    fn nested_pattern_inside_a_struct_destructuring_function_param() {
        // Proves nesting also flows through the synthetic-param-name
        // path, not just match arms directly.
        let program = lower_program(
            "struct Point { x: Int, y: Int }\n\
             struct Line { start: Point, end: Point }\n\
             let dx (Line { start: Point { x: x0, y: _ }, end: Point { x: x1, y: _ } }) = x1 - x0",
        );
        assert_eq!(program.functions[0].params, vec!["__param0".to_string()]);
        // Just prove it lowers successfully with the expected shape of
        // nesting (outer Line match wrapping two Point matches) rather
        // than asserting the whole tree by hand.
        let ir::Expr::Match { arms, .. } = &program.functions[0].body else {
            panic!("expected the body to be a Match");
        };
        assert_eq!(arms[0].tag, "Line");
        assert_eq!(arms[0].bindings.len(), 2);
        let ir::Expr::Match { .. } = &arms[0].body else {
            panic!("expected the Line arm's body to be a nested Match for `start`/`end`");
        };
    }

    #[test]
    fn range_as_a_standalone_expression_lowers_to_a_synthetic_ctor() {
        assert_eq!(
            lower("0..5"),
            ir::Expr::Ctor {
                tag: "0Range".to_string(),
                fields: vec![ir::Expr::Int(0), ir::Expr::Int(5)],
            }
        );
    }

    // --- Closures ---

    #[test]
    fn closure_lowers_to_ir_closure() {
        assert_eq!(
            lower("|x| x + 1"),
            ir::Expr::Closure {
                params: vec!["x".to_string()],
                param_types: None,
                ret_type: None,
                body: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("x".to_string())),
                    Box::new(ir::Expr::Int(1)),
                )),
            }
        );
    }

    #[test]
    fn closure_multiple_params() {
        assert_eq!(
            lower("|a, b| a + b"),
            ir::Expr::Closure {
                params: vec!["a".to_string(), "b".to_string()],
                param_types: None,
                ret_type: None,
                body: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("a".to_string())),
                    Box::new(ir::Expr::Var("b".to_string())),
                )),
            }
        );
    }

    #[test]
    fn closure_zero_params() {
        assert_eq!(
            lower("|| 5"),
            ir::Expr::Closure {
                params: vec![],
                param_types: None,
                ret_type: None,
                body: Box::new(ir::Expr::Int(5)),
            }
        );
    }

    #[test]
    fn closure_param_annotations_do_not_affect_lowering() {
        assert_eq!(lower("|x: Int| x"), lower("|x| x"));
    }

    #[test]
    fn closure_body_can_be_a_block() {
        assert_eq!(
            lower("|x| { let y = x + 1; y }"),
            ir::Expr::Closure {
                params: vec!["x".to_string()],
                param_types: None,
                ret_type: None,
                body: Box::new(ir::Expr::Let {
                    name: "y".to_string(),
                    value: Box::new(ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Int(1)),
                    )),
                    body: Box::new(ir::Expr::Var("y".to_string())),
                }),
            }
        );
    }

    // --- `for`/`unsafe`/`spawn` ---

    #[test]
    fn for_over_a_literal_range_lowers_to_ir_for() {
        assert_eq!(
            lower("for i in 0..5 { i }"),
            ir::Expr::For {
                var: "i".to_string(),
                start: Box::new(ir::Expr::Int(0)),
                end: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Var("i".to_string())),
            }
        );
    }

    #[test]
    fn for_range_bounds_can_be_arbitrary_expressions() {
        assert_eq!(
            lower("for i in a..(b + 1) { i }"),
            ir::Expr::For {
                var: "i".to_string(),
                start: Box::new(ir::Expr::Var("a".to_string())),
                end: Box::new(ir::Expr::Binary(
                    ir::BinOp::Add,
                    Box::new(ir::Expr::Var("b".to_string())),
                    Box::new(ir::Expr::Int(1)),
                )),
                body: Box::new(ir::Expr::Var("i".to_string())),
            }
        );
    }

    #[test]
    fn for_body_is_a_real_block_with_statements() {
        assert_eq!(
            lower("for i in 0..5 { let x = i; x }"),
            ir::Expr::For {
                var: "i".to_string(),
                start: Box::new(ir::Expr::Int(0)),
                end: Box::new(ir::Expr::Int(5)),
                body: Box::new(ir::Expr::Let {
                    name: "x".to_string(),
                    value: Box::new(ir::Expr::Var("i".to_string())),
                    body: Box::new(ir::Expr::Var("x".to_string())),
                }),
            }
        );
    }

    #[test]
    fn for_over_a_variable_holding_a_range_destructures_it_via_match() {
        assert_eq!(
            lower("for i in xs { i }"),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("xs".to_string())),
                arms: vec![ir::MatchArm {
                    tag: "0Range".to_string(),
                    bindings: vec!["__range_start".to_string(), "__range_end".to_string()],
                    guard: None,
                    body: ir::Expr::For {
                        var: "i".to_string(),
                        start: Box::new(ir::Expr::Var("__range_start".to_string())),
                        end: Box::new(ir::Expr::Var("__range_end".to_string())),
                        body: Box::new(ir::Expr::Var("i".to_string())),
                    },
                }],
            }
        );
    }

    #[test]
    fn for_destructuring_pattern_is_not_yet_supported() {
        lower_err("for (a, b) in 0..5 { a }");
    }

    #[test]
    fn unsafe_block_lowers_transparently() {
        assert_eq!(lower("unsafe { 1 + 2 }"), lower("{ 1 + 2 }"));
    }

    #[test]
    fn spawn_lowers_to_a_spawn_node_wrapping_the_block() {
        assert_eq!(
            lower("spawn { 1 }"),
            ir::Expr::Spawn {
                block: Box::new(ir::Expr::Int(1)),
            }
        );
    }

    #[test]
    fn task_join_lowers_to_a_task_join_node() {
        assert_eq!(
            lower("t.join()"),
            ir::Expr::TaskJoin {
                task: Box::new(ir::Expr::Var("t".to_string())),
            }
        );
    }

    #[test]
    fn task_join_on_a_compound_expression() {
        assert_eq!(
            lower("(spawn { 1 }).join()"),
            ir::Expr::TaskJoin {
                task: Box::new(ir::Expr::Spawn {
                    block: Box::new(ir::Expr::Int(1)),
                }),
            }
        );
    }

    #[test]
    fn channel_generic_instantiation_lowers_to_a_channel_node() {
        assert_eq!(lower("channel[Int]()"), ir::Expr::Channel);
    }

    #[test]
    fn channel_send_lowers_to_a_channel_send_node() {
        assert_eq!(
            lower("tx.send(5)"),
            ir::Expr::ChannelSend {
                sender: Box::new(ir::Expr::Var("tx".to_string())),
                value: Box::new(ir::Expr::Int(5)),
            }
        );
    }

    #[test]
    fn channel_recv_lowers_to_a_channel_recv_node() {
        assert_eq!(
            lower("rx.recv()"),
            ir::Expr::ChannelRecv {
                receiver: Box::new(ir::Expr::Var("rx".to_string())),
            }
        );
    }

    // --- `select` ---

    #[test]
    fn select_with_ident_arms_lowers_to_a_select_node() {
        assert_eq!(
            lower("select { v = rx1.recv() => v, w = rx2.recv() => w }"),
            ir::Expr::Select {
                arms: vec![
                    ir::SelectArm {
                        receiver: ir::Expr::Var("rx1".to_string()),
                        body: ir::Expr::Let {
                            name: "v".to_string(),
                            value: Box::new(ir::Expr::Var("__select_recv".to_string())),
                            body: Box::new(ir::Expr::Var("v".to_string())),
                        },
                    },
                    ir::SelectArm {
                        receiver: ir::Expr::Var("rx2".to_string()),
                        body: ir::Expr::Let {
                            name: "w".to_string(),
                            value: Box::new(ir::Expr::Var("__select_recv".to_string())),
                            body: Box::new(ir::Expr::Var("w".to_string())),
                        },
                    },
                ],
            }
        );
    }

    #[test]
    fn select_wildcard_arm_needs_no_binding() {
        assert_eq!(
            lower("select { _ = rx.recv() => 0 }"),
            ir::Expr::Select {
                arms: vec![ir::SelectArm {
                    receiver: ir::Expr::Var("rx".to_string()),
                    body: ir::Expr::Int(0),
                }],
            }
        );
    }

    #[test]
    fn select_tuple_pattern_arm_reuses_wrap_destructure() {
        assert_eq!(
            lower("select { (a, b) = rx.recv() => a }"),
            ir::Expr::Select {
                arms: vec![ir::SelectArm {
                    receiver: ir::Expr::Var("rx".to_string()),
                    body: ir::Expr::Match {
                        scrutinee: Box::new(ir::Expr::Var("__select_recv".to_string())),
                        arms: vec![ir::MatchArm {
                            tag: "2Tuple".to_string(),
                            bindings: vec!["a".to_string(), "b".to_string()],
                            guard: None,
                            body: ir::Expr::Var("a".to_string()),
                        }],
                    },
                }],
            }
        );
    }

    #[test]
    fn select_arm_not_shaped_like_a_recv_call_is_an_error() {
        lower_err("select { v = 5 => v }");
    }

    #[test]
    fn select_arm_calling_something_other_than_recv_is_an_error() {
        lower_err("select { v = rx.join() => v }");
    }

    // --- Arrays ---

    #[test]
    fn array_literal_lowers_to_a_ctor_with_a_synthetic_tag() {
        assert_eq!(
            lower("[1, 2, 3]"),
            ir::Expr::Ctor {
                tag: "0Array".to_string(),
                fields: vec![ir::Expr::Int(1), ir::Expr::Int(2), ir::Expr::Int(3)],
            }
        );
    }

    #[test]
    fn empty_array_literal_lowers_to_a_ctor_with_no_fields() {
        assert_eq!(
            lower("[]"),
            ir::Expr::Ctor {
                tag: "0Array".to_string(),
                fields: vec![],
            }
        );
    }

    #[test]
    fn array_index_lowers_to_an_index_node() {
        assert_eq!(
            lower("arr[0]"),
            ir::Expr::Index {
                base: Box::new(ir::Expr::Var("arr".to_string())),
                index: Box::new(ir::Expr::Int(0)),
            }
        );
    }

    #[test]
    fn array_len_lowers_to_an_array_len_node() {
        assert_eq!(
            lower("arr.len()"),
            ir::Expr::ArrayLen {
                array: Box::new(ir::Expr::Var("arr".to_string())),
            }
        );
    }

    #[test]
    fn array_push_lowers_to_an_array_push_node() {
        assert_eq!(
            lower("arr.push(5)"),
            ir::Expr::ArrayPush {
                array: Box::new(ir::Expr::Var("arr".to_string())),
                value: Box::new(ir::Expr::Int(5)),
            }
        );
    }

    #[test]
    fn array_pop_lowers_to_an_array_pop_node() {
        assert_eq!(
            lower("arr.pop()"),
            ir::Expr::ArrayPop {
                array: Box::new(ir::Expr::Var("arr".to_string())),
            }
        );
    }

    #[test]
    fn array_set_lowers_to_an_array_set_node() {
        assert_eq!(
            lower("arr.set(0, 5)"),
            ir::Expr::ArraySet {
                array: Box::new(ir::Expr::Var("arr".to_string())),
                index: Box::new(ir::Expr::Int(0)),
                value: Box::new(ir::Expr::Int(5)),
            }
        );
    }

    #[test]
    fn array_remove_lowers_to_an_array_remove_node() {
        assert_eq!(
            lower("arr.remove(0)"),
            ir::Expr::ArrayRemove {
                array: Box::new(ir::Expr::Var("arr".to_string())),
                index: Box::new(ir::Expr::Int(0)),
            }
        );
    }

    #[test]
    fn string_literal_lowers_to_a_str_node() {
        assert_eq!(lower("\"hi\""), ir::Expr::Str("hi".to_string()));
    }

    #[test]
    fn string_len_lowers_to_the_same_array_len_node() {
        // Deliberately shared with arrays — lowering has no type info
        // to tell them apart; `Interpreter::eval` dispatches at
        // runtime. See `ArrayLen`'s eval case.
        assert_eq!(
            lower("s.len()"),
            ir::Expr::ArrayLen {
                array: Box::new(ir::Expr::Var("s".to_string())),
            }
        );
    }

    #[test]
    fn string_concat_lowers_to_a_str_concat_node() {
        assert_eq!(
            lower("s.concat(\"x\")"),
            ir::Expr::StrConcat {
                base: Box::new(ir::Expr::Var("s".to_string())),
                other: Box::new(ir::Expr::Str("x".to_string())),
            }
        );
    }

    #[test]
    fn string_index_lowers_to_the_same_index_node_as_arrays() {
        // Deliberately shared — see `string_len_lowers_to_the_same_
        // array_len_node`'s comment for the identical reasoning.
        assert_eq!(
            lower("s[0]"),
            ir::Expr::Index {
                base: Box::new(ir::Expr::Var("s".to_string())),
                index: Box::new(ir::Expr::Int(0)),
            }
        );
    }

    #[test]
    fn string_runes_lowers_to_a_str_runes_node() {
        assert_eq!(
            lower("s.runes()"),
            ir::Expr::StrRunes {
                base: Box::new(ir::Expr::Var("s".to_string())),
            }
        );
    }

    #[test]
    fn string_trim_lowers_to_a_str_trim_node() {
        assert_eq!(
            lower("s.trim()"),
            ir::Expr::StrTrim {
                base: Box::new(ir::Expr::Var("s".to_string())),
            }
        );
    }

    #[test]
    fn string_split_lowers_to_a_str_split_node() {
        assert_eq!(
            lower("s.split(\",\")"),
            ir::Expr::StrSplit {
                base: Box::new(ir::Expr::Var("s".to_string())),
                sep: Box::new(ir::Expr::Str(",".to_string())),
            }
        );
    }

    #[test]
    fn string_to_upper_lowers_to_a_str_to_upper_node() {
        assert_eq!(
            lower("s.to_upper()"),
            ir::Expr::StrToUpper {
                base: Box::new(ir::Expr::Var("s".to_string())),
            }
        );
    }

    #[test]
    fn string_to_lower_lowers_to_a_str_to_lower_node() {
        assert_eq!(
            lower("s.to_lower()"),
            ir::Expr::StrToLower {
                base: Box::new(ir::Expr::Var("s".to_string())),
            }
        );
    }

    #[test]
    fn string_contains_lowers_to_a_str_contains_node() {
        assert_eq!(
            lower("s.contains(\"x\")"),
            ir::Expr::StrContains {
                base: Box::new(ir::Expr::Var("s".to_string())),
                needle: Box::new(ir::Expr::Str("x".to_string())),
            }
        );
    }

    #[test]
    fn string_starts_with_lowers_to_a_str_starts_with_node() {
        assert_eq!(
            lower("s.starts_with(\"x\")"),
            ir::Expr::StrStartsWith {
                base: Box::new(ir::Expr::Var("s".to_string())),
                prefix: Box::new(ir::Expr::Str("x".to_string())),
            }
        );
    }

    #[test]
    fn string_ends_with_lowers_to_a_str_ends_with_node() {
        assert_eq!(
            lower("s.ends_with(\"x\")"),
            ir::Expr::StrEndsWith {
                base: Box::new(ir::Expr::Var("s".to_string())),
                suffix: Box::new(ir::Expr::Str("x".to_string())),
            }
        );
    }

    #[test]
    fn string_replace_lowers_to_a_str_replace_node() {
        assert_eq!(
            lower("s.replace(\"x\", \"y\")"),
            ir::Expr::StrReplace {
                base: Box::new(ir::Expr::Var("s".to_string())),
                from: Box::new(ir::Expr::Str("x".to_string())),
                to: Box::new(ir::Expr::Str("y".to_string())),
            }
        );
    }

    #[test]
    fn to_string_lowers_to_a_to_string_node() {
        assert_eq!(
            lower("x.to_string()"),
            ir::Expr::ToString {
                base: Box::new(ir::Expr::Var("x".to_string())),
            }
        );
    }

    #[test]
    fn ref_new_lowers_to_a_ref_new_node() {
        assert_eq!(
            lower("ref(5)"),
            ir::Expr::RefNew {
                value: Box::new(ir::Expr::Int(5)),
            }
        );
    }

    #[test]
    fn ref_get_lowers_to_a_ref_get_node() {
        assert_eq!(
            lower("r.get()"),
            ir::Expr::RefGet {
                base: Box::new(ir::Expr::Var("r".to_string())),
            }
        );
    }

    #[test]
    fn ref_set_lowers_to_a_ref_set_node() {
        assert_eq!(
            lower("r.set(6)"),
            ir::Expr::RefSet {
                base: Box::new(ir::Expr::Var("r".to_string())),
                value: Box::new(ir::Expr::Int(6)),
            }
        );
    }

    #[test]
    fn array_set_with_two_args_still_lowers_to_array_set_not_ref_set() {
        // Arity disambiguates `Array`'s `.set(i, v)` from `Ref`'s
        // `.set(v)` — confirms the two shape-detection rules coexist
        // without either shadowing the other.
        assert_eq!(
            lower("arr.set(0, 9)"),
            ir::Expr::ArraySet {
                array: Box::new(ir::Expr::Var("arr".to_string())),
                index: Box::new(ir::Expr::Int(0)),
                value: Box::new(ir::Expr::Int(9)),
            }
        );
    }

    #[test]
    fn array_map_desugars_to_an_index_based_loop_that_pushes() {
        // `f`'s resolved type (`Int -> Int`, matching the identity
        // closure written here) is what the empty output accumulator's
        // `EmptyArray(PrimTy::Int)` element type now comes from — see
        // `closure_result_types`'s doc comment; `lower_array_op_with_
        // closure_types` stands in for the real `plum_types::Infer`
        // pass that would have populated this in the real pipeline.
        let closure = ir::Expr::Closure {
            params: vec!["x".to_string()],
            param_types: Some(vec![ir::PrimTy::Int]),
            ret_type: Some(ir::PrimTy::Int),
            body: Box::new(ir::Expr::Var("x".to_string())),
        };
        assert_eq!(
            lower_array_op_with_closure_types(
                "arr.map(|x| x)",
                vec![plum_types::types::Type::Int],
                plum_types::types::Type::Int,
            ),
            ir::Expr::Let {
                name: "__map_arr".to_string(),
                value: Box::new(ir::Expr::Var("arr".to_string())),
                body: Box::new(ir::Expr::Let {
                    name: "__map_out".to_string(),
                    value: Box::new(ir::Expr::EmptyArray(ir::PrimTy::Int)),
                    body: Box::new(ir::Expr::Let {
                        name: "_".to_string(),
                        value: Box::new(ir::Expr::For {
                            var: "__map_i".to_string(),
                            start: Box::new(ir::Expr::Int(0)),
                            end: Box::new(ir::Expr::ArrayLen {
                                array: Box::new(ir::Expr::Var("__map_arr".to_string())),
                            }),
                            body: Box::new(ir::Expr::Assign {
                                name: "__map_out".to_string(),
                                value: Box::new(ir::Expr::ArrayPush {
                                    array: Box::new(ir::Expr::Var("__map_out".to_string())),
                                    value: Box::new(ir::Expr::Call {
                                        callee: Box::new(closure),
                                        args: vec![ir::Expr::Index {
                                            base: Box::new(ir::Expr::Var("__map_arr".to_string())),
                                            index: Box::new(ir::Expr::Var("__map_i".to_string())),
                                        }],
                                    }),
                                }),
                                rest: Box::new(ir::Expr::Unit),
                            }),
                        }),
                        body: Box::new(ir::Expr::Var("__map_out".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn array_filter_desugars_to_an_index_based_loop_with_an_if() {
        // `f`'s resolved param type (`Int`, matching the predicate
        // closure written here) is what the empty output accumulator's
        // element type now comes from (the SAME type the input array's
        // elements have — filter never changes element type).
        let result = lower_array_op_with_closure_types(
            "arr.filter(|x| x > 0)",
            vec![plum_types::types::Type::Int],
            plum_types::types::Type::Bool,
        );
        match result {
            ir::Expr::Let { name, body, .. } => {
                assert_eq!(name, "__filter_arr");
                match *body {
                    ir::Expr::Let { name, value, body } => {
                        assert_eq!(name, "__filter_out");
                        assert_eq!(*value, ir::Expr::EmptyArray(ir::PrimTy::Int));
                        match *body {
                            ir::Expr::Let { value, body, .. } => {
                                assert!(matches!(*value, ir::Expr::For { .. }));
                                assert_eq!(*body, ir::Expr::Var("__filter_out".to_string()));
                            }
                            other => panic!("expected inner Let, got {other:?}"),
                        }
                    }
                    other => panic!("expected __filter_out Let, got {other:?}"),
                }
            }
            other => panic!("expected outer Let, got {other:?}"),
        }
    }

    #[test]
    fn array_fold_desugars_to_an_index_based_accumulator_loop() {
        assert_eq!(
            lower("arr.fold(0, f)"),
            ir::Expr::Let {
                name: "__fold_arr".to_string(),
                value: Box::new(ir::Expr::Var("arr".to_string())),
                body: Box::new(ir::Expr::Let {
                    name: "__fold_acc".to_string(),
                    value: Box::new(ir::Expr::Int(0)),
                    body: Box::new(ir::Expr::Let {
                        name: "_".to_string(),
                        value: Box::new(ir::Expr::For {
                            var: "__fold_i".to_string(),
                            start: Box::new(ir::Expr::Int(0)),
                            end: Box::new(ir::Expr::ArrayLen {
                                array: Box::new(ir::Expr::Var("__fold_arr".to_string())),
                            }),
                            body: Box::new(ir::Expr::Assign {
                                name: "__fold_acc".to_string(),
                                value: Box::new(ir::Expr::Call {
                                    callee: Box::new(ir::Expr::Var("f".to_string())),
                                    args: vec![
                                        ir::Expr::Var("__fold_acc".to_string()),
                                        ir::Expr::Index {
                                            base: Box::new(ir::Expr::Var("__fold_arr".to_string())),
                                            index: Box::new(ir::Expr::Var("__fold_i".to_string())),
                                        },
                                    ],
                                }),
                                rest: Box::new(ir::Expr::Unit),
                            }),
                        }),
                        body: Box::new(ir::Expr::Var("__fold_acc".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn for_over_an_array_desugars_to_an_index_based_loop() {
        let tokens = Lexer::new("for x in [1, 2, 3] { x }").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap();
        let mut array_for_loops = std::collections::HashSet::new();
        array_for_loops.insert(ast.span());
        let ctx = LoweringContext::new().with_array_for_loops(array_for_loops);
        let result = lower_expr(&ast, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        assert_eq!(
            result,
            ir::Expr::Let {
                name: "__for_arr".to_string(),
                value: Box::new(ir::Expr::Ctor {
                    tag: "0Array".to_string(),
                    fields: vec![ir::Expr::Int(1), ir::Expr::Int(2), ir::Expr::Int(3)],
                }),
                body: Box::new(ir::Expr::For {
                    var: "__for_i".to_string(),
                    start: Box::new(ir::Expr::Int(0)),
                    end: Box::new(ir::Expr::ArrayLen {
                        array: Box::new(ir::Expr::Var("__for_arr".to_string())),
                    }),
                    body: Box::new(ir::Expr::Let {
                        name: "x".to_string(),
                        value: Box::new(ir::Expr::Index {
                            base: Box::new(ir::Expr::Var("__for_arr".to_string())),
                            index: Box::new(ir::Expr::Var("__for_i".to_string())),
                        }),
                        body: Box::new(ir::Expr::Var("x".to_string())),
                    }),
                }),
            }
        );
    }

    #[test]
    fn a_for_loop_not_recorded_in_array_for_loops_still_desugars_as_a_range() {
        // With no `array_for_loops` entry at all (the default —
        // exactly what a lowering-only test or a Range-typed loop
        // gets), a non-literal iterand still falls through to the
        // pre-existing Range-Match-unwrap desugaring, unchanged.
        assert!(matches!(lower("for x in r { x }"), ir::Expr::Match { .. }));
    }

    // --- Item-level lowering: `let`-defined functions -> ir::Function

    fn lower_program(src: &str) -> ir::Program {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        super::lower_program(&program, &ctx).unwrap_or_else(|e| panic!("program lowering error for {src:?}: {e}"))
    }

    fn lower_program_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        super::lower_program(&program, &ctx).expect_err(&format!("expected lowering of {src:?} to fail")).to_string()
    }

    #[test]
    fn single_param_function() {
        let program = lower_program("let double n = n * 2");
        assert_eq!(
            program.functions,
            vec![ir::Function {
                name: "double".to_string(),
                params: vec!["n".to_string()],
                body: ir::Expr::Binary(
                    ir::BinOp::Mul,
                    Box::new(ir::Expr::Var("n".to_string())),
                    Box::new(ir::Expr::Int(2)),
                ),
            }]
        );
    }

    #[test]
    fn unit_pattern_param_needs_no_destructuring_match() {
        // `()` binds nothing and isn't a heap `Ctor` — this should
        // lower to a plain synthetic (never-referenced) param name,
        // NOT a `Match` wrapping the body (there's nothing to
        // destructure a `Unit` value INTO).
        let program = lower_program("let go () = 5");
        assert_eq!(program.functions[0].params, vec!["__unit_param0".to_string()]);
        assert_eq!(program.functions[0].body, ir::Expr::Int(5));
    }

    #[test]
    fn annotations_do_not_affect_lowering() {
        let annotated = lower_program("let double (n: Int): Int = n * 2");
        let bare = lower_program("let double n = n * 2");
        assert_eq!(annotated.functions, bare.functions);
    }

    #[test]
    fn multi_param_function() {
        let program = lower_program("let add a b = a + b");
        assert_eq!(program.functions[0].params, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn generics_are_ignored_not_rejected() {
        // No type checker exists yet, so a type parameter has no
        // runtime effect — this is deliberate erasure, not a missing
        // feature. Proven by lowering succeeding at all here.
        let program = lower_program("let identity[T] (x: T): T = x");
        assert_eq!(program.functions[0].params, vec!["x".to_string()]);
    }

    #[test]
    fn struct_and_enum_and_use_items_produce_no_functions() {
        let program = lower_program(
            "struct Point { x: Float, y: Float }\n\
             enum Shape { Circle(Float) }\n\
             use shapes;\n\
             let double n = n * 2",
        );
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].name, "double");
    }

    #[test]
    fn multiple_functions_lower_in_order() {
        let program = lower_program("let square x = x * x\nlet cube x = x * x * x");
        let names: Vec<&str> = program.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["square", "cube"]);
    }

    #[test]
    fn zero_param_let_lowers_to_a_global() {
        let program = lower_program("let x = 5");
        assert_eq!(program.functions, vec![]);
        assert_eq!(
            program.globals,
            vec![ir::Global {
                name: "x".to_string(),
                value: ir::Expr::Int(5),
            }]
        );
    }

    #[test]
    fn multiple_globals_lower_in_order() {
        let program = lower_program("let a = 1\nlet b = 2");
        let names: Vec<&str> = program.globals.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn a_global_can_reference_an_earlier_global() {
        let program = lower_program("let a = 1\nlet b = a + 1");
        assert_eq!(
            program.globals[1].value,
            ir::Expr::Binary(ir::BinOp::Add, Box::new(ir::Expr::Var("a".to_string())), Box::new(ir::Expr::Int(1)))
        );
    }

    #[test]
    fn globals_and_functions_can_be_interleaved_in_source_order() {
        let program = lower_program("let a = 1\nlet double n = n * 2\nlet b = 2");
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].name, "double");
        let global_names: Vec<&str> = program.globals.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(global_names, vec!["a", "b"]);
    }

    #[test]
    fn tuple_destructuring_param_wraps_the_body_in_a_match() {
        // The flagship example from examples/overview.plum, now real —
        // a synthetic name seeds the flat param list, then a `Match`
        // destructures it before the real body runs.
        let program = lower_program("let swap (a, b) = (b, a)");
        assert_eq!(
            program.functions,
            vec![ir::Function {
                name: "swap".to_string(),
                params: vec!["__param0".to_string()],
                body: ir::Expr::Match {
                    scrutinee: Box::new(ir::Expr::Var("__param0".to_string())),
                    arms: vec![ir::MatchArm {
                        guard: None,
                        tag: "2Tuple".to_string(),
                        bindings: vec!["a".to_string(), "b".to_string()],
                        body: ir::Expr::Ctor {
                            tag: "2Tuple".to_string(),
                            fields: vec![ir::Expr::Var("b".to_string()), ir::Expr::Var("a".to_string())],
                        },
                    }],
                },
            }]
        );
    }

    #[test]
    fn tuple_destructuring_param_can_be_mixed_with_plain_params() {
        let program = lower_program("let f x (a, b) y = x + a + b + y");
        assert_eq!(program.functions[0].params, vec!["x", "__param1", "y"]);
    }

    #[test]
    fn struct_destructuring_param_wraps_the_body_in_a_match() {
        let program = lower_program("struct Point { x: Int, y: Int }\nlet area (Point { x, y }) = x * y");
        assert_eq!(program.functions[0].params, vec!["__param0".to_string()]);
        assert_eq!(
            program.functions[0].body,
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("__param0".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["x".to_string(), "y".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Mul,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Var("y".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn struct_destructuring_param_field_order_is_declared_order_not_written_order() {
        // Fields written in the OPPOSITE order from the declaration —
        // bindings must still come out x-then-y (declared order), same
        // guarantee struct literals already give.
        let program = lower_program("struct Point { x: Int, y: Int }\nlet area (Point { y, x }) = x * y");
        assert_eq!(
            program.functions[0].body,
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("__param0".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["x".to_string(), "y".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Mul,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Var("y".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn struct_destructuring_param_field_rename() {
        let program = lower_program("struct Point { x: Int, y: Int }\nlet area (Point { x: px, y: py }) = px * py");
        assert_eq!(
            program.functions[0].body,
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("__param0".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["px".to_string(), "py".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Mul,
                        Box::new(ir::Expr::Var("px".to_string())),
                        Box::new(ir::Expr::Var("py".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn struct_destructuring_param_with_rest_fills_omitted_fields_with_wildcard() {
        let program = lower_program("struct Point { x: Int, y: Int }\nlet get_x (Point { x, .. }) = x");
        assert_eq!(
            program.functions[0].body,
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("__param0".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["x".to_string(), "_".to_string()],
                    body: ir::Expr::Var("x".to_string()),
                }],
            }
        );
    }

    #[test]
    fn struct_destructuring_param_missing_field_without_rest_is_an_error() {
        lower_program_err("struct Point { x: Int, y: Int }\nlet get_x (Point { x }) = x");
    }

    #[test]
    fn struct_destructuring_param_unknown_field_is_an_error() {
        lower_program_err("struct Point { x: Int, y: Int }\nlet get_x (Point { x, y, z }) = x");
    }

    #[test]
    fn struct_destructuring_param_unknown_struct_is_an_error() {
        lower_program_err("let area (Point { x, y }) = x * y");
    }

    #[test]
    fn match_arm_struct_pattern() {
        let ctx = context_from_program("struct Point { x: Int, y: Int }");
        assert_eq!(
            lower_with("match p { Point { x, y } => x + y }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["x".to_string(), "y".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Var("y".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn block_let_struct_destructure() {
        let ctx = context_from_program("struct Point { x: Int, y: Int }");
        assert_eq!(
            lower_with("{ let Point { x, y } = p; x + y }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Point".to_string(),
                    bindings: vec!["x".to_string(), "y".to_string()],
                    body: ir::Expr::Binary(
                        ir::BinOp::Add,
                        Box::new(ir::Expr::Var("x".to_string())),
                        Box::new(ir::Expr::Var("y".to_string())),
                    ),
                }],
            }
        );
    }

    #[test]
    fn struct_pattern_duplicate_field_is_an_error() {
        let ctx = context_from_program("struct Point { x: Int, y: Int }");
        lower_with_err("match p { Point { x, x } => x }", &ctx);
    }

    #[test]
    fn struct_pattern_nested_pattern_lowers_to_a_nested_match() {
        let ctx = context_from_program("struct Inner { v: Int }\nstruct Outer { inner: Inner, y: Int }");
        assert_eq!(
            lower_with("match p { Outer { inner: Inner { v }, y } => v + y }", &ctx),
            ir::Expr::Match {
                scrutinee: Box::new(ir::Expr::Var("p".to_string())),
                arms: vec![ir::MatchArm {
                    guard: None,
                    tag: "Outer".to_string(),
                    bindings: vec!["__nested0".to_string(), "y".to_string()],
                    body: ir::Expr::Match {
                        scrutinee: Box::new(ir::Expr::Var("__nested0".to_string())),
                        arms: vec![ir::MatchArm {
                            tag: "Inner".to_string(),
                            bindings: vec!["v".to_string()],
                            guard: None,
                            body: ir::Expr::Binary(
                                ir::BinOp::Add,
                                Box::new(ir::Expr::Var("v".to_string())),
                                Box::new(ir::Expr::Var("y".to_string())),
                            ),
                        }],
                    },
                }],
            }
        );
    }

    #[test]
    fn nested_or_pattern_is_still_not_yet_supported() {
        // Nesting works for tag-based patterns (variant/tuple/struct)
        // — an or-pattern nested inside one is a genuinely separate,
        // still-unsupported gap (or-patterns aren't lowerable at all
        // yet, nested or not).
        let ctx = context_from_program("struct Point { x: Int, y: Int }");
        lower_with_err("match p { Point { x: 1 | 2, y } => y }", &ctx);
    }

    // --- extern "C" / .as_cstr() ---

    #[test]
    fn extern_call_lowers_to_an_extern_call_node_not_an_ordinary_call() {
        let ctx = context_from_program("extern \"C\" { fn sqrt(x: Float) -> Float; }");
        assert_eq!(
            lower_with("sqrt(2.0)", &ctx),
            ir::Expr::ExternCall {
                name: "sqrt".to_string(),
                args: vec![ir::Expr::Float(2.0)],
            }
        );
    }

    #[test]
    fn extern_block_populates_program_externs() {
        let program = lower_program("extern \"C\" { fn sqrt(x: Float) -> Float; fn abs(n: Int) -> Int; }");
        assert_eq!(
            program.externs,
            vec![
                ir::ExternFn {
                    name: "sqrt".to_string(),
                    param_types: vec![ir::ExternType::Float],
                    ret_type: Some(ir::ExternType::Float),
                },
                ir::ExternFn {
                    name: "abs".to_string(),
                    param_types: vec![ir::ExternType::Int],
                    ret_type: Some(ir::ExternType::Int),
                },
            ]
        );
    }

    #[test]
    fn extern_fn_with_a_void_return_has_no_ret_type() {
        let program = lower_program("extern \"C\" { fn srand(seed: Int); }");
        assert_eq!(program.externs[0].ret_type, None);
    }

    #[test]
    fn extern_fn_with_an_unsupported_param_type_is_a_lowering_error() {
        let err = lower_program_err("extern \"C\" { fn foo(x: Bool2) -> Int; }");
        assert!(err.contains("Int, Float, Bool, CStr"), "unexpected error: {err}");
    }

    #[test]
    fn cstr_is_a_supported_extern_type() {
        let program = lower_program("extern \"C\" { fn strlen(s: CStr) -> Int; }");
        assert_eq!(program.externs[0].param_types, vec![ir::ExternType::Str]);
    }

    #[test]
    fn a_struct_of_ffi_safe_fields_is_a_supported_extern_type() {
        let program =
            lower_program("struct Point { x: Int, y: Float } extern \"C\" { fn make_point(x: Int, y: Float) -> Point; }");
        assert_eq!(
            program.externs[0].ret_type,
            Some(ir::ExternType::Struct(
                "Point".to_string(),
                vec![ir::ExternType::Int, ir::ExternType::Float]
            ))
        );
    }

    #[test]
    fn a_nested_ffi_safe_struct_resolves_recursively() {
        let program = lower_program(
            "struct Inner { a: Int } struct Outer { inner: Inner, b: Int } \
             extern \"C\" { fn foo(o: Outer) -> Int; }",
        );
        assert_eq!(
            program.externs[0].param_types,
            vec![ir::ExternType::Struct(
                "Outer".to_string(),
                vec![
                    ir::ExternType::Struct("Inner".to_string(), vec![ir::ExternType::Int]),
                    ir::ExternType::Int
                ]
            )]
        );
    }

    #[test]
    fn a_self_referential_struct_is_rejected_as_an_extern_type() {
        let err = lower_program_err("struct Node { next: Node } extern \"C\" { fn foo(n: Node) -> Int; }");
        assert!(err.contains("self-referential"), "unexpected error: {err}");
    }

    #[test]
    fn a_struct_with_an_unsupported_field_type_is_rejected_as_an_extern_type() {
        let err = lower_program_err(
            "enum Color { Red } struct Shape { color: Color } extern \"C\" { fn foo(s: Shape) -> Int; }",
        );
        assert!(err.contains("Int, Float, Bool, CStr"), "unexpected error: {err}");
    }

    #[test]
    fn a_cstr_field_inside_a_struct_is_rejected_as_an_extern_type() {
        // Unlike `plum-types` (which rejects `struct Labeled { label:
        // CStr }` outright, since `CStr` isn't a resolvable ordinary
        // field annotation at all — see the matching `plum-types`
        // test), `lower.rs`'s `LoweringContext` stores a struct's field
        // types UNRESOLVED (raw `ast::Type`s — see `struct_field_types`'
        // doc comment), so it only rejects `CStr`-as-a-field lazily,
        // the moment something actually tries to resolve it as an
        // extern type — exercised here directly, independent of
        // `plum-types` having already caught it first.
        let err = lower_program_err("struct Labeled { label: CStr } extern \"C\" { fn foo(l: Labeled) -> Int; }");
        assert!(err.contains("top-level"), "unexpected error: {err}");
    }

    #[test]
    fn a_callback_type_resolves_to_extern_type_callback() {
        let program = lower_program("extern \"C\" { fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int; }");
        assert_eq!(
            program.externs[0].param_types,
            vec![ir::ExternType::Callback {
                params: vec![ir::ExternType::Int, ir::ExternType::Int],
                ret: Some(Box::new(ir::ExternType::Int)),
            }]
        );
    }

    #[test]
    fn a_void_callback_resolves_to_no_ret_type() {
        let program = lower_program("extern \"C\" { fn foo(f: () -> Unit) -> Int; }");
        assert_eq!(
            program.externs[0].param_types,
            vec![ir::ExternType::Callback { params: vec![], ret: None }]
        );
    }

    #[test]
    fn a_callback_return_type_is_rejected() {
        let err = lower_program_err("extern \"C\" { fn foo() -> (Int) -> Int; }");
        assert!(err.contains("return type"), "unexpected error: {err}");
    }

    #[test]
    fn a_nested_callback_type_is_rejected() {
        let err = lower_program_err("extern \"C\" { fn foo(f: ((Int) -> Int) -> Int) -> Int; }");
        assert!(err.contains("nested"), "unexpected error: {err}");
    }

    #[test]
    fn extern_call_with_a_bare_function_callback_argument_lowers_correctly() {
        let ctx = context_from_program("extern \"C\" { fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int; }");
        assert_eq!(
            lower_with("call_with_10_and_20(add)", &ctx),
            ir::Expr::ExternCall {
                name: "call_with_10_and_20".to_string(),
                args: vec![ir::Expr::Var("add".to_string())],
            }
        );
    }

    #[test]
    fn as_cstr_lowers_to_its_own_node() {
        assert_eq!(
            lower("\"hi\".as_cstr()"),
            ir::Expr::AsCStr(Box::new(ir::Expr::Str("hi".to_string())))
        );
    }

    #[test]
    fn extern_call_with_an_as_cstr_argument_lowers_correctly() {
        let ctx = context_from_program("extern \"C\" { fn strlen(s: CStr) -> Int; }");
        assert_eq!(
            lower_with("strlen(\"hi\".as_cstr())", &ctx),
            ir::Expr::ExternCall {
                name: "strlen".to_string(),
                args: vec![ir::Expr::AsCStr(Box::new(ir::Expr::Str("hi".to_string())))],
            }
        );
    }
}
