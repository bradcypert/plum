//! True monomorphization for generic structs/enums/functions — the pass
//! that makes LLVM codegen (`plum-codegen`) able to handle generics at
//! all. Unlike the interpreter (uniformly dynamically-tagged, so
//! `List[Int]` and `List[Bool]` are byte-identical heap cells) codegen's
//! uniform heap-cell scheme picks a different STORE/LOAD bit-conversion
//! per field depending on that field's concrete `CgType` — so two
//! different instantiations of the same generic declaration need
//! DISTINCT, mangled tags/function names, not just "allow generics
//! through." See DESIGN.md's generics/monomorphization section for the
//! full design writeup this module implements.
//!
//! High level: `plum_types::Infer::resolve_generic_sites` already turned
//! every generic construction/pattern/call site encountered during
//! inference into a `ResolvedSite` — a concrete argument list, or (for a
//! site nested inside another still-generic function's own body) a
//! `Type::Param` TEMPLATE referring to that function's own declared
//! generic. `plan` runs a fixpoint worklist over every REACHABLE
//! `(declaration, concrete args)` pair, discovering deeper ones as it
//! rewrites each generic function's body (substituting any template
//! `Param` through that function's own concrete binding), and produces:
//! a fully mangled, monomorphic `ir::Function` per reachable function
//! instantiation (PLUS one for every ordinary, already-non-generic
//! function — see `plan`'s own doc comment for why those need
//! rewriting too), a `TagFields`-shaped map for every reachable struct/
//! enum instantiation, and enough bookkeeping (`signatures`,
//! `entry_rename`) for `plumc::codegen_cli` to wire the result into
//! `plum_codegen::emit_program` as if it were ordinary, non-generic
//! input.
//!
//! Termination: `unify.rs`'s occurs check rejects any self-recursive
//! function/type whose own use would require a type strictly containing
//! itself, so the set of reachable `(decl, concrete_args)` pairs is
//! finite by construction — the worklist can't loop forever, and
//! `done_fns`/`done_types` (deduping by mangled name) guarantee each
//! reachable pair is only ever processed once.

use crate::ir;
use crate::lower::{lower_expr, lower_params, wrap_destructure, LoweringContext};
use plum_syntax::ast;
use plum_syntax::span::Span;
use plum_types::context::TypeContext;
use plum_types::infer::{ResolvedSite, SiteKind};
use plum_types::types::{Type, TypeVarId};
use std::collections::HashMap;

/// The result of monomorphization — see this module's own doc comment.
/// Every field here is meant to be merged into `plumc::codegen_cli`'s
/// existing non-generic derivation (disjoint by construction: a mangled
/// name can never collide with a plain one, since `$` isn't a legal
/// Plum identifier character — see `mangle`'s doc comment).
pub struct MonoPlan {
    /// Every function this program's codegen actually needs, INCLUDING
    /// ordinary (never-generic) ones — see `plan`'s doc comment for why
    /// this fully REPLACES `lower_program`'s own function list rather
    /// than being spliced alongside it. A generic function's own
    /// declared (unmangled) name never appears here directly; each of
    /// its reachable instantiations does, under its mangled name.
    pub functions: Vec<ir::Function>,
    /// Mangled generic-function name -> its concrete (param types, ret
    /// type) — the counterpart to `plumc`'s own `types`-derived
    /// `FnSig`s for ordinary functions, which can't cover a generic
    /// function's mangled instantiations (its `Infer::infer_program`
    /// entry only ever has ONE, generic-templated signature).
    pub signatures: HashMap<String, (Vec<Type>, Type)>,
    /// Mangled tag (a struct name, or an enum VARIANT tag) -> its
    /// fields'/payload's concrete types, in declared order — the
    /// generic-instantiation counterpart to `plumc`'s own
    /// `derive_tag_fields`, which only covers non-generic declarations.
    pub tag_fields: HashMap<String, Vec<Type>>,
    /// Original top-level name -> every mangled instantiation reachable
    /// for it, in discovery order — a non-generic name always maps to
    /// exactly `[name.clone()]` (identity). `plumc`'s entry-point lookup
    /// treats more than one entry as an "ambiguous entry point" error,
    /// since a compiled program needs exactly one concrete `main`-callable
    /// signature for whichever name the caller asked to run.
    pub entry_rename: HashMap<String, Vec<String>>,
}

/// `$` is not a legal Plum identifier character (the lexer only allows
/// `[A-Za-z_][A-Za-z0-9_]*`), matching the existing precedent of
/// synthetic tags using identifier-illegal characters (`tuple_tag`,
/// `RANGE_TAG`, `DEFAULT_ARM_TAG` in lower.rs/plum-codegen) — so a
/// mangled name can never collide with a real user identifier, and a
/// non-generic name (`args` empty) mangles to exactly itself, meaning
/// this whole pass produces a ZERO output diff for any program that
/// never uses generics at all.
pub fn mangle(name: &str, args: &[Type]) -> String {
    let mut out = name.to_string();
    for a in args {
        out.push('$');
        out.push_str(&mangle_type(a));
    }
    out
}

fn mangle_type(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::Str => "Str".to_string(),
        Type::CStr => "CStr".to_string(),
        Type::Unit => "Unit".to_string(),
        Type::Range => "Range".to_string(),
        Type::Struct(name, args) | Type::Enum(name, args) => mangle(name, args),
        Type::Tuple(elems) => {
            let mut s = format!("{}Tuple", elems.len());
            for e in elems {
                s.push('$');
                s.push_str(&mangle_type(e));
            }
            s
        }
        Type::Function(params, ret) => {
            let mut s = "Fn".to_string();
            for p in params {
                s.push('$');
                s.push_str(&mangle_type(p));
            }
            s.push_str("$to$");
            s.push_str(&mangle_type(ret));
            s
        }
        // Neither should ever reach here in a fully-resolved concrete
        // argument list — kept as a defensive, still-deterministic
        // spelling rather than a panic, matching this whole pass's
        // "never panic on user-controllable input" discipline.
        Type::Var(id) => format!("Var{id}"),
        Type::Param(name) => format!("Param_{name}"),
    }
}

/// Substitutes every `Type::Param(name)` in `ty` per `binding` — the
/// monomorphize-time counterpart to `plum_types::infer`'s internal
/// `subst_params` (not reused directly: it's private to that crate,
/// and this needs to return a `Result` for the "unbound param" case,
/// which should only ever indicate an internal inconsistency here,
/// never a user-facing error).
fn apply_binding(ty: &Type, binding: &HashMap<String, Type>) -> Result<Type, String> {
    match ty {
        Type::Param(name) => binding
            .get(name)
            .cloned()
            .ok_or_else(|| format!("internal error: monomorphize: unbound generic parameter {name:?}")),
        Type::Function(params, ret) => Ok(Type::Function(
            params.iter().map(|p| apply_binding(p, binding)).collect::<Result<_, _>>()?,
            Box::new(apply_binding(ret, binding)?),
        )),
        Type::Tuple(elems) => Ok(Type::Tuple(elems.iter().map(|e| apply_binding(e, binding)).collect::<Result<_, _>>()?)),
        Type::Struct(name, args) => Ok(Type::Struct(
            name.clone(),
            args.iter().map(|a| apply_binding(a, binding)).collect::<Result<_, _>>()?,
        )),
        Type::Enum(name, args) => Ok(Type::Enum(
            name.clone(),
            args.iter().map(|a| apply_binding(a, binding)).collect::<Result<_, _>>()?,
        )),
        other => Ok(other.clone()),
    }
}

/// Substitutes every `Type::Var(id)` present in `binding` — used to
/// specialize a generic function's own (var-templated, since a
/// function's own generics resolve to raw `Var`s, never `Param`s — see
/// `plum_types::infer::Infer::resolve_annotation`'s doc comment) base
/// signature into ITS concrete signature for one instantiation.
fn subst_vars(ty: &Type, binding: &HashMap<TypeVarId, Type>) -> Type {
    match ty {
        Type::Var(id) => binding.get(id).cloned().unwrap_or_else(|| ty.clone()),
        Type::Function(params, ret) => {
            Type::Function(params.iter().map(|p| subst_vars(p, binding)).collect(), Box::new(subst_vars(ret, binding)))
        }
        Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| subst_vars(e, binding)).collect()),
        Type::Struct(name, args) => Type::Struct(name.clone(), args.iter().map(|a| subst_vars(a, binding)).collect()),
        Type::Enum(name, args) => Type::Enum(name.clone(), args.iter().map(|a| subst_vars(a, binding)).collect()),
        other => other.clone(),
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Task {
    Function(String, Vec<TypeKey>),
    Struct(String, Vec<TypeKey>),
    Enum(String, Vec<TypeKey>),
}

// `Type` doesn't derive `Hash`/`Eq` (it holds no need for them anywhere
// else in the codebase), and re-deriving a structural hash for it here
// would duplicate real logic — a `Task` only ever needs `Vec<Type>`
// for EQUALITY/dedup purposes (the worklist itself), so this stores the
// already-mangled per-argument string instead, which both compares
// correctly AND doubles as a cheap dedup key. `plan` always carries the
// real `Vec<Type>` alongside a `Task` in its own local variables, never
// through `Task` itself, so nothing is lost.
type TypeKey = String;

fn to_keys(args: &[Type]) -> Vec<TypeKey> {
    args.iter().map(mangle_type).collect()
}

/// Runs the fixpoint worklist described in this module's doc comment.
/// `types` is `Infer::infer_program`'s own result map — needed to
/// recover a generic function's base (var-templated) signature so it
/// can be specialized per instantiation via `subst_vars`. `field_owners`/
/// `array_for_loops` are `Infer`'s own side-channels (see their doc
/// comments in plum-types) — this pass builds and lowers through its
/// OWN `LoweringContext` internally (every reachable function gets
/// re-lowered from a rewritten AST clone — see `MonoPlan::functions`'s
/// doc comment), so it needs these threaded in directly rather than
/// via a caller-supplied `LoweringContext`, the same way `plumc`'s own
/// ordinary `lower_program` call does.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    program: &ast::Program,
    type_ctx: &TypeContext,
    resolved_sites: &HashMap<Span, ResolvedSite>,
    fn_generics: &HashMap<String, Vec<(String, TypeVarId)>>,
    types: &HashMap<String, Type>,
    field_owners: &HashMap<Span, String>,
    array_for_loops: &std::collections::HashSet<Span>,
    // Closure-literal span -> resolved (param types, return type), and
    // variant-tag -> payload types — the SAME two side-channels
    // `plumc::codegen_cli` threads into its own top-level
    // `LoweringContext` (see `LoweringContext::closure_types`/
    // `variant_payload_types`'s doc comments), needed HERE too because
    // `MonoPlan::functions` re-lowers EVERY function (generic or not)
    // through this module's OWN `base_lctx`, not the caller's — see
    // `MonoPlan::functions`'s doc comment for why the caller's already-
    // lowered output can't just be reused directly. Spans stay valid
    // across this module's AST clone+rewrite (`rewrite_expr` only ever
    // mutates NAMES in place, never spans), so the caller's `Infer`-
    // derived maps still key correctly here.
    closure_types: &HashMap<Span, (Vec<Type>, Type)>,
    variant_payload_types: &HashMap<String, Vec<Type>>,
) -> Result<MonoPlan, String> {
    let mut struct_decls: HashMap<String, &ast::StructDecl> = HashMap::new();
    let mut variant_arity: HashMap<String, usize> = HashMap::new();
    let mut enum_variant_tags: HashMap<String, Vec<String>> = HashMap::new();
    let mut let_defs: HashMap<String, &ast::LetDef> = HashMap::new();
    for item in &program.items {
        match &item.kind {
            ast::ItemKind::Struct(d) => {
                struct_decls.insert(d.name.clone(), d);
            }
            ast::ItemKind::Enum(d) => {
                let mut tags = Vec::with_capacity(d.variants.len());
                for v in &d.variants {
                    variant_arity.insert(v.name.clone(), v.payload.len());
                    tags.push(v.name.clone());
                }
                enum_variant_tags.insert(d.name.clone(), tags);
            }
            ast::ItemKind::Let(def) => {
                if !def.params.is_empty() {
                    let_defs.insert(def.name.clone(), def);
                }
            }
            _ => {}
        }
    }

    // `field_owners` is NOT baked into `base_lctx` here — a field
    // access on a GENERIC struct instance needs its OWNING NAME
    // mangled per-specialization (see `RewriteCtx::field_owner_overrides`'s
    // doc comment), so each function's own lowering call below builds
    // its own merged copy instead.
    let base_lctx = LoweringContext::from_items(&program.items)
        .with_array_for_loops(array_for_loops.clone())
        .with_closure_types(closure_types.clone())
        .with_variant_payload_types(variant_payload_types.clone());

    let mut worklist: Vec<(Task, Vec<Type>)> = Vec::new();
    // Seed with EVERY ordinary (non-generic) function unconditionally —
    // `MonoPlan::functions` fully replaces `lower_program`'s own
    // function list (see `MonoPlan::functions`'s doc comment), so an
    // ordinary function that never touches a generic type still needs
    // to be included here, not just ones reachable via some generic
    // instantiation.
    for (name, def) in &let_defs {
        if def.generics.is_empty() {
            worklist.push((Task::Function(name.clone(), vec![]), vec![]));
        }
    }
    for site in resolved_sites.values() {
        if site.args.iter().any(|a| matches!(a, Type::Param(_))) {
            // Only reachable once SOME enclosing generic function is
            // itself instantiated concretely — discovered later, when
            // that function's own Task is processed and this site gets
            // re-walked with a concrete binding (see the rewrite pass
            // below).
            continue;
        }
        match site.kind {
            SiteKind::Function => {
                worklist.push((Task::Function(site.decl_name.clone(), to_keys(&site.args)), site.args.clone()));
            }
            SiteKind::Struct => {
                worklist.push((Task::Struct(site.decl_name.clone(), to_keys(&site.args)), site.args.clone()));
            }
            SiteKind::Enum => {
                let owning = type_ctx
                    .variant(&site.decl_name)
                    .map(|(enum_name, _)| enum_name.clone())
                    .ok_or_else(|| format!("internal error: monomorphize: unknown variant {:?}", site.decl_name))?;
                worklist.push((Task::Enum(owning, to_keys(&site.args)), site.args.clone()));
            }
        }
    }

    let mut done_fns: std::collections::HashSet<Task> = std::collections::HashSet::new();
    let mut done_types: std::collections::HashSet<Task> = std::collections::HashSet::new();
    let mut functions: Vec<ir::Function> = Vec::new();
    let mut signatures: HashMap<String, (Vec<Type>, Type)> = HashMap::new();
    let mut tag_fields: HashMap<String, Vec<Type>> = HashMap::new();
    let mut entry_rename: HashMap<String, Vec<String>> = HashMap::new();

    while let Some((task, args)) = worklist.pop() {
        match task {
            Task::Function(name, keys) => {
                let dedup_task = Task::Function(name.clone(), keys);
                if !done_fns.insert(dedup_task) {
                    continue;
                }
                let def = let_defs
                    .get(&name)
                    .ok_or_else(|| format!("internal error: monomorphize: unknown function {name:?}"))?;
                let is_generic = !def.generics.is_empty();
                let mangled = mangle(&name, &args);

                let binding: HashMap<String, Type> = if is_generic {
                    let decl_generics = fn_generics.get(&name).cloned().unwrap_or_default();
                    if decl_generics.len() != args.len() {
                        return Err(format!(
                            "internal error: monomorphize: {name:?} expects {} generic argument(s), found {} \
                             (instantiating at {args:?})",
                            decl_generics.len(),
                            args.len()
                        ));
                    }
                    decl_generics.into_iter().map(|(pname, _)| pname).zip(args.iter().cloned()).collect()
                } else {
                    HashMap::new()
                };

                let mut def_clone = (*def).clone();
                def_clone.name = mangled.clone();
                let mut rc = RewriteCtx {
                    resolved_sites,
                    enclosing_fn: &name,
                    binding: &binding,
                    variant_arity: &variant_arity,
                    struct_decls: &struct_decls,
                    new_tasks: Vec::new(),
                    extra_variants: HashMap::new(),
                    extra_struct_fields: HashMap::new(),
                    field_owner_overrides: HashMap::new(),
                };
                for p in &mut def_clone.params {
                    if let ast::ParamKind::Pattern(pat, _) = &mut p.kind {
                        rewrite_pattern(pat, &mut rc)?;
                    }
                }
                rewrite_expr(&mut def_clone.body, &mut rc)?;
                let RewriteCtx {
                    new_tasks,
                    extra_variants,
                    extra_struct_fields,
                    field_owner_overrides,
                    ..
                } = rc;
                worklist.extend(new_tasks);

                // A field access on a GENERIC struct (`p.x`) needs its
                // owning-struct name MANGLED to match the actual heap
                // value's real tag — `field_owner_overrides` carries
                // exactly that, per this specialization's own binding
                // (see `RewriteCtx`'s doc comment). Merged OVER the
                // plain, unmangled base map so a field access on a
                // non-generic struct in the SAME function body still
                // resolves normally.
                let mut merged_field_owners = field_owners.clone();
                merged_field_owners.extend(field_owner_overrides);
                let lctx = base_lctx
                    .clone()
                    .with_field_owners(merged_field_owners)
                    .with_extra_variants(extra_variants)
                    .with_extra_struct_fields(extra_struct_fields);
                let (params, destructures) = lower_params(&def_clone.params)?;
                let mut body = lower_expr(&def_clone.body, &lctx)?;
                for (synthetic, pattern) in destructures.into_iter().rev() {
                    body = wrap_destructure(synthetic, &pattern, &lctx, body)?;
                }
                functions.push(ir::Function {
                    name: mangled.clone(),
                    params,
                    body,
                });

                if is_generic {
                    let base_ty = types
                        .get(&name)
                        .ok_or_else(|| format!("internal error: monomorphize: no inferred type for {name:?}"))?;
                    let Type::Function(base_params, base_ret) = base_ty else {
                        return Err(format!("internal error: monomorphize: {name:?} has a non-function type"));
                    };
                    let decl_generics = fn_generics.get(&name).cloned().unwrap_or_default();
                    let var_binding: HashMap<TypeVarId, Type> =
                        decl_generics.into_iter().map(|(_, vid)| vid).zip(args.iter().cloned()).collect();
                    let spec_params: Vec<Type> = base_params.iter().map(|p| subst_vars(p, &var_binding)).collect();
                    let spec_ret = subst_vars(base_ret, &var_binding);
                    signatures.insert(mangled.clone(), (spec_params, spec_ret));
                }
                entry_rename.entry(name.clone()).or_default().push(mangled);
            }

            Task::Struct(name, keys) => {
                let dedup_task = Task::Struct(name.clone(), keys);
                if !done_types.insert(dedup_task) {
                    continue;
                }
                let mangled = mangle(&name, &args);
                let fields = type_ctx
                    .struct_fields_for(&name, &args)
                    .ok_or_else(|| format!("internal error: monomorphize: unknown generic struct {name:?}"))?;
                let mut field_types = Vec::with_capacity(fields.len());
                for (_, fty) in &fields {
                    field_types.push(validate_field_type(fty, &name, &args, &mut worklist)?);
                }
                tag_fields.insert(mangled, field_types);
            }

            Task::Enum(name, keys) => {
                let dedup_task = Task::Enum(name.clone(), keys);
                if !done_types.insert(dedup_task) {
                    continue;
                }
                let tags = enum_variant_tags.get(&name).cloned().unwrap_or_default();
                for tag in tags {
                    let (_, payload) = type_ctx
                        .variant_payload_for(&tag, &args)
                        .ok_or_else(|| format!("internal error: monomorphize: unknown variant {tag:?}"))?;
                    let mangled_tag = mangle(&tag, &args);
                    let mut field_types = Vec::with_capacity(payload.len());
                    for fty in &payload {
                        field_types.push(validate_field_type(fty, &tag, &args, &mut worklist)?);
                    }
                    tag_fields.insert(mangled_tag, field_types);
                }
            }
        }
    }

    Ok(MonoPlan {
        functions,
        signatures,
        tag_fields,
        entry_rename,
    })
}

/// A struct field's/variant payload's concrete type, once resolved via
/// `struct_fields_for`/`variant_payload_for`, must itself be codegen-
/// supported — Int/Float/Bool/Unit (a scalar `CgType`), or ANOTHER
/// struct/enum reference (pushed onto the worklist as its own
/// dependency, whether or not it's itself generic — a non-generic
/// nested struct is harmlessly re-discovered here too, since `mangle`
/// on empty args is the identity and `plumc`'s own `derive_tag_fields`
/// already covers it, so any duplicate `tag_fields` entry this produces
/// is identical, not conflicting). Anything else (`Str`, a closure, a
/// tuple, a still-unresolved `Var`/`Param`) is outside codegen's
/// supported scope — reported as a clear `Err`, never a panic, exactly
/// like `plumc::codegen_cli::plum_type_to_cg_type`'s own equivalent
/// check for the non-generic case.
fn validate_field_type(ty: &Type, owner_tag: &str, args: &[Type], worklist: &mut Vec<(Task, Vec<Type>)>) -> Result<Type, String> {
    match ty {
        Type::Int | Type::Float | Type::Bool | Type::Unit => Ok(ty.clone()),
        Type::Struct(n, a) => {
            worklist.push((Task::Struct(n.clone(), to_keys(a)), a.clone()));
            Ok(ty.clone())
        }
        Type::Enum(n, a) => {
            worklist.push((Task::Enum(n.clone(), to_keys(a)), a.clone()));
            Ok(ty.clone())
        }
        other => Err(format!(
            "codegen does not yet support monomorphizing {owner_tag:?} (instantiated at {args:?}) — field type \
             {other:?} is outside codegen's supported scope"
        )),
    }
}

/// Mutable state threaded through one function body's AST rewrite —
/// bundled into a struct purely to keep `rewrite_expr`/`rewrite_pattern`'s
/// own signatures manageable.
struct RewriteCtx<'a> {
    resolved_sites: &'a HashMap<Span, ResolvedSite>,
    // The ORIGINAL (unmangled) name of the function whose body is being
    // rewritten — matches `ResolvedSite::enclosing_fn` exactly (both
    // ultimately come from `plum_types::Infer::current_fn`).
    enclosing_fn: &'a str,
    // This SPECIALIZATION's binding of `enclosing_fn`'s own declared
    // generics to concrete types — empty for a non-generic function
    // (whose `ResolvedSite`s, being tier-1 only, never contain a
    // `Type::Param` needing this in the first place).
    binding: &'a HashMap<String, Type>,
    variant_arity: &'a HashMap<String, usize>,
    struct_decls: &'a HashMap<String, &'a ast::StructDecl>,
    new_tasks: Vec<(Task, Vec<Type>)>,
    extra_variants: HashMap<String, usize>,
    extra_struct_fields: HashMap<String, Vec<String>>,
    // A field access on a GENERIC struct instance (`p.x`) is lowered
    // through a COMPLETELY separate mechanism from every other site
    // here — `lower.rs`'s `Expr::Field` arm never looks at the AST
    // node's own name/path at all, only at `LoweringContext::
    // field_owners[span]` (populated by `plum_types::Infer` during
    // inference, see that field's own doc comment) to find which
    // struct's declared field list to build a `Match` tag from. So
    // there's nothing IN THE AST for `rewrite_expr` to rename for a
    // `Field` node — instead, this collects span -> MANGLED-owner-name
    // overrides to merge OVER the plain `field_owners` map before
    // lowering this one specialization, so the `Match` this produces
    // targets the correct, mangled tag.
    field_owner_overrides: HashMap<Span, String>,
}

/// If `span` names a generic instantiation site belonging to
/// `rc.enclosing_fn`, resolves it to this specialization's CONCRETE
/// args, mangles it, registers whatever new worklist task and
/// `LoweringContext` extras it implies, and returns the mangled name —
/// `None` if `span` isn't a sit at all (an ordinary, non-generic AST
/// node) or belongs to some OTHER function (shouldn't happen given how
/// spans are scoped, but checked defensively rather than assumed).
fn resolve_site(rc: &mut RewriteCtx, span: Span) -> Result<Option<String>, String> {
    let Some(site) = rc.resolved_sites.get(&span) else {
        return Ok(None);
    };
    if site.enclosing_fn.as_deref() != Some(rc.enclosing_fn) {
        return Ok(None);
    }
    let concrete: Vec<Type> = site.args.iter().map(|a| apply_binding(a, rc.binding)).collect::<Result<_, _>>()?;
    let mangled = mangle(&site.decl_name, &concrete);
    match site.kind {
        SiteKind::Function => {
            rc.new_tasks.push((Task::Function(site.decl_name.clone(), to_keys(&concrete)), concrete));
        }
        SiteKind::Struct => {
            rc.new_tasks.push((Task::Struct(site.decl_name.clone(), to_keys(&concrete)), concrete));
            if let Some(decl) = rc.struct_decls.get(site.decl_name.as_str()) {
                let field_names: Vec<String> = decl.fields.iter().map(|f| f.name.clone()).collect();
                rc.extra_struct_fields.entry(mangled.clone()).or_insert(field_names);
            }
        }
        SiteKind::Enum => {
            // `site.decl_name` is the VARIANT tag for an enum-kind site
            // (see `RawSite`'s own doc comment in plum-types) — the
            // owning enum's Task is discovered separately, either from
            // this same function's OTHER sites or via a field-type
            // dependency once the enum's Task itself is processed;
            // nothing further to push here beyond registering this one
            // variant tag's arity for lowering.
            if let Some(&arity) = rc.variant_arity.get(site.decl_name.as_str()) {
                rc.extra_variants.entry(mangled.clone()).or_insert(arity);
            }
        }
    }
    Ok(Some(mangled))
}

fn rewrite_expr(expr: &mut ast::Expr, rc: &mut RewriteCtx) -> Result<(), String> {
    match expr {
        ast::Expr::Int(..) | ast::Expr::Float(..) | ast::Expr::Str(..) | ast::Expr::Bool(..) => Ok(()),
        ast::Expr::Ident(name, span) => {
            if let Some(mangled) = resolve_site(rc, *span)? {
                *name = mangled;
            }
            Ok(())
        }
        ast::Expr::Tuple(elems, _) | ast::Expr::ArrayLiteral(elems, _) => {
            for e in elems {
                rewrite_expr(e, rc)?;
            }
            Ok(())
        }
        ast::Expr::Unary { expr, .. } => rewrite_expr(expr, rc),
        ast::Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr(lhs, rc)?;
            rewrite_expr(rhs, rc)
        }
        ast::Expr::Field { base, span, .. } => {
            // `resolve_site` here mangles+registers as a side effect but
            // its RETURN (the mangled name itself) isn't what's needed —
            // `field_owner_overrides` needs the same mangled name keyed
            // by SPAN instead (see `RewriteCtx::field_owner_overrides`'s
            // doc comment for why `Field` has nothing in the AST itself
            // to rename).
            if let Some(mangled) = resolve_site(rc, *span)? {
                rc.field_owner_overrides.insert(*span, mangled);
            }
            rewrite_expr(base, rc)
        }
        ast::Expr::Call { callee, args, span } => {
            // A variant-construction site is recorded at the CALL
            // expression's OWN span (matching `Infer::infer_expr`'s
            // `Call` arm, which passes `*span` — the whole call's span,
            // not the callee's — to `record_site`) — so this checks the
            // Call's span FIRST, renaming just the callee's tag name and
            // skipping any further rewrite of `callee` itself (a bare
            // tag name has nothing else to rewrite). An ordinary call
            // (no site at this span) falls through to recursing into
            // `callee` normally, which is where a GENERIC FUNCTION call
            // gets renamed (recorded at the callee `Ident`'s own,
            // different span).
            if let Some(mangled) = resolve_site(rc, *span)? {
                match callee.as_mut() {
                    ast::Expr::Ident(n, _) => *n = mangled,
                    ast::Expr::Field { name, .. } => *name = mangled,
                    _ => {}
                }
            } else {
                rewrite_expr(callee, rc)?;
            }
            for a in args {
                rewrite_expr(a, rc)?;
            }
            Ok(())
        }
        ast::Expr::GenericInst { callee, .. } => rewrite_expr(callee, rc),
        ast::Expr::Index { base, index, .. } => {
            rewrite_expr(base, rc)?;
            rewrite_expr(index, rc)
        }
        ast::Expr::Block(block, _) => rewrite_block(block, rc),
        ast::Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_expr(cond, rc)?;
            rewrite_block(then_branch, rc)?;
            if let Some(e) = else_branch {
                rewrite_expr(e, rc)?;
            }
            Ok(())
        }
        ast::Expr::Match { scrutinee, arms, .. } => {
            rewrite_expr(scrutinee, rc)?;
            for arm in arms {
                rewrite_pattern(&mut arm.pattern, rc)?;
                if let Some(g) = &mut arm.guard {
                    rewrite_expr(g, rc)?;
                }
                rewrite_expr(&mut arm.body, rc)?;
            }
            Ok(())
        }
        ast::Expr::For { pattern, iter, body, .. } => {
            rewrite_pattern(pattern, rc)?;
            rewrite_expr(iter, rc)?;
            rewrite_block(body, rc)
        }
        ast::Expr::Closure { body, .. } => rewrite_expr(body, rc),
        ast::Expr::Unsafe(block, _) | ast::Expr::Spawn(block, _) => rewrite_block(block, rc),
        ast::Expr::StructLiteral { path, fields, spread, span } => {
            if let Some(mangled) = resolve_site(rc, *span)? {
                if let Some(last) = path.last_mut() {
                    *last = mangled;
                }
            }
            for f in fields {
                rewrite_expr(&mut f.value, rc)?;
            }
            if let Some(s) = spread {
                rewrite_expr(s, rc)?;
            }
            Ok(())
        }
        ast::Expr::Select { arms, .. } => {
            for arm in arms {
                rewrite_pattern(&mut arm.pattern, rc)?;
                rewrite_expr(&mut arm.expr, rc)?;
                rewrite_expr(&mut arm.body, rc)?;
            }
            Ok(())
        }
    }
}

fn rewrite_block(block: &mut ast::Block, rc: &mut RewriteCtx) -> Result<(), String> {
    for stmt in &mut block.stmts {
        rewrite_stmt(stmt, rc)?;
    }
    if let Some(t) = &mut block.tail {
        rewrite_expr(t, rc)?;
    }
    Ok(())
}

fn rewrite_stmt(stmt: &mut ast::Stmt, rc: &mut RewriteCtx) -> Result<(), String> {
    match stmt {
        ast::Stmt::Let { pattern, value, .. } => {
            rewrite_pattern(pattern, rc)?;
            rewrite_expr(value, rc)
        }
        ast::Stmt::Assign { value, .. } => rewrite_expr(value, rc),
        ast::Stmt::Expr(e) => rewrite_expr(e, rc),
    }
}

fn rewrite_pattern(pattern: &mut ast::Pattern, rc: &mut RewriteCtx) -> Result<(), String> {
    match pattern {
        ast::Pattern::Int(..)
        | ast::Pattern::Float(..)
        | ast::Pattern::Str(..)
        | ast::Pattern::Bool(..)
        | ast::Pattern::Wildcard(_)
        | ast::Pattern::Ident(..) => Ok(()),
        ast::Pattern::Tuple(elems, _) => {
            for e in elems {
                rewrite_pattern(e, rc)?;
            }
            Ok(())
        }
        ast::Pattern::Variant { path, args, span } => {
            if let Some(mangled) = resolve_site(rc, *span)? {
                if let Some(last) = path.last_mut() {
                    *last = mangled;
                }
            }
            for a in args {
                rewrite_pattern(a, rc)?;
            }
            Ok(())
        }
        ast::Pattern::Struct { path, fields, span, .. } => {
            if let Some(mangled) = resolve_site(rc, *span)? {
                if let Some(last) = path.last_mut() {
                    *last = mangled;
                }
            }
            for f in fields {
                rewrite_pattern(&mut f.pattern, rc)?;
            }
            Ok(())
        }
        ast::Pattern::Or(alts, _) => {
            for a in alts {
                rewrite_pattern(a, rc)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;
    use plum_types::infer::Infer;

    fn plan_for(src: &str) -> MonoPlan {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let type_ctx = TypeContext::from_items(&program.items).unwrap_or_else(|e| panic!("context error: {e}"));
        let mut infer = Infer::with_context(type_ctx);
        let types = infer.infer_program(&program).unwrap_or_else(|e| panic!("type error: {e}"));
        let resolved_sites = infer.resolve_generic_sites().unwrap_or_else(|e| panic!("resolve error: {e}"));
        let type_ctx2 = TypeContext::from_items(&program.items).unwrap();
        let closure_types = infer.resolve_closure_types().unwrap_or_else(|e| panic!("closure type error: {e}"));
        plan(
            &program,
            &type_ctx2,
            &resolved_sites,
            infer.fn_generics(),
            &types,
            infer.field_owners(),
            infer.array_for_loops(),
            &closure_types,
            &HashMap::new(),
        )
        .unwrap_or_else(|e| panic!("plan error: {e}"))
    }

    #[test]
    fn a_generic_struct_instantiated_at_two_types_produces_two_distinct_mangled_tags_and_no_plain_one() {
        let src = "\
            struct Pair[T] { first: T, second: T }\n\
            let go_int (): Int = { let p = Pair { first: 1, second: 2 }; p.first }\n\
            let go_bool (): Bool = { let p = Pair { first: true, second: false }; p.first }\n\
        ";
        let plan = plan_for(src);
        assert!(plan.tag_fields.contains_key("Pair$Int"), "tag_fields: {:?}", plan.tag_fields.keys());
        assert!(plan.tag_fields.contains_key("Pair$Bool"), "tag_fields: {:?}", plan.tag_fields.keys());
        assert!(!plan.tag_fields.contains_key("Pair"));
        assert_eq!(plan.tag_fields["Pair$Int"], vec![Type::Int, Type::Int]);
        assert_eq!(plan.tag_fields["Pair$Bool"], vec![Type::Bool, Type::Bool]);
    }

    #[test]
    fn mangle_of_a_non_generic_name_is_the_identity() {
        assert_eq!(mangle("Point", &[]), "Point");
    }

    #[test]
    fn mangle_of_a_generic_instantiation_joins_with_dollar() {
        assert_eq!(mangle("Pair", &[Type::Int, Type::Bool]), "Pair$Int$Bool");
    }
}
