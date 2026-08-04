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
    /// Mangled STRUCT tag -> its field NAMES, in the SAME declared
    /// order as `tag_fields`'s own `Vec<Type>` for that tag — the
    /// generic-instantiation counterpart to `plumc`'s own `derive_tag_
    /// fields`'s second return value. Has NO entries for enum variant
    /// tags, for the same reason `plum_codegen::StructFieldNames`
    /// itself doesn't: Plum enum variant payloads are already
    /// positional at the language level.
    pub struct_field_names: HashMap<String, Vec<String>>,
    /// Original top-level name -> every mangled instantiation reachable
    /// for it, in discovery order — a non-generic name always maps to
    /// exactly `[name.clone()]` (identity). `plumc`'s entry-point lookup
    /// treats more than one entry as an "ambiguous entry point" error,
    /// since a compiled program needs exactly one concrete `main`-callable
    /// signature for whichever name the caller asked to run.
    pub entry_rename: HashMap<String, Vec<String>>,
    /// Every zero-param top-level `let` (i.e. `plumc::codegen_cli`'s own
    /// notion of a `Global`), REWRITTEN so any still-generic function it
    /// calls is renamed to that call's concrete mangled instantiation —
    /// see `plan`'s doc comment for why this fully REPLACES
    /// `lower_program`'s own `globals` list rather than being spliced
    /// alongside it, mirroring `functions` above exactly. In ORIGINAL
    /// SOURCE declaration order (NOT worklist/discovery order — later
    /// globals, and `@plum_init_globals`, depend on earlier globals
    /// having already run, see DESIGN.md's "Non-constant Global
    /// initializers" section), unlike `functions`, which has no such
    /// ordering requirement.
    pub globals: Vec<ir::Global>,
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
    /// A zero-param top-level `let` (a `Global`, in `plumc::codegen_cli`'s
    /// terminology) — never itself generic, so no `Vec<TypeKey>` (always
    /// exactly one instantiation). Processed through the SAME worklist
    /// as `Function`/`Struct`/`Enum`, deliberately, rather than as a
    /// separate post-pass: a global's own initializer can call a
    /// generic function, and `resolve_site` pushes a `new_tasks` entry
    /// EVERY time it rewrites a matching call site — regardless of
    /// whether that exact instantiation was already discovered and
    /// processed elsewhere (dedup happens once, at the top of THIS
    /// task's own processing, via `done_fns`, same as every other
    /// variant) — so a global's rewrite needs the real worklist
    /// machinery to safely drain whatever it (redundantly but
    /// harmlessly) re-requests, not a hand-rolled defensive check.
    Global(String),
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
    // Empty-array-literal span -> resolved (possibly template)
    // element type — the empty-array counterpart to `closure_types`
    // immediately above, needed HERE for the same reason (see this
    // module's own doc comment and `closure_types`'s doc comment):
    // `MonoPlan::functions` re-lowers EVERY function through this
    // module's OWN `base_lctx`, not the caller's already-lowered one.
    empty_array_elem_types: &HashMap<Span, Type>,
    variant_payload_types: &HashMap<String, Vec<Type>>,
) -> Result<MonoPlan, String> {
    let mut struct_decls: HashMap<String, &ast::StructDecl> = HashMap::new();
    let mut variant_arity: HashMap<String, usize> = HashMap::new();
    let mut enum_variant_tags: HashMap<String, Vec<String>> = HashMap::new();
    // Variant tag -> its OWNING enum's name (e.g. `"MapNode"` ->
    // `"Map"`) — needed by `resolve_site`'s `SiteKind::Enum` branch (see
    // its own doc comment) to push a `Task::Enum` for a Ctor call
    // discovered INSIDE a generic function's body, mirroring exactly
    // what the seeding loop below already does for a Ctor call reached
    // directly from a non-generic function's body (`type_ctx.variant`,
    // a few lines down) — without this, a generic function whose ENTIRE
    // reachable enum usage is a bare Ctor call (no other struct/enum-
    // typed site in the same body) never gets its enum's `Task::Enum`
    // enqueued at all, and codegen fails with "unknown tag" for a tag
    // that's real but was simply never registered. Found empirically:
    // `let map_new[K, V] (): Map[K, V] = MapEnd` is exactly this shape.
    let mut variant_owner: HashMap<String, String> = HashMap::new();
    let mut let_defs: HashMap<String, &ast::LetDef> = HashMap::new();
    // The exact complement of `let_defs` above — every zero-param
    // top-level `let` (a `Global`, in `plumc::codegen_cli`'s own
    // terminology), collected here so its body can be rewritten in a
    // final deterministic pass AFTER the worklist below drains (see
    // `MonoPlan::globals`'s doc comment).
    let mut global_defs: HashMap<String, &ast::LetDef> = HashMap::new();
    for item in &program.items {
        match &item.kind {
            ast::ItemKind::Struct(d) => {
                struct_decls.insert(d.name.clone(), d);
            }
            ast::ItemKind::Enum(d) => {
                let mut tags = Vec::with_capacity(d.variants.len());
                for v in &d.variants {
                    variant_arity.insert(v.name.clone(), v.payload.len());
                    variant_owner.insert(v.name.clone(), d.name.clone());
                    tags.push(v.name.clone());
                }
                enum_variant_tags.insert(d.name.clone(), tags);
            }
            ast::ItemKind::Let(def) => {
                if !def.params.is_empty() {
                    let_defs.insert(def.name.clone(), def);
                } else {
                    global_defs.insert(def.name.clone(), def);
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
        .with_empty_array_elem_types(empty_array_elem_types.clone())
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
    // Seed with EVERY global too, unconditionally — a global is never
    // itself generic, so every one is always reachable, mirroring the
    // ordinary-function seeding just above.
    for name in global_defs.keys() {
        worklist.push((Task::Global(name.clone()), vec![]));
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
    let mut struct_field_names: HashMap<String, Vec<String>> = HashMap::new();
    let mut entry_rename: HashMap<String, Vec<String>> = HashMap::new();
    // Unordered — the worklist's processing order is LIFO/discovery-
    // order, not source-declaration order, which `@plum_init_globals`
    // depends on for correctness. Reordered into `MonoPlan::globals`
    // (source order) in one final pass once the loop below drains — see
    // that pass's own comment just after the loop.
    let mut globals_by_name: HashMap<String, ir::Global> = HashMap::new();

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
                    variant_owner: &variant_owner,
                    struct_decls: &struct_decls,
                    new_tasks: Vec::new(),
                    extra_variants: HashMap::new(),
                    extra_struct_fields: HashMap::new(),
                    field_owner_overrides: HashMap::new(),
                    closure_types,
                    extra_closure_types: HashMap::new(),
                    empty_array_elem_types,
                    extra_empty_array_elem_types: HashMap::new(),
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
                    extra_closure_types,
                    extra_empty_array_elem_types,
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
                // Same merge-OVER-the-base-map pattern as
                // `merged_field_owners` immediately above, just for a
                // closure literal's own concrete per-instantiation type
                // (see `RewriteCtx::extra_closure_types`'s doc comment).
                let mut merged_closure_types = closure_types.clone();
                merged_closure_types.extend(extra_closure_types);
                // Same merge-OVER-the-base-map pattern again, for an
                // empty array literal's own concrete per-instantiation
                // element type (see `RewriteCtx::extra_empty_array_elem_
                // types`'s doc comment).
                let mut merged_empty_array_elem_types = empty_array_elem_types.clone();
                merged_empty_array_elem_types.extend(extra_empty_array_elem_types);
                let lctx = base_lctx
                    .clone()
                    .with_field_owners(merged_field_owners)
                    .with_extra_variants(extra_variants)
                    .with_extra_struct_fields(extra_struct_fields)
                    .with_closure_types(merged_closure_types)
                    .with_empty_array_elem_types(merged_empty_array_elem_types);
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

            // A global is never itself generic — no `mangle`d name, no
            // `signatures`/`entry_rename` entry, and (unlike a struct's
            // field types or a function's own body) its rewrite can
            // never discover a DIFFERENT concrete instantiation for
            // ITSELF, only for whatever generic callees its body
            // touches (handled the normal way, via `rc.new_tasks` below,
            // same as `Task::Function`'s own body-rewrite).
            Task::Global(name) => {
                let dedup_task = Task::Global(name.clone());
                if !done_fns.insert(dedup_task) {
                    continue;
                }
                let def = global_defs
                    .get(&name)
                    .ok_or_else(|| format!("internal error: monomorphize: unknown global {name:?}"))?;
                let mut body_clone = def.body.clone();
                let mut rc = RewriteCtx {
                    resolved_sites,
                    enclosing_fn: &name,
                    binding: &HashMap::new(),
                    variant_arity: &variant_arity,
                    variant_owner: &variant_owner,
                    struct_decls: &struct_decls,
                    new_tasks: Vec::new(),
                    extra_variants: HashMap::new(),
                    extra_struct_fields: HashMap::new(),
                    field_owner_overrides: HashMap::new(),
                    closure_types,
                    extra_closure_types: HashMap::new(),
                    empty_array_elem_types,
                    extra_empty_array_elem_types: HashMap::new(),
                };
                rewrite_expr(&mut body_clone, &mut rc)?;
                let RewriteCtx {
                    new_tasks,
                    extra_variants,
                    extra_struct_fields,
                    field_owner_overrides,
                    extra_closure_types,
                    extra_empty_array_elem_types,
                    ..
                } = rc;
                worklist.extend(new_tasks);

                let mut merged_field_owners = field_owners.clone();
                merged_field_owners.extend(field_owner_overrides);
                let mut merged_closure_types = closure_types.clone();
                merged_closure_types.extend(extra_closure_types);
                let mut merged_empty_array_elem_types = empty_array_elem_types.clone();
                merged_empty_array_elem_types.extend(extra_empty_array_elem_types);
                let lctx = base_lctx
                    .clone()
                    .with_field_owners(merged_field_owners)
                    .with_extra_variants(extra_variants)
                    .with_extra_struct_fields(extra_struct_fields)
                    .with_closure_types(merged_closure_types)
                    .with_empty_array_elem_types(merged_empty_array_elem_types);
                let body = lower_expr(&body_clone, &lctx)?;
                globals_by_name.insert(name.clone(), ir::Global { name, value: body });
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
                let mut field_names = Vec::with_capacity(fields.len());
                for (fname, fty) in &fields {
                    field_types.push(validate_field_type(fty, &name, &args, &mut worklist)?);
                    field_names.push(fname.clone());
                }
                tag_fields.insert(mangled.clone(), field_types);
                struct_field_names.insert(mangled, field_names);
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

    // Reorder `globals_by_name` (built by the `Task::Global` arm above,
    // in worklist/discovery order — LIFO, not meaningful) into ORIGINAL
    // SOURCE declaration order, which `@plum_init_globals` depends on
    // for correctness (a later global's own initializer, or the runtime
    // init function itself, can reference an earlier global — see
    // ir.rs's `Global` doc comment). Every zero-param `Let` was
    // unconditionally seeded as its own `Task::Global` above, so every
    // lookup here is expected to hit.
    let mut globals: Vec<ir::Global> = Vec::new();
    for item in &program.items {
        let ast::ItemKind::Let(def) = &item.kind else { continue };
        if !def.params.is_empty() {
            continue;
        }
        let global = globals_by_name
            .remove(&def.name)
            .ok_or_else(|| format!("internal error: monomorphize: global {:?} was never processed", def.name))?;
        globals.push(global);
    }

    Ok(MonoPlan {
        functions,
        signatures,
        tag_fields,
        struct_field_names,
        entry_rename,
        globals,
    })
}

/// A struct field's/variant payload's concrete type, once resolved via
/// `struct_fields_for`/`variant_payload_for`, must itself be codegen-
/// supported — Int/Float/Bool/Unit/Str (every scalar `CgType`), or
/// ANOTHER struct/enum reference (pushed onto the worklist as its own
/// dependency, whether or not it's itself generic — a non-generic
/// nested struct is harmlessly re-discovered here too, since `mangle`
/// on empty args is the identity and `plumc`'s own `derive_tag_fields`
/// already covers it, so any duplicate `tag_fields` entry this produces
/// is identical, not conflicting). Anything else (a closure, a tuple, a
/// still-unresolved `Var`/`Param`) is outside codegen's supported
/// scope — reported as a clear `Err`, never a panic, exactly like
/// `plumc::codegen_cli::plum_type_to_cg_type`'s own equivalent check
/// for the non-generic case.
///
/// `Str` was ORIGINALLY excluded here (this doc comment used to list it
/// alongside the genuinely-unsupported cases) — found, while building
/// `Map`/`Set`, to be a stale mismatch: `plum_type_to_cg_type` (this
/// function's own non-generic counterpart) has ALWAYS mapped
/// `PlumType::Str -> CgType::Str` unconditionally, so a Str-typed field
/// on a NON-generic struct/enum already worked fine — only the generic
/// path here had never grown a matching arm, presumably because no
/// earlier generic-struct/enum test happened to use a Str field. A
/// Str-keyed `Map`/`Set` needs exactly this (`MapNode(Str, V, ...)` at
/// `V = Str`, etc.), which is what surfaced it.
fn validate_field_type(ty: &Type, owner_tag: &str, args: &[Type], worklist: &mut Vec<(Task, Vec<Type>)>) -> Result<Type, String> {
    match ty {
        Type::Int | Type::Float | Type::Bool | Type::Unit | Type::Str => Ok(ty.clone()),
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
    // See its own doc comment where it's built, in `plan` — needed here
    // so `resolve_site`'s `SiteKind::Enum` branch can push a
    // `Task::Enum` for the OWNING enum of a Ctor call reached only
    // through a generic function's body.
    variant_owner: &'a HashMap<String, String>,
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
    // The BASE (possibly template-containing) closure-type map computed
    // by `plum_types::Infer::resolve_closure_types` — a closure literal
    // nested inside `rc.enclosing_fn`'s own body may have its param/
    // return types recorded there as `Type::Param` TEMPLATES (see that
    // function's own doc comment), exactly like `resolved_sites` can
    // hold a `Type::Param` for a nested generic call/construction site.
    closure_types: &'a HashMap<Span, (Vec<Type>, Type)>,
    // This SPECIALIZATION's own concrete resolution of every closure
    // literal directly inside `enclosing_fn`'s body, keyed by span —
    // collected the SAME way `field_owner_overrides`/`extra_variants`/
    // `extra_struct_fields` are (see their own doc comments), by
    // applying `rc.binding` to whatever `closure_types` recorded for
    // that span. Merged OVER the plain `closure_types` base map before
    // lowering this one specialization, so `lower.rs`'s unmodified
    // `Expr::Closure` arm picks up a fully concrete type for this
    // instantiation instead of the shared template.
    extra_closure_types: HashMap<Span, (Vec<Type>, Type)>,
    // The BASE (possibly template-containing) empty-array-element-type
    // map computed by `plum_types::Infer::resolve_empty_array_elem_
    // types` — an empty array literal (`[]`) nested inside `rc.
    // enclosing_fn`'s own body may have its element type recorded there
    // as a `Type::Param` TEMPLATE (see that function's own doc comment),
    // exactly like `closure_types` immediately above.
    empty_array_elem_types: &'a HashMap<Span, Type>,
    // This SPECIALIZATION's own concrete resolution of every empty array
    // literal directly inside `enclosing_fn`'s body, keyed by span —
    // collected the SAME way `extra_closure_types` is (see its own doc
    // comment), by applying `rc.binding` to whatever `empty_array_elem_
    // types` recorded for that span. Merged OVER the plain `empty_array_
    // elem_types` base map before lowering this one specialization, so
    // `lower.rs`'s unmodified `Expr::ArrayLiteral` arm picks up a fully
    // concrete element type for this instantiation instead of the
    // shared template.
    extra_empty_array_elem_types: HashMap<Span, Type>,
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
            // (see `RawSite`'s own doc comment in plum-types). The
            // owning enum's own `Task::Enum` is pushed HERE, explicitly
            // — it is NOT always discovered some other way: if a
            // generic function's ENTIRE reachable use of an enum is a
            // bare Ctor call (e.g. `let map_new[K, V] (): Map[K, V] =
            // MapEnd`, with no other struct/enum-typed site anywhere in
            // its body), there is no other site that would ever enqueue
            // `Task::Enum` for it, and `tag_fields` silently never gets
            // an entry for that instantiation — codegen then fails with
            // "unknown tag", for a tag that's real but was simply never
            // registered. Found empirically while building `Map`/`Set`.
            // Mirrors the identical `Task::Enum` push the seeding loop
            // above already does for a Ctor call reached directly from
            // a NON-generic function's body.
            if let Some(&arity) = rc.variant_arity.get(site.decl_name.as_str()) {
                rc.extra_variants.entry(mangled.clone()).or_insert(arity);
            }
            if let Some(owning) = rc.variant_owner.get(site.decl_name.as_str()) {
                rc.new_tasks.push((Task::Enum(owning.clone(), to_keys(&concrete)), concrete.clone()));
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
        ast::Expr::ArrayLiteral(elems, span) if elems.is_empty() => {
            // Mirrors the `Closure` arm below exactly — see `RewriteCtx::
            // empty_array_elem_types`/`extra_empty_array_elem_types`'s
            // own doc comments. An empty literal has no elements to
            // recurse into, so this is the whole arm.
            if let Some(elem_ty) = rc.empty_array_elem_types.get(span) {
                let concrete = apply_binding(elem_ty, rc.binding)?;
                rc.extra_empty_array_elem_types.insert(*span, concrete);
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
        ast::Expr::Closure { body, span, .. } => {
            // If this closure literal's type was recorded (possibly as
            // a `Type::Param` template — see `RewriteCtx::closure_types`'s
            // doc comment), resolve it CONCRETELY for this specific
            // instantiation via this specialization's own `binding`,
            // the same way `resolve_site` resolves a nested generic
            // call/construction site's args. A closure with no entry at
            // all (declared inside a NON-generic function, or one whose
            // type was already fully concrete with nothing to
            // substitute) needs nothing further here — the base
            // `closure_types` map already has its correct entry.
            if let Some((param_tys, ret_ty)) = rc.closure_types.get(span) {
                let concrete_params: Vec<Type> =
                    param_tys.iter().map(|t| apply_binding(t, rc.binding)).collect::<Result<_, _>>()?;
                let concrete_ret = apply_binding(ret_ty, rc.binding)?;
                rc.extra_closure_types.insert(*span, (concrete_params, concrete_ret));
            }
            rewrite_expr(body, rc)
        }
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
        let empty_array_elem_types =
            infer.resolve_empty_array_elem_types().unwrap_or_else(|e| panic!("empty array elem type error: {e}"));
        plan(
            &program,
            &type_ctx2,
            &resolved_sites,
            infer.fn_generics(),
            &types,
            infer.field_owners(),
            infer.array_for_loops(),
            &closure_types,
            &empty_array_elem_types,
            &HashMap::new(),
        )
        .unwrap_or_else(|e| panic!("plan error: {e}"))
    }

    /// Finds the callee name of the first `ir::Expr::Call` reachable
    /// from `expr`, walking through `Let`'s value/body — just enough to
    /// find `g`'s own single top-level call in these tests, not a
    /// general-purpose IR walker.
    fn find_call_callee(expr: &ir::Expr) -> Option<&str> {
        match expr {
            ir::Expr::Call { callee, .. } => match callee.as_ref() {
                ir::Expr::Var(name) => Some(name.as_str()),
                _ => None,
            },
            ir::Expr::Let { value, body, .. } => find_call_callee(value).or_else(|| find_call_callee(body)),
            _ => None,
        }
    }

    #[test]
    fn a_global_calling_a_generic_function_is_rewritten_to_the_mangled_instantiation() {
        let src = "let make[T] (x: T): T = x\nlet g = make(5)";
        let plan = plan_for(src);
        assert_eq!(plan.globals.len(), 1);
        assert_eq!(plan.globals[0].name, "g");
        assert_eq!(find_call_callee(&plan.globals[0].value), Some("make$Int"));
        assert!(plan.functions.iter().any(|f| f.name == "make$Int"), "functions: {:?}", plan.functions.iter().map(|f| &f.name).collect::<Vec<_>>());
    }

    #[test]
    fn globals_are_emitted_in_original_source_declaration_order() {
        let src = "let a = 1\nlet b = a + 1\nlet c = b + 1";
        let plan = plan_for(src);
        let names: Vec<&str> = plan.globals.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
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

    /// Finds the first `ir::Expr::Closure` node reachable from `expr`,
    /// walking through the handful of shapes `wrap`'s own body (`{ let
    /// f = |y| y; f(x) }`) actually lowers to — just enough for this
    /// test, not a general-purpose IR walker.
    fn find_closure(expr: &ir::Expr) -> Option<&ir::Expr> {
        match expr {
            ir::Expr::Closure { .. } => Some(expr),
            ir::Expr::Let { value, body, .. } => find_closure(value).or_else(|| find_closure(body)),
            ir::Expr::Call { callee, args, .. } => {
                find_closure(callee).or_else(|| args.iter().find_map(find_closure))
            }
            _ => None,
        }
    }

    #[test]
    fn a_closure_inside_a_generic_function_instantiated_at_two_types_produces_two_independently_typed_specializations() {
        // Mirrors `a_generic_struct_instantiated_at_two_types_produces_
        // two_distinct_mangled_tags_and_no_plain_one`'s own precedent
        // (two distinct mangled outputs from one generic source), just
        // for a closure LITERAL nested inside a generic function's own
        // body instead of a generic struct — the exact case this
        // chunk's two upstream fixes (the `resolve_closure_types`
        // template fallback, and `monomorphize.rs`'s own per-
        // instantiation substitution) exist for.
        let src = "\
            let wrap[T] (x: T): T = { let f = |y| y; f(x) }\n\
            let go_int (): Int = wrap(5)\n\
            let go_bool (): Bool = wrap(true)\n\
        ";
        let plan = plan_for(src);
        let names: Vec<&str> = plan.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"wrap$Int"), "functions: {names:?}");
        assert!(names.contains(&"wrap$Bool"), "functions: {names:?}");

        let int_fn = plan.functions.iter().find(|f| f.name == "wrap$Int").unwrap();
        let bool_fn = plan.functions.iter().find(|f| f.name == "wrap$Bool").unwrap();
        let int_closure = find_closure(&int_fn.body).expect("wrap$Int body should contain a Closure node");
        let bool_closure = find_closure(&bool_fn.body).expect("wrap$Bool body should contain a Closure node");
        let ir::Expr::Closure { param_types: int_params, ret_type: int_ret, .. } = int_closure else {
            unreachable!()
        };
        let ir::Expr::Closure { param_types: bool_params, ret_type: bool_ret, .. } = bool_closure else {
            unreachable!()
        };
        assert_eq!(int_params, &Some(vec![ir::PrimTy::Int]));
        assert_eq!(int_ret, &Some(ir::PrimTy::Int));
        assert_eq!(bool_params, &Some(vec![ir::PrimTy::Bool]));
        assert_eq!(bool_ret, &Some(ir::PrimTy::Bool));
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
