use crate::subst::Subst;
use crate::types::{Type, TypeVarId};
use crate::unify::unify;
use plum_syntax::ast;
use plum_syntax::span::Span;
use std::collections::{HashMap, HashSet};

/// Which kind of declaration a generic instantiation site names — the
/// monomorphization pass (`plum_ir::monomorphize`) needs to know this to
/// decide whether a site is a struct/enum-tag construction (mangles a
/// `Ctor` tag) or a generic function call (mangles a callee name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteKind {
    Struct,
    Enum,
    Function,
}

/// One generic instantiation site captured DURING inference, before the
/// final substitution is known — see this module's top-level docs (or
/// the design conversation this chunk was implemented from) for the
/// "two-tier resolution" story `resolve_generic_sites` builds on top of
/// this. `decl_name` is:
/// - the struct's own name, for `SiteKind::Struct`;
/// - the VARIANT TAG (not the owning enum's name) for `SiteKind::Enum`
///   — this is what a `Ctor`'s `tag` field actually needs mangled, and
///   what `TypeContext::variant_payload_for` is keyed by;
/// - the called function's name, for `SiteKind::Function`.
///
/// `args` are the FRESH type variables minted for this site at the
/// moment it was inferred (from `instantiate_generic`/
/// `instantiate_with_bounds`'s own fresh-var mapping) — genuinely
/// unresolved until `resolve_generic_sites` applies the whole program's
/// final substitution to them.
#[derive(Debug, Clone)]
pub struct RawSite {
    pub kind: SiteKind,
    pub decl_name: String,
    pub args: Vec<Type>,
    pub enclosing_fn: Option<String>,
}

/// `RawSite`, after `resolve_generic_sites` has applied the whole
/// program's final substitution to `args` — every entry is now either a
/// fully concrete type, or (for a "tier 2" site nested inside another
/// generic function's own body, where the argument depends on that
/// OUTER function's own not-yet-instantiated generic) a
/// `Type::Param(name)` TEMPLATE referring to `enclosing_fn`'s own
/// declared generic parameter `name`. `plum_ir::monomorphize::plan`
/// resolves any remaining `Param` once it knows a concrete binding for
/// `enclosing_fn`'s own generics (from the worklist).
#[derive(Debug, Clone)]
pub struct ResolvedSite {
    pub kind: SiteKind,
    pub decl_name: String,
    pub args: Vec<Type>,
    pub enclosing_fn: Option<String>,
}

/// A polytype: `ty` with every variable listed in `vars` universally
/// quantified. An empty `vars` is exactly a monomorphic type — every
/// binding this module used to store directly is really just a Scheme
/// with nothing quantified. Only top-level function bindings are ever
/// generalized (see `generalize`/`Infer::instantiate`); everything else
/// (params, closure args, match bindings, local `let`s) stays
/// monomorphic, matching ML's usual value restriction in spirit — we
/// don't have mutable references yet, so the real motivation doesn't
/// apply, but generalizing arbitrary local bindings isn't needed for
/// anything currently in the language either.
#[derive(Debug, Clone, PartialEq)]
pub struct Scheme {
    pub vars: Vec<TypeVarId>,
    pub ty: Type,
    // Ad-hoc polymorphism bounds (DESIGN.md's `Num`/`Eq`/`Show`) on
    // whichever of `vars` came from an EXPLICIT function-level generic
    // annotation with a bound (`let f[T: Num] (x: T) = ...`) — keyed by
    // the SAME (pre-instantiation) var id `vars` uses. Empty for every
    // monomorphic scheme and for any quantified var with no declared
    // bound. Checked at each CALL site, not here at generalization time
    // — see `Infer::instantiate_with_bounds`'s doc comment for why a
    // function's own generic var is usually still genuinely
    // UNRESOLVED at the point it's generalized, so there's nothing
    // concrete yet to check a bound against.
    pub bounds: HashMap<TypeVarId, Vec<String>>,
}

impl Scheme {
    fn monomorphic(ty: Type) -> Scheme {
        Scheme {
            vars: Vec::new(),
            ty,
            bounds: HashMap::new(),
        }
    }
}

/// Every type variable appearing anywhere inside `ty`.
fn free_vars(ty: &Type) -> HashSet<TypeVarId> {
    match ty {
        Type::Var(id) => {
            let mut set = HashSet::new();
            set.insert(*id);
            set
        }
        Type::Function(params, ret) => {
            let mut set = free_vars(ret);
            for p in params {
                set.extend(free_vars(p));
            }
            set
        }
        Type::Tuple(elems) => {
            let mut set = HashSet::new();
            for e in elems {
                set.extend(free_vars(e));
            }
            set
        }
        Type::Struct(_, args) | Type::Enum(_, args) => {
            let mut set = HashSet::new();
            for a in args {
                set.extend(free_vars(a));
            }
            set
        }
        // `Param` is a declaration-scoped placeholder, never a real
        // inference metavariable — see its doc comment. It should
        // never reach here in practice (every use site instantiates it
        // to a fresh `Var` first), so it contributes no free vars
        // either way.
        Type::Int | Type::Float | Type::Bool | Type::Str | Type::CStr | Type::Unit | Type::Range | Type::Param(_) => {
            HashSet::new()
        }
    }
}

/// A scheme's free variables are its type's free variables MINUS the
/// ones it quantifies over — the quantified ones are bound, not free.
fn free_vars_scheme(scheme: &Scheme) -> HashSet<TypeVarId> {
    let mut vars = free_vars(&scheme.ty);
    for bound in &scheme.vars {
        vars.remove(bound);
    }
    vars
}

/// Generalizing `ty` into a Scheme: quantify every variable that's free
/// in `ty` but NOT free anywhere in `env` — those are the ones truly
/// local to what's being generalized, not still tied to some outer,
/// not-yet-resolved context (e.g. a sibling function in the same
/// mutually-recursive group that hasn't been generalized itself yet).
/// Variables free in `env` must stay exactly as they are, since a
/// FUTURE unification involving that outer context still needs to
/// pin them down consistently — quantifying them here would let each
/// instantiation drift independently, silently losing that constraint.
fn generalize(ty: &Type, env: &TypeEnv) -> Scheme {
    let env_vars: HashSet<TypeVarId> = env.0.iter().flat_map(|(_, s)| free_vars_scheme(s)).collect();
    let vars: Vec<TypeVarId> = free_vars(ty).difference(&env_vars).copied().collect();
    Scheme {
        vars,
        ty: ty.clone(),
        bounds: HashMap::new(),
    }
}

/// A simple scope stack — same shadowing/scoping shape as
/// plum-interp's `env`, just mapping names to Schemes instead of
/// Values. Most bindings are monomorphic Schemes (nothing quantified);
/// only top-level functions are ever stored as genuinely polymorphic
/// ones — see `generalize`.
#[derive(Clone)]
pub struct TypeEnv(Vec<(String, Scheme)>);

impl TypeEnv {
    pub fn new() -> Self {
        TypeEnv(Vec::new())
    }

    pub fn extend(&self, name: String, ty: Type) -> TypeEnv {
        self.extend_scheme(name, Scheme::monomorphic(ty))
    }

    pub fn extend_scheme(&self, name: String, scheme: Scheme) -> TypeEnv {
        let mut v = self.0.clone();
        v.push((name, scheme));
        TypeEnv(v)
    }

    fn lookup_scheme(&self, name: &str) -> Option<&Scheme> {
        self.0.iter().rev().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// Refines every binding already in the env through `subst` — not
    /// just newly-added ones. Without this, a name whose type is
    /// pinned down while inferring one subexpression (e.g. a function
    /// parameter constrained by an `if` condition) keeps its stale,
    /// unconstrained `Var` when a SIBLING subexpression (the `then`
    /// branch) looks it up, so a real conflict there gets a fresh,
    /// uninformed binding instead of being checked against what's
    /// already known — the conflict then silently disappears the
    /// moment substitutions are composed, since `compose` intentionally
    /// prefers the more-authoritative (outer, already-established)
    /// binding for a repeated variable rather than treating it as an
    /// error. Call this any time `acc` grows and more of the same env
    /// will be consulted again.
    ///
    /// Safe to apply to a Scheme's quantified vars too: a bound var's
    /// id, once generalized, never appears in anything unification
    /// touches again (instantiation always mints fresh replacements
    /// before a scheme is used), so `subst` can never have an opinion
    /// about one.
    pub fn apply_subst(&self, subst: &Subst) -> TypeEnv {
        TypeEnv(
            self.0
                .iter()
                .map(|(n, s)| {
                    (
                        n.clone(),
                        Scheme {
                            vars: s.vars.clone(),
                            ty: subst.apply(&s.ty),
                            bounds: s.bounds.clone(),
                        },
                    )
                })
                .collect(),
        )
    }
}

impl Default for TypeEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds the fresh-type-variable counter — the only piece of mutable
/// state inference needs (everything else flows through return values,
/// following the Subst-accumulator idiom described in this module's
/// top-level docs / the surrounding conversation).
pub struct Infer {
    next_var: TypeVarId,
    ctx: crate::context::TypeContext,
    // Records, for every `p.x`-shaped field access encountered, WHICH
    // struct `p` resolved to — keyed by the `Expr::Field` node's own
    // span, since that's the only stable handle lowering (which runs
    // as a totally separate pass over the same AST, with no type
    // information of its own) can use to find its way back to this
    // answer. This is a deliberately narrow side-channel rather than a
    // full "typed IR": lowering still only ever consults this one map,
    // for this one expression shape, instead of every node in the IR
    // carrying a type. See `lower.rs`'s `LoweringContext::field_owners`
    // doc comment for how it's consumed.
    field_owners: HashMap<plum_syntax::span::Span, String>,
    // `for` loops whose iterand resolved to `Array[T]` rather than
    // `Range` — keyed by the `Expr::For` node's own span, for the same
    // reason `field_owners` is span-keyed (lowering has no type
    // information and needs a stable handle back to this answer).
    // `lower_for`'s literal-range fast path never touches this (it's
    // syntactically obvious); only the general fallback needs to know
    // which of the two totally different desugarings — Range-Match-
    // unwrap vs. index-based array loop — applies.
    array_for_loops: std::collections::HashSet<plum_syntax::span::Span>,
    // `Expr::ArrayLiteral` span -> its element type's still-possibly-
    // unresolved `Type`, recorded ONLY for EMPTY literals (`[]`) — see
    // `resolve_empty_array_elem_types`'s doc comment for why this needs
    // its own two-phase resolution, mirroring `generic_sites`/
    // `resolve_generic_sites` exactly, rather than being resolvable
    // in-place the way `field_owners`'s plain `String` payload is: at
    // the literal's OWN inference site the element type is just a
    // fresh, still-unconstrained `Var` (see `infer_expr`'s
    // `ArrayLiteral` arm), and only becomes concrete (if it ever does)
    // once the WHOLE program's final substitution is known.
    empty_array_elem_types: HashMap<Span, Type>,
    // `Expr::Closure` span -> its (still-possibly-unresolved) param
    // types and body/return type — the closure-literal counterpart to
    // `empty_array_elem_types` immediately above, populated by
    // `infer_closure` for the SAME reason: `plum-ir`'s lowering needs a
    // closure's param types to generate its per-literal-site LLVM
    // function signature (`plum-codegen`), and unlike every other IR
    // shape, there's nothing structural in the closure literal itself
    // to derive that from. See `resolve_closure_types`.
    closure_types: HashMap<Span, (Vec<Type>, Type)>,
    // Whether the expression currently being inferred is lexically
    // inside an `unsafe { .. }` block — toggled true/false around
    // `ast::Expr::Unsafe`'s own handling, not a stack (nested `unsafe`
    // just leaves it `true`; there's no reason to ever set it back to
    // `false` partway through an outer `unsafe` block). Consulted only
    // at a `Call` whose callee names a declared `extern` function (see
    // `TypeContext::extern_fn`) — DESIGN.md's "Effect/unsafe tracking"
    // section's whole scope, not a general Koka-style effect system.
    in_unsafe: bool,
    // Every top-level (1+ param) function's name, populated by
    // `infer_program`'s own Phase 1 — used ONLY to check a callback-
    // typed extern argument is a bare reference to a genuine top-level
    // function (see the `Call` case's callback-argument check), never
    // consulted for ordinary type inference. Empty when inference
    // doesn't go through `infer_program` at all (a standalone
    // `infer_expr` test, say) — a callback-argument check in that
    // context always fails closed (rejects), which is fine since a
    // callback-typed extern param can't meaningfully exist without a
    // whole program's `extern` block anyway.
    top_level_fns: HashSet<String>,
    // The name of the top-level `let`-def whose body is CURRENTLY being
    // inferred, mirroring `in_unsafe`'s toggle-around-a-call pattern —
    // set/cleared around each Phase-2 body's `infer_expr` call in
    // `infer_program`, for EVERY top-level def (function or generic-or-
    // not), never just generic ones (see `generic_sites`'s doc comment
    // for why an ordinary function's own generic-type-touching sites
    // still need an `enclosing_fn` recorded). `None` outside any
    // top-level def's own body (globals, and any standalone `infer_expr`
    // call in tests) — a site recorded with `enclosing_fn: None` is
    // always tier-1 (never needs a template `Param`), since there's no
    // enclosing generic function it could depend on.
    current_fn: Option<String>,
    // `current_fn`'s own declared generics, in SOURCE order, paired with
    // the fresh `Var`s `infer_program`'s Phase 2 minted for them
    // (`generic_vars`) — set/cleared in lockstep with `current_fn`.
    // Needed SPECIFICALLY for a SELF-recursive call (`len(t)` inside
    // `len`'s own body): `env.lookup_scheme` for such a call finds
    // Phase 1's MONOMORPHIC placeholder (empty `vars` — the usual self-
    // recursion trick, see `infer_program`'s Phase 1 doc comment), not
    // `current_fn`'s eventual polymorphic scheme (which doesn't exist
    // yet — it's still mid-inference!), so the ordinary `scheme.vars.
    // is_empty()` check that gates recording an ordinary call site is
    // ALWAYS true here, and would otherwise silently miss self-
    // recursive generic calls entirely. A self-recursive call is always
    // exactly at `current_fn`'s OWN generics (calling yourself can't
    // change your own type parameter), so this is what the `Ident` arm
    // falls back to recording from directly when it detects that shape.
    current_fn_generics: Vec<(String, Type)>,
    // Every generic struct/enum construction/pattern site and generic
    // function call site encountered during inference, keyed by that
    // site's own AST span — the raw material `resolve_generic_sites`
    // turns into `ResolvedSite`s once the whole program's final
    // substitution is known. See `RawSite`'s own doc comment. Consumed
    // by `plum_ir::monomorphize::plan`, which is the actual reason this
    // exists (mangled-tag/callee-name monomorphization needs to know,
    // for every AST node that constructs/matches/calls a generic
    // declaration, exactly which concrete type(s) it resolved to).
    generic_sites: HashMap<Span, RawSite>,
    // Every generic function's own declared generic parameter NAMES,
    // paired with the fresh `TypeVarId` `infer_program`'s Phase 2 minted
    // for each one (`generic_vars` in that loop) — in SOURCE (declared)
    // order, which is what makes a mangled name like `identity$Int`
    // deterministic (`Scheme.vars`'s own order comes from a `HashSet`
    // and is NOT reproducible run to run). Populated once per generic
    // function, right after that function's own scheme/bounds are
    // finalized in Phase 2. A function's var id here resolves correctly
    // against `final_subst` regardless of whether `generalize` actually
    // ended up quantifying it (see `resolve_generic_sites`'s doc
    // comment) — `TypeVarId`s are globally unique for the whole compile,
    // so `final_subst.apply(Type::Var(id))` is meaningful either way.
    fn_generics: HashMap<String, Vec<(String, TypeVarId)>>,
    // The final, fully-composed substitution `infer_program` accumulated
    // across its ENTIRE run (Phase 1.5 globals + Phase 2 function
    // bodies) — see this module's/the design conversation's notes on
    // why `acc.apply()` on any `Var` recorded ANYWHERE during the whole
    // compile resolves correctly once `infer_program` returns `Ok`.
    // `None` until `infer_program` finishes; `resolve_generic_sites`
    // requires it to already be set (calling it before/without a
    // successful `infer_program` run is a caller error, reported
    // clearly rather than panicking).
    final_subst: Option<Subst>,
}

impl Infer {
    pub fn new() -> Self {
        Infer {
            next_var: 0,
            ctx: crate::context::TypeContext::new(),
            field_owners: HashMap::new(),
            array_for_loops: std::collections::HashSet::new(),
            empty_array_elem_types: HashMap::new(),
            closure_types: HashMap::new(),
            in_unsafe: false,
            top_level_fns: HashSet::new(),
            current_fn: None,
            current_fn_generics: Vec::new(),
            generic_sites: HashMap::new(),
            fn_generics: HashMap::new(),
            final_subst: None,
        }
    }

    /// For inferring anything that touches struct literals or `match`
    /// — see context.rs. Plain `new()` still works for everything that
    /// doesn't (an empty context just means struct/enum lookups always
    /// fail with "unknown type").
    pub fn with_context(ctx: crate::context::TypeContext) -> Self {
        Infer {
            next_var: 0,
            ctx,
            field_owners: HashMap::new(),
            array_for_loops: std::collections::HashSet::new(),
            empty_array_elem_types: HashMap::new(),
            closure_types: HashMap::new(),
            in_unsafe: false,
            top_level_fns: HashSet::new(),
            current_fn: None,
            current_fn_generics: Vec::new(),
            generic_sites: HashMap::new(),
            fn_generics: HashMap::new(),
            final_subst: None,
        }
    }

    /// The `Expr::Field` span -> owning-struct-name map built up during
    /// inference — `plumc` passes this to `LoweringContext` so `p.x`
    /// can lower correctly. See this struct's `field_owners` doc
    /// comment for why a span-keyed side-channel, not a typed IR.
    pub fn field_owners(&self) -> &HashMap<plum_syntax::span::Span, String> {
        &self.field_owners
    }

    /// The set of `for` loops (keyed by the `Expr::For` node's own
    /// span) whose iterand is `Array[T]` rather than `Range` — see
    /// `array_for_loops`'s doc comment.
    pub fn array_for_loops(&self) -> &std::collections::HashSet<plum_syntax::span::Span> {
        &self.array_for_loops
    }

    /// Every generic function's own declared generic parameter names,
    /// paired with the `TypeVarId` each one resolves through — see
    /// `fn_generics`'s own doc comment. `plum_ir::monomorphize::plan`
    /// needs this to know, for a given concrete instantiation's
    /// argument list, which name each positional argument binds to.
    pub fn fn_generics(&self) -> &HashMap<String, Vec<(String, TypeVarId)>> {
        &self.fn_generics
    }

    /// Resolves every EMPTY array literal's element type against the
    /// whole program's final substitution — mirrors `resolve_generic_
    /// sites` exactly (same "must be called after a successful
    /// `infer_program`" precondition, same reason), just for the
    /// simpler `empty_array_elem_types` side table instead of
    /// `generic_sites`. Unlike `resolve_generic_sites`, there's no
    /// tier-2 "template" fallback here (an empty array literal is never
    /// itself inside a generic declaration's OWN type parameter the way
    /// a generic function call site can be) — a still-unresolved `Var`
    /// after the final substitution is always a genuine ambiguity
    /// (`let x = []` with `x` never used anywhere that would pin its
    /// element type), reported clearly rather than silently defaulted.
    pub fn resolve_empty_array_elem_types(&self) -> Result<HashMap<Span, Type>, String> {
        let subst = self
            .final_subst
            .as_ref()
            .ok_or_else(|| "internal error: resolve_empty_array_elem_types called before infer_program completed".to_string())?;
        let mut out = HashMap::with_capacity(self.empty_array_elem_types.len());
        for (span, ty) in &self.empty_array_elem_types {
            let resolved = subst.apply(ty);
            if matches!(resolved, Type::Var(_)) {
                return Err(format!(
                    "cannot determine the element type of the empty array literal at {span:?} — it's never \
                     used anywhere that would pin its element type to something concrete"
                ));
            }
            out.insert(*span, resolved);
        }
        Ok(out)
    }

    /// Resolves every closure LITERAL's param/return types against the
    /// whole program's final substitution — mirrors `resolve_empty_
    /// array_elem_types` exactly (same precondition, same "still a
    /// `Var` after the final substitution is a genuine ambiguity, not
    /// silently defaulted" reasoning), just for `closure_types` instead.
    /// `plum_ir::lower::LoweringContext::with_closure_types` is where
    /// the result gets consumed.
    pub fn resolve_closure_types(&self) -> Result<HashMap<Span, (Vec<Type>, Type)>, String> {
        let subst = self
            .final_subst
            .as_ref()
            .ok_or_else(|| "internal error: resolve_closure_types called before infer_program completed".to_string())?;
        let mut out = HashMap::with_capacity(self.closure_types.len());
        for (span, (param_tys, ret_ty)) in &self.closure_types {
            let resolved_params: Vec<Type> = param_tys.iter().map(|t| subst.apply(t)).collect();
            let resolved_ret = subst.apply(ret_ty);
            if resolved_params.iter().any(|t| matches!(t, Type::Var(_))) || matches!(resolved_ret, Type::Var(_)) {
                return Err(format!(
                    "cannot determine a concrete param/return type for the closure literal at {span:?} — it's \
                     never used anywhere that would pin its type to something concrete"
                ));
            }
            out.insert(*span, (resolved_params, resolved_ret));
        }
        Ok(out)
    }

    /// Turns every `RawSite` captured during inference into a
    /// `ResolvedSite`, by applying the whole program's final
    /// substitution to each one's `args` — see `RawSite`'s and
    /// `ResolvedSite`'s own doc comments for the two-tier resolution
    /// this performs. Must be called AFTER `infer_program` has returned
    /// `Ok` (an internal-error `Result`, not a panic, if called before
    /// that or after a failed run — `final_subst` is only ever set at
    /// the very end of a successful `infer_program`).
    pub fn resolve_generic_sites(&self) -> Result<HashMap<Span, ResolvedSite>, String> {
        let subst = self
            .final_subst
            .as_ref()
            .ok_or_else(|| "internal error: resolve_generic_sites called before infer_program completed".to_string())?;
        let mut out = HashMap::with_capacity(self.generic_sites.len());
        for (span, raw) in &self.generic_sites {
            let mut resolved_args = Vec::with_capacity(raw.args.len());
            for arg in &raw.args {
                let resolved = subst.apply(arg);
                let final_arg = if matches!(resolved, Type::Var(_)) {
                    self.resolve_as_template(&resolved, raw, subst).ok_or_else(|| {
                        format!(
                            "cannot determine a concrete type for {:?} at {span:?} — its type parameter is \
                             never pinned to a concrete type anywhere it's used",
                            raw.decl_name
                        )
                    })?
                } else {
                    resolved
                };
                resolved_args.push(final_arg);
            }
            out.insert(
                *span,
                ResolvedSite {
                    kind: raw.kind,
                    decl_name: raw.decl_name.clone(),
                    args: resolved_args,
                    enclosing_fn: raw.enclosing_fn.clone(),
                },
            );
        }
        Ok(out)
    }

    /// `resolved` is a still-unresolved `Type::Var` after applying the
    /// program's final substitution — checks whether it's actually a
    /// "tier 2" TEMPLATE: exactly the enclosing function's own declared
    /// generic parameter, not a genuine ambiguity. Comparing
    /// `subst.apply(&Type::Var(gid)) == resolved` (rather than a direct
    /// id equality check on `resolved`'s own var id) is what makes this
    /// correct regardless of which DIRECTION `unify`'s `bind_var` chose
    /// when it originally connected this site's fresh var to the
    /// enclosing function's own generic var — `apply` chains fully in
    /// either direction, so both sides land on the same representative
    /// var (or concrete type) once fully resolved. See the design
    /// conversation this chunk was implemented from for a worked
    /// example (`wrap[T](x: T): Box[T]`).
    fn resolve_as_template(&self, resolved: &Type, raw: &RawSite, subst: &Subst) -> Option<Type> {
        let fn_name = raw.enclosing_fn.as_ref()?;
        let generics = self.fn_generics.get(fn_name)?;
        for (name, var_id) in generics {
            if subst.apply(&Type::Var(*var_id)) == *resolved {
                return Some(Type::Param(name.clone()));
            }
        }
        None
    }

    /// Generates a never-before-used type variable — used internally
    /// for unannotated function/closure parameters and call return
    /// types, and exposed publicly since it's a reasonable thing for a
    /// consumer of this crate to want directly too.
    pub fn fresh(&mut self) -> Type {
        let id = self.next_var;
        self.next_var += 1;
        Type::Var(id)
    }

    /// Instantiates a Scheme: every quantified variable gets replaced
    /// with a BRAND NEW fresh variable, independently at each call —
    /// this is what makes `identity(true)` and `identity(1)` in the
    /// same program both type-check even though they need `identity`
    /// to behave as `Bool -> Bool` and `Int -> Int` respectively. A
    /// monomorphic scheme (empty `vars`, the common case) instantiates
    /// to exactly its own type, unchanged. Discards bound-checking
    /// entirely — for a BARE reference to a bounded generic function
    /// (not called), same shape as struct/enum construction skipping a
    /// still-unresolved `Var`. See `instantiate_with_bounds` for the
    /// version an actual CALL site needs.
    fn instantiate(&mut self, scheme: &Scheme) -> Type {
        self.instantiate_with_bounds(scheme).0
    }

    /// Records a generic instantiation site — see `RawSite`'s doc
    /// comment. Only called for a site whose args are actually generic
    /// (`instantiate_generic` already returns an empty `args` for a
    /// non-generic declaration, and the function-call sites only call
    /// this when the callee's scheme is genuinely polymorphic), so
    /// there's no separate "is this generic at all" check here.
    fn record_site(&mut self, kind: SiteKind, decl_name: &str, args: &[Type], span: Span) {
        self.generic_sites.insert(
            span,
            RawSite {
                kind,
                decl_name: decl_name.to_string(),
                args: args.to_vec(),
                enclosing_fn: self.current_fn.clone(),
            },
        );
    }

    /// Same substitution `instantiate` does, but ALSO returns which of
    /// the freshly-minted vars carry a bound (`scheme.bounds`, re-keyed
    /// from the scheme's original var ids to the NEW fresh ones) —
    /// what a real call site needs to actually CHECK a bound.
    ///
    /// Bounds are deliberately checked HERE, at instantiation/call time,
    /// never back when the scheme was generalized: a function's own
    /// generic var (`let f[T: Num] (x: T) = ...`) is usually still
    /// completely UNRESOLVED at that point — nothing inside `f`'s own
    /// body necessarily pins `T` to any concrete type, so there's
    /// nothing yet to check it against. It only becomes checkable once
    /// a REAL call supplies a concrete argument, exactly mirroring why
    /// `Infer::check_generic_bounds` (struct/enum construction) checks
    /// the FINAL resolved argument, not the fresh var `instantiate_generic`
    /// mints. The caller is responsible for actually running the check
    /// AFTER unifying call arguments — see `infer_call_with_callee`.
    ///
    /// ALSO returns the old-quantified-var -> fresh-type mapping this
    /// instantiation minted — needed by a generic-function-call site to
    /// figure out, in `fn_generics[callee]`'s DECLARED order, which
    /// fresh type each of the callee's own generic parameters got (see
    /// `RawSite`'s doc comment): `scheme.vars`' own order is a
    /// `HashSet`-derived one and can't be relied on for that.
    fn instantiate_with_bounds(
        &mut self,
        scheme: &Scheme,
    ) -> (Type, Vec<(TypeVarId, Vec<String>)>, HashMap<TypeVarId, Type>) {
        if scheme.vars.is_empty() {
            return (scheme.ty.clone(), Vec::new(), HashMap::new());
        }
        let mut subst = Subst::empty();
        let mut pending = Vec::new();
        let mut mapping = HashMap::new();
        for &v in &scheme.vars {
            let fresh = self.fresh();
            mapping.insert(v, fresh.clone());
            if let Some(bounds) = scheme.bounds.get(&v) {
                let Type::Var(fresh_id) = &fresh else {
                    unreachable!("Infer::fresh always returns Type::Var");
                };
                pending.push((*fresh_id, bounds.clone()));
            }
            subst = Subst::single(v, fresh).compose(&subst);
        }
        (subst.apply(&scheme.ty), pending, mapping)
    }

    /// Given a generic function's callee `name` and the fresh-var
    /// `mapping` an instantiation just minted, records a `RawSite` IF
    /// `name` is a known generic function (see `fn_generics`) — shared
    /// by both places a generic function's callee gets instantiated
    /// (the plain `Ident` lookup path and the bounded-call special
    /// case). `args` are built in `fn_generics[name]`'s DECLARED order,
    /// falling back to the pre-instantiation `Type::Var(old_id)` itself
    /// when `mapping` has no entry for it (meaning `generalize` didn't
    /// end up quantifying it — see `fn_generics`'s doc comment for why
    /// that fallback is still correct, not just defensive).
    fn record_fn_call_site(&mut self, name: &str, mapping: &HashMap<TypeVarId, Type>, span: Span) {
        let Some(decl_generics) = self.fn_generics.get(name).cloned() else {
            return;
        };
        if decl_generics.is_empty() {
            return;
        }
        let args: Vec<Type> = decl_generics
            .iter()
            .map(|(_, old_id)| mapping.get(old_id).cloned().unwrap_or(Type::Var(*old_id)))
            .collect();
        self.record_site(SiteKind::Function, name, &args, span);
    }

    /// Instantiates a generic struct/enum declaration named `decl_name`
    /// at a CONSTRUCTION or PATTERN site: mints one brand-new fresh
    /// `Var` per declared parameter name (shared across every field —
    /// `struct Pair[T] { first: T, second: T }` gets the SAME fresh var
    /// substituted for both occurrences of `T`), substitutes those into
    /// `field_types` (declared field/payload types, which may contain
    /// `Type::Param`), and returns both the substituted field types AND
    /// the ordered `Vec<Type>` of fresh args — the second is exactly
    /// what a `Type::Struct(decl_name, args)`/`Type::Enum(decl_name,
    /// args)` result type needs. A non-generic declaration (no declared
    /// params) is unaffected: `field_types` pass through unchanged and
    /// `args` comes back empty, exactly like before generics existed.
    fn instantiate_generic(&mut self, decl_name: &str, field_types: &[Type]) -> (Vec<Type>, Vec<Type>) {
        let param_names: Vec<String> = self.ctx.generic_params(decl_name).unwrap_or(&[]).to_vec();
        if param_names.is_empty() {
            return (field_types.to_vec(), Vec::new());
        }
        let mapping: HashMap<String, Type> = param_names.iter().map(|p| (p.clone(), self.fresh())).collect();
        let substituted = field_types.iter().map(|t| subst_params(t, &mapping)).collect();
        let args = param_names.iter().map(|p| mapping[p].clone()).collect();
        (substituted, args)
    }

    /// Checks `resolved_args` (a construction site's FINAL, fully-
    /// resolved generic arguments — `acc.apply`'d, not the fresh vars
    /// `instantiate_generic` minted) against `decl_name`'s own declared
    /// bounds, positionally. Only ever called at a CONSTRUCTION site
    /// (a struct literal, a variant call, a bare variant constructor),
    /// never at a pattern-match site — once a value exists, it was
    /// only ever constructible if it already satisfied its bounds, so
    /// re-checking on every match would be redundant. An argument
    /// that's STILL an unresolved `Type::Var` (nothing pinned it to a
    /// concrete type yet — e.g. a bare `None` from `Option[T: Num]`)
    /// is skipped rather than rejected: there's nothing concrete yet
    /// to check a bound against.
    fn check_generic_bounds(&self, decl_name: &str, resolved_args: &[Type], span: plum_syntax::span::Span) -> Result<(), String> {
        let Some(bounds) = self.ctx.generic_bounds(decl_name) else {
            return Ok(());
        };
        for (arg_ty, param_bounds) in resolved_args.iter().zip(bounds.iter()) {
            if matches!(arg_ty, Type::Var(_)) {
                continue;
            }
            for bound in param_bounds {
                if !satisfies_bound(arg_ty, bound) {
                    return Err(format!(
                        "{decl_name:?} requires its type argument to satisfy `{bound}`, but {arg_ty:?} does not \
                         (at {span:?})"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolves a function param/return annotation, ALSO understanding
    /// that function's own declared generic names (`generic_vars`,
    /// built once per function in `infer_program` — see its own doc
    /// comment) — the function-signature counterpart to `ast_type_to_type`'s
    /// `in_scope_params`/`Type::Param` handling for struct/enum
    /// declarations, but deliberately NOT the same mechanism: a
    /// function's own generic name resolves DIRECTLY to the one fresh
    /// `Var` `generic_vars` minted for it, not to a `Type::Param`
    /// template. There's no separate "instantiate later" step needed
    /// here the way a struct/enum construction site needs one — a
    /// function's body is inferred exactly ONCE (right here, in Phase
    /// 2), and per-call-site polymorphism already comes entirely from
    /// ordinary `generalize`/`instantiate` on the RESULT of that single
    /// inference, ordinary Hindley-Milner let-polymorphism machinery
    /// that already existed before this method did.
    ///
    /// Everything else (primitives, a struct/enum name, a generic
    /// instantiation like `Option[T]` where `T` is itself one of this
    /// function's generics) falls through to `ast_type_to_type`,
    /// recursing back through THIS method for any nested type
    /// arguments so a function generic can appear arbitrarily deep
    /// (`Option[Pair[T]]` and similar).
    ///
    /// CALLER MUST `acc.apply()` the result before unifying with it.
    /// A `generic_vars` entry is a RAW `Var` fixed once per function,
    /// not re-resolved here — by the time a LATER annotation in the
    /// same signature (e.g. the return type, checked after every
    /// parameter) consults it, `acc` may already have resolved that
    /// same `Var` further. Skipping this step once during development
    /// produced a genuine infinite loop: unifying an un-applied stale
    /// `Var` could bind it back onto something `acc` already resolved
    /// FROM it, creating a self-referential `Subst` entry that
    /// `Subst::apply`'s chain-following recurses on forever.
    fn resolve_annotation(&self, ty: &ast::Type, generic_vars: &HashMap<String, Type>) -> Result<Type, String> {
        match ty {
            ast::Type::Path(segments, _) => match segments.last() {
                Some(name) if generic_vars.contains_key(name) => Ok(generic_vars[name].clone()),
                _ => ast_type_to_type(ty, &self.ctx, &[]),
            },
            ast::Type::Generic { base, args, span } => {
                let name = base.last().cloned().ok_or_else(|| {
                    format!("type inference not yet implemented for this type annotation at {span:?}")
                })?;
                // The opaque pseudo-generic builtin types (`Array[T]`,
                // `Task[T]`, `Sender[T]`, `Receiver[T]`, `Ref[T]`) are
                // DELIBERATELY never registered in `self.ctx` (see the
                // type's own construction sites — e.g. `.push()`'s
                // `Type::Struct("Array", ...)` — for why: they exist
                // purely for their structural unify behavior, not as
                // real declarations). That means `ctx.generic_params`
                // can never answer for them, so they need their own
                // fixed-arity-one check here, checked BEFORE falling
                // through to the ordinary ctx-registered-declaration
                // path below.
                if matches!(name.as_str(), "Array" | "Task" | "Sender" | "Receiver" | "Ref") {
                    if args.len() != 1 {
                        return Err(format!(
                            "{name:?} expects 1 generic argument, found {} at {span:?}",
                            args.len()
                        ));
                    }
                    let resolved_arg = self.resolve_annotation(&args[0], generic_vars)?;
                    return Ok(Type::Struct(name, vec![resolved_arg]));
                }
                let Some(declared_params) = self.ctx.generic_params(&name) else {
                    return Err(format!(
                        "type inference not yet implemented for this type annotation at {span:?}"
                    ));
                };
                if args.len() != declared_params.len() {
                    return Err(format!(
                        "{name:?} expects {} generic argument(s), found {} at {span:?}",
                        declared_params.len(),
                        args.len()
                    ));
                }
                let resolved_args = args
                    .iter()
                    .map(|a| self.resolve_annotation(a, generic_vars))
                    .collect::<Result<Vec<_>, _>>()?;
                if self.ctx.is_struct(&name) {
                    Ok(Type::Struct(name, resolved_args))
                } else if self.ctx.is_enum(&name) {
                    Ok(Type::Enum(name, resolved_args))
                } else {
                    Err(format!(
                        "type inference not yet implemented for this type annotation at {span:?}"
                    ))
                }
            }
            // `(T) -> T`-shaped annotation — recurses through `self`
            // (not straight to `ast_type_to_type`) so a function's OWN
            // generic parameter can appear inside a callback-typed
            // param/return (`fn apply(f: (T) -> T, x: T) -> T`), same
            // reasoning as the `Path` arm just above.
            ast::Type::Function { params, ret, .. } => {
                let param_types = params
                    .iter()
                    .map(|p| self.resolve_annotation(p, generic_vars))
                    .collect::<Result<Vec<_>, _>>()?;
                let ret_type = self.resolve_annotation(ret, generic_vars)?;
                Ok(Type::Function(param_types, Box::new(ret_type)))
            }
        }
    }

    /// Infers a type for every `let`-defined function in a program.
    /// See this method's implementation comment for the two-phase
    /// approach (pre-declare signatures, then infer bodies) that makes
    /// self- and mutual recursion work.
    pub fn infer_program(&mut self, program: &ast::Program) -> Result<HashMap<String, Type>, String> {
        // Phase 1: pre-declare EVERY function's signature with fresh
        // type variables before inferring any body — this is what
        // makes self- and mutual recursion type-check. A recursive (or
        // mutually recursive) call finds a signature already sitting
        // in the environment, even though it isn't resolved to
        // concrete types yet; unification fills those in as bodies get
        // processed.
        let mut global_env = TypeEnv::new();
        let mut signatures: HashMap<String, (Vec<Type>, Type)> = HashMap::new();
        let mut defs: Vec<&ast::LetDef> = Vec::new();
        // Zero-parameter top-level `let`s, collected separately and IN
        // FILE ORDER — see ir.rs's `Global` doc comment (plum-ir) for
        // why order among them matters, unlike functions.
        let mut global_defs: Vec<&ast::LetDef> = Vec::new();
        // Functions and globals share ONE flat top-level namespace
        // (both ultimately live in `global_env`/`Interpreter::
        // functions`+`globals`) — a name reused between them, or
        // between two functions, previously just silently overwrote
        // whichever `HashMap` entry came first, with NO warning; two
        // same-named FUNCTIONS were actively broken (both bodies got
        // processed, the second one's `global_env` entry winning,
        // regardless of which one a caller probably meant). Checked
        // here, before anything else touches `signatures`/`global_defs`.
        let mut declared_names: HashSet<String> = HashSet::new();

        // Phase 0: declared `extern "C"` functions — their signatures
        // are already fully concrete (no fresh type variables needed,
        // unlike an ordinary function's Phase 1 below, since an extern
        // signature can never be recursive or need inference of its
        // own). `TypeContext::from_items` already resolved and
        // validated each one; this just seeds `global_env` so an
        // ordinary `Call` against the name type-checks through the
        // normal path once `unsafe`-gating (see `in_unsafe`) lets it
        // through. Extern names share the same top-level namespace as
        // functions/globals — checked here too, so `extern "C" { fn
        // sqrt(..); } let sqrt = 1` collides loudly instead of one
        // silently shadowing the other.
        for item in &program.items {
            if let ast::ItemKind::Extern(block) = &item.kind {
                for f in &block.fns {
                    if !declared_names.insert(f.name.clone()) {
                        return Err(format!("{:?} is already declared (at {:?})", f.name, f.span));
                    }
                    let (param_types, ret_type) = self
                        .ctx
                        .extern_fn(&f.name)
                        .cloned()
                        .ok_or_else(|| format!("internal error: extern function {:?} not in context", f.name))?;
                    let fn_ty = Type::Function(param_types, Box::new(ret_type));
                    global_env = global_env.extend(f.name.clone(), fn_ty);
                }
            }
        }

        for item in &program.items {
            if let ast::ItemKind::Let(def) = &item.kind {
                if !declared_names.insert(def.name.clone()) {
                    return Err(format!("{:?} is already declared (at {:?})", def.name, def.span));
                }
                if def.params.is_empty() {
                    global_defs.push(def);
                    continue;
                }
                let param_vars: Vec<Type> = def.params.iter().map(|_| self.fresh()).collect();
                let ret_var = self.fresh();
                let fn_ty = Type::Function(param_vars.clone(), Box::new(ret_var.clone()));
                global_env = global_env.extend(def.name.clone(), fn_ty);
                signatures.insert(def.name.clone(), (param_vars, ret_var));
                self.top_level_fns.insert(def.name.clone());
                defs.push(def);
            }
        }

        let mut acc = Subst::empty();

        // Phase 1.5: globals, in file order, BEFORE any function body
        // is inferred — a global stays monomorphic (never generalized,
        // same as a block-level `let`), and each one's initializer sees
        // every function (Phase 1 above pre-declared every signature
        // regardless of order) plus every EARLIER global (this loop
        // extends `global_env` immediately after each one, so a LATER
        // global or a function body can see it — see ir.rs's `Global`
        // doc comment for why the reverse, a global seeing a LATER
        // global, isn't meaningful and isn't supported).
        let mut global_types: HashMap<String, Type> = HashMap::new();
        for def in &global_defs {
            // Self-referential closures (`let fib = |n| .. fib(n-1) ..`)
            // need `def.name` visible to `def.body`'s OWN inference
            // when `def.body` is itself a closure literal — pre-bind it
            // to a fresh placeholder type first, same "fresh var now,
            // unify with the real type after" trick Phase 1 above
            // already uses for top-level FUNCTION self/mutual
            // recursion. Deliberately SELF-recursion only, not mutual
            // recursion between two closure-valued globals: unlike
            // functions, globals are never pre-declared as a whole
            // batch (see this loop's own doc comment on why a global
            // seeing a LATER global isn't supported), so only a
            // global's OWN name is ever added early, never a
            // still-to-come sibling's. The interpreter needs NO
            // matching fix for this case (unlike the local-block-let
            // case below) — `Interpreter::load_program` evaluates every
            // global's initializer before any closure is ever actually
            // CALLED, and closures resolve free names through `self.
            // globals` at call time regardless of what was captured at
            // creation time, so a recursive call into a global name
            // already just works once `self.globals` is fully
            // populated.
            let is_closure_literal = matches!(def.body, ast::Expr::Closure { .. });
            let (ty, s) = if is_closure_literal {
                let placeholder = self.fresh();
                let rec_env = global_env.extend(def.name.clone(), placeholder.clone());
                let (body_ty, s) = self.infer_expr(&def.body, &rec_env)?;
                let mut acc2 = s;
                let s2 = unify(&acc2.apply(&placeholder), &acc2.apply(&body_ty))
                    .map_err(|e| format!("recursive closure {:?}: {e}", def.name))?;
                acc2 = s2.compose(&acc2);
                (acc2.apply(&body_ty), acc2)
            } else {
                self.infer_expr(&def.body, &global_env)?
            };
            acc = s.compose(&acc);
            let resolved = acc.apply(&ty);
            global_env = global_env.extend(def.name.clone(), resolved.clone());
            global_env = global_env.apply_subst(&acc);
            global_types.insert(def.name.clone(), resolved);
        }

        // Phase 2: infer each function body against the SHARED global
        // env (so it can call itself, any sibling function, OR any
        // global), threading the SAME substitution accumulator — a
        // call from function A into function B's still-fresh signature
        // has to be able to constrain B before B's own body gets
        // processed.
        for def in &defs {
            let (param_vars, ret_var) = signatures.get(&def.name).cloned().expect("just inserted above");
            let mut body_env = global_env.apply_subst(&acc);
            // One fresh `Var` per THIS function's own declared generic
            // name (`let pair[T] (a: T) (b: T): T = a` mints ONE var for
            // `T`, shared across every annotation that mentions it) —
            // see `resolve_annotation`'s doc comment for why this is a
            // direct `Var` substitution, not the `Type::Param`-template
            // machinery struct/enum declarations use.
            let generic_vars: HashMap<String, Type> =
                def.generics.iter().map(|g| (g.name.clone(), self.fresh())).collect();
            for (param, param_ty) in def.params.iter().zip(param_vars.iter()) {
                match &param.kind {
                    ast::ParamKind::Ident(name) => {
                        body_env = body_env.extend(name.clone(), acc.apply(param_ty));
                    }
                    // `(x: Int)` — a declared PARAMETER annotation,
                    // previously parsed (`ParamKind::Pattern`'s second
                    // field) but silently discarded here regardless —
                    // every param got a bare fresh var no matter what
                    // it said. Now resolved (via `resolve_annotation`,
                    // which ALSO understands this function's own
                    // declared generic names — `x: T` resolves to the
                    // SAME `Var` `generic_vars` minted for `T`, shared
                    // across every annotation in this signature that
                    // mentions it) and unified against that fresh var —
                    // a MISMATCH between the annotation and how the
                    // body actually uses the parameter is now a real,
                    // reported type error, not silently accepted.
                    ast::ParamKind::Pattern(ast::Pattern::Ident(name, _), annotation) => {
                        if let Some(ty) = annotation {
                            let annotated_ty = self.resolve_annotation(ty, &generic_vars)?;
                            let s = unify(&acc.apply(param_ty), &acc.apply(&annotated_ty)).map_err(|e| {
                                format!("function {:?} parameter {name:?}: {e}", def.name)
                            })?;
                            acc = s.compose(&acc);
                        }
                        body_env = body_env.extend(name.clone(), acc.apply(param_ty));
                    }
                    // `bind_pattern` unifies `param_ty` (the single
                    // fresh var Phase 1 gave this flat-arity parameter)
                    // against whatever shape the pattern requires and
                    // binds every name it introduces, including nested
                    // ones — see `bind_pattern`'s doc comment. An
                    // annotation here (`(Point { x, y }: Point)`) is
                    // checked the SAME way, before `bind_pattern` runs
                    // — largely redundant with what the pattern itself
                    // already implies, but still a real, honest check
                    // rather than a silently ignored one.
                    ast::ParamKind::Pattern(pattern @ (ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. }), annotation) => {
                        if let Some(ty) = annotation {
                            let annotated_ty = self.resolve_annotation(ty, &generic_vars)?;
                            let s = unify(&acc.apply(param_ty), &acc.apply(&annotated_ty))
                                .map_err(|e| format!("function {:?} parameter: {e}", def.name))?;
                            acc = s.compose(&acc);
                        }
                        body_env = self
                            .bind_pattern(pattern, &acc.apply(param_ty), body_env, &mut acc)
                            .map_err(|e| format!("function {:?} parameter: {e}", def.name))?;
                    }
                    _ => {
                        return Err(format!(
                            "type inference not yet implemented for destructuring function \
                             parameters of this shape at {:?}",
                            param.span
                        ));
                    }
                }
            }
            // Every construction/call site inside THIS function's own
            // body records `enclosing_fn: Some(def.name)` — see
            // `current_fn`'s doc comment. Set for EVERY function, not
            // just generic ones: an ordinary function's own generic-
            // type-touching sites still need an owner recorded (they're
            // trivially tier-1, but `plum_ir::monomorphize` still needs
            // to know which function's body to rewrite).
            self.current_fn = Some(def.name.clone());
            self.current_fn_generics =
                def.generics.iter().map(|g| (g.name.clone(), generic_vars[&g.name].clone())).collect();
            let (body_ty, s) = self.infer_expr(&def.body, &body_env)?;
            self.current_fn = None;
            self.current_fn_generics = Vec::new();
            acc = s.compose(&acc);
            let s = unify(&acc.apply(&body_ty), &acc.apply(&ret_var)).map_err(|e| {
                format!("function {:?}: body type does not match its return type: {e}", def.name)
            })?;
            acc = s.compose(&acc);

            // A declared return-type annotation (`let f x: Bool = ...`)
            // was PARSED (`LetDef.ret_ty`) but never actually consulted
            // here before — silently accepted regardless of what it
            // said, unlike a closure or `let`-binding annotation, both
            // of which already go through `ast_type_to_type`. Unify it
            // against what the body actually produced, same as any
            // other annotation.
            if let Some(annotated) = &def.ret_ty {
                let annotated_ty = self.resolve_annotation(annotated, &generic_vars)?;
                let s = unify(&acc.apply(&ret_var), &acc.apply(&annotated_ty)).map_err(|e| {
                    format!("function {:?}: declared return type does not match its body: {e}", def.name)
                })?;
                acc = s.compose(&acc);
            }

            // Critical: refresh THIS function's entry in `global_env`
            // to what was actually just learned about it, not the raw
            // Phase-1 placeholder. Without this, a LATER function
            // calling this one would unify against unconstrained fresh
            // variables instead of the real signature — silently
            // accepting calls that should be type errors, and leaving
            // callers' inferred types full of never-resolved variables.
            // `extend` appends rather than replaces, but `lookup` scans
            // from the end, so this correctly shadows the old entry.
            let resolved_fn_ty = Type::Function(
                param_vars.iter().map(|t| acc.apply(t)).collect(),
                Box::new(acc.apply(&ret_var)),
            );
            // Generalize against every OTHER function's current best-
            // known signature (deliberately excluding `def`'s own — its
            // own placeholder resolves to exactly `resolved_fn_ty`, so
            // including it would "protect" every one of its own free
            // vars from generalization and defeat the point). A
            // variable still free in some not-yet-processed sibling's
            // Phase-1 placeholder must stay exactly as-is, since a
            // later mutually-recursive call still needs unification to
            // pin it down consistently across both functions, not let
            // each instantiation drift independently.
            let mut outer_env = TypeEnv::new();
            for (name, (p_vars, r_var)) in &signatures {
                if name == &def.name {
                    continue;
                }
                let fn_ty = Type::Function(p_vars.iter().map(|t| acc.apply(t)).collect(), Box::new(acc.apply(r_var)));
                outer_env = outer_env.extend(name.clone(), fn_ty);
            }
            let mut scheme = generalize(&resolved_fn_ty, &outer_env);
            // Attach THIS function's own declared generic bounds to the
            // scheme, keyed by whichever var id each generic name
            // resolved to — usually still a genuinely free `Var` (which
            // `generalize` just quantified over), recorded for
            // `instantiate_with_bounds` to check later at each call
            // site. If a generic ALREADY resolved to something concrete
            // (the function's own body pinned it internally — e.g. `let
            // f[T: Num] (x: T): T = x + 1` forces `T = Int` on its own),
            // there's no var left to attach a bound to — checked RIGHT
            // NOW instead, the same "check as soon as it's checkable"
            // principle `instantiate_with_bounds` follows for the
            // call-site case.
            for g in &def.generics {
                if g.bound.is_empty() {
                    continue;
                }
                let Some(var) = generic_vars.get(&g.name) else {
                    continue;
                };
                match acc.apply(var) {
                    Type::Var(id) => {
                        scheme.bounds.insert(id, g.bound.clone());
                    }
                    resolved => {
                        for bound in &g.bound {
                            if !satisfies_bound(&resolved, bound) {
                                return Err(format!(
                                    "function {:?}: generic parameter {:?} requires `{bound}`, but {resolved:?} does not satisfy it",
                                    def.name, g.name
                                ));
                            }
                        }
                    }
                }
            }
            global_env = global_env.extend_scheme(def.name.clone(), scheme);

            // Recorded regardless of whether `generalize` ended up
            // in SOURCE order, which is what makes a mangled name
            // deterministic. Recorded as `acc.apply(generic_vars[name])`
            // — NOT the raw `generic_vars[name]` id itself — because a
            // function whose body naturally unifies its own generic
            // with some OTHER var (e.g. `let identity[T] (x: T): T = x`
            // unifies `T`'s var with the RETURN var, and `generalize`'s
            // `free_vars` scan finds whichever one survives as the
            // representative, not necessarily the original) means the
            // var id that ends up in `scheme.vars` — and therefore the
            // one a later CALL site's `instantiate_with_bounds` mapping
            // is actually keyed by — can differ from the original id
            // `generic_vars` minted. Applying `acc` here resolves to
            // that same representative, so `record_fn_call_site`'s
            // `mapping.get(old_id)` lookup actually hits. A generic
            // that resolved to something CONCRETE internally (no var
            // left at all — e.g. a bound forces it, `T: Num` used as
            // `x + 1`) is skipped: there's no call-site-varying
            // instantiation to track for it anymore, since every call
            // is forced to the same fixed type regardless of the
            // caller's argument.
            if !def.generics.is_empty() {
                let ordered: Vec<(String, TypeVarId)> = def
                    .generics
                    .iter()
                    .filter_map(|g| {
                        let var = generic_vars.get(&g.name)?;
                        match acc.apply(var) {
                            Type::Var(id) => Some((g.name.clone(), id)),
                            _ => None,
                        }
                    })
                    .collect();
                self.fn_generics.insert(def.name.clone(), ordered);
            }
        }

        self.final_subst = Some(acc.clone());

        let mut result = HashMap::new();
        for (name, (param_vars, ret_var)) in &signatures {
            let params = param_vars.iter().map(|t| acc.apply(t)).collect();
            let ret = acc.apply(ret_var);
            result.insert(name.clone(), Type::Function(params, Box::new(ret)));
        }
        // Re-applying the FINAL acc here (not just what was known when
        // each global was inferred) matters: a global calling a
        // function declared LATER in the file only saw that function's
        // still-fresh Phase-1 placeholder at the time, and Phase 2
        // resolves it further — same reasoning as the function-vs-
        // function cross-reference fix above.
        for (name, ty) in &global_types {
            result.insert(name.clone(), acc.apply(ty));
        }
        Ok(result)
    }

    pub fn infer_expr(&mut self, expr: &ast::Expr, env: &TypeEnv) -> Result<(Type, Subst), String> {
        match expr {
            ast::Expr::Int(_, _) => Ok((Type::Int, Subst::empty())),
            ast::Expr::Float(_, _) => Ok((Type::Float, Subst::empty())),
            ast::Expr::Str(_, _) => Ok((Type::Str, Subst::empty())),
            ast::Expr::Bool(_, _) => Ok((Type::Bool, Subst::empty())),
            ast::Expr::Tuple(elems, _) if elems.is_empty() => Ok((Type::Unit, Subst::empty())),
            ast::Expr::Tuple(elems, _) => {
                let mut acc = Subst::empty();
                let mut refined_env = env.clone();
                let mut elem_types = Vec::with_capacity(elems.len());
                for e in elems {
                    let (t, s) = self.infer_expr(e, &refined_env)?;
                    acc = s.compose(&acc);
                    refined_env = refined_env.apply_subst(&acc);
                    elem_types.push(t);
                }
                let resolved = elem_types.iter().map(|t| acc.apply(t)).collect();
                Ok((Type::Tuple(resolved), acc))
            }
            // `[e1, e2, ...]` — every element must unify to ONE shared
            // `T`, giving `Array[T]`. `[]` alone leaves `T` a fresh,
            // completely unconstrained var — same "stays free until
            // something else pins it" story as any other never-used
            // type variable elsewhere (an unused function parameter,
            // a bare `None`, ...).
            ast::Expr::ArrayLiteral(elements, span) => {
                let elem_ty = self.fresh();
                let mut acc = Subst::empty();
                let mut refined_env = env.clone();
                for e in elements {
                    let (t, s) = self.infer_expr(e, &refined_env)?;
                    acc = s.compose(&acc);
                    refined_env = refined_env.apply_subst(&acc);
                    let s = unify(&acc.apply(&t), &acc.apply(&elem_ty)).map_err(|e| format!("array element: {e}"))?;
                    acc = s.compose(&acc);
                    refined_env = refined_env.apply_subst(&acc);
                }
                // `lower.rs` needs an EMPTY literal's element type (it
                // has no field to derive one from structurally, unlike
                // codegen for every other array site — see `ir::Expr::
                // EmptyArray`'s doc comment) — recorded here as the
                // still-possibly-unresolved `Var` itself; `resolve_
                // empty_array_elem_types` applies the program's FINAL
                // substitution once `infer_program` completes, exactly
                // like `generic_sites`/`resolve_generic_sites`.
                if elements.is_empty() {
                    self.empty_array_elem_types.insert(*span, elem_ty.clone());
                }
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&elem_ty)]), acc))
            }
            // `arr[i]` — `arr` must be `Array[T]`, `i` must be `Int`;
            // evaluates to `T`. `s[i]` — `s` must be `Str`, `i` must be
            // `Int`; evaluates to `Int` (the raw BYTE value — see
            // `Interpreter::eval`'s `Index` case). Out-of-bounds is a
            // RUNTIME error either way (the index's actual VALUE isn't
            // known at type-checking time), not something caught here.
            // Same "check the resolved shape directly" precedent as
            // `.len()`'s own Str/Array split, for the same reason.
            ast::Expr::Index { base, index, span } => {
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                if acc.apply(&base_ty) == Type::Str {
                    let refined_env = env.apply_subst(&acc);
                    let (index_ty, s) = self.infer_expr(index, &refined_env)?;
                    acc = s.compose(&acc);
                    let s = unify(&acc.apply(&index_ty), &Type::Int).map_err(|e| format!("string index at {span:?}: {e}"))?;
                    acc = s.compose(&acc);
                    return Ok((Type::Int, acc));
                }
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("indexing at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (index_ty, s) = self.infer_expr(index, &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&index_ty), &Type::Int).map_err(|e| format!("array index at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&elem_ty), acc))
            }
            // A bare capitalized name referencing a zero-arity variant
            // (`None`, not `None()`) constructs it directly — mirrors
            // lower.rs's identical `Ident` case.
            ast::Expr::Ident(name, span) if matches!(self.ctx.variant(name), Some((_, p)) if p.is_empty()) => {
                let (enum_name, _) = self.ctx.variant(name).expect("just matched Some above").clone();
                // Empty payload, but the ENUM itself may still be
                // generic (`None` from `Option[T] { Some(T), None }`) —
                // nothing here reveals what `T` is, so it gets its own
                // fresh, still-unconstrained arg, same as an unapplied
                // polymorphic function would.
                let (_, args) = self.instantiate_generic(&enum_name, &[]);
                if !args.is_empty() {
                    self.record_site(SiteKind::Enum, name, &args, *span);
                }
                Ok((Type::Enum(enum_name, args), Subst::empty()))
            }
            // A non-zero-arity variant referenced BARE (not called) is
            // its constructor as a function value — `Circle` alone has
            // type `Function([Float], Enum("Shape"))`, the same type
            // `Circle(1.0)` would eventually produce once applied.
            // Mirrors lower.rs's identical `Ident` case, which
            // eta-expands the SAME bare reference into a real Closure.
            ast::Expr::Ident(name, span) if matches!(self.ctx.variant(name), Some((_, p)) if !p.is_empty()) => {
                let (enum_name, payload) = self.ctx.variant(name).expect("just matched Some above").clone();
                let (payload, args) = self.instantiate_generic(&enum_name, &payload);
                if !args.is_empty() {
                    self.record_site(SiteKind::Enum, name, &args, *span);
                }
                Ok((Type::Function(payload, Box::new(Type::Enum(enum_name, args))), Subst::empty()))
            }
            ast::Expr::Ident(name, span) => {
                let scheme = env
                    .lookup_scheme(name)
                    .cloned()
                    .ok_or_else(|| format!("unbound variable: {name} at {span:?}"))?;
                let (ty, _bounds, mapping) = self.instantiate_with_bounds(&scheme);
                if !scheme.vars.is_empty() {
                    self.record_fn_call_site(name, &mapping, *span);
                } else if self.current_fn.as_deref() == Some(name.as_str()) && !self.current_fn_generics.is_empty() {
                    // A SELF-recursive reference to the generic function
                    // currently being inferred — see `current_fn_generics`'s
                    // doc comment for why `scheme.vars` is always empty
                    // here (Phase 1's monomorphic self/mutual-recursion
                    // placeholder, not `name`'s eventual real scheme) and
                    // why this is still always exactly `name`'s own
                    // generics, unconditionally.
                    let args: Vec<Type> = self.current_fn_generics.iter().map(|(_, ty)| ty.clone()).collect();
                    self.record_site(SiteKind::Function, name, &args, *span);
                }
                Ok((ty, Subst::empty()))
            }
            ast::Expr::Unary { op, expr, .. } => self.infer_unary(op, expr, env),
            ast::Expr::Binary {
                op: ast::BinaryOp::Pipe,
                lhs,
                rhs,
                ..
            } => self.infer_pipe(lhs, rhs, env),
            ast::Expr::Binary {
                op: ast::BinaryOp::Range,
                lhs,
                rhs,
                ..
            } => {
                let (lhs_ty, s) = self.infer_expr(lhs, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&lhs_ty), &Type::Int).map_err(|e| format!("range start: {e}"))?;
                acc = s.compose(&acc);
                let (rhs_ty, s) = self.infer_expr(rhs, env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&rhs_ty), &Type::Int).map_err(|e| format!("range end: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Range, acc))
            }
            ast::Expr::Binary { op, lhs, rhs, .. } => self.infer_binary(op, lhs, rhs, env),
            ast::Expr::Block(block, _) => self.infer_block(block, env),
            ast::Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => self.infer_if(cond, then_branch, else_branch, env),
            // `t.join()` — the type-level counterpart to lower.rs's
            // identical `Call` handling: ANY `expr.join()` call shape
            // is always treated as a task join, matching lowering's
            // own shape-only precedent exactly (so the two passes never
            // disagree about which node a given expression becomes).
            // `base`'s type is unified against `Task[T]` — if `base`
            // genuinely is a `Task`, this resolves `T`; if it's
            // anything else (including a struct with an ordinary,
            // unrelated field literally named `join`), unification
            // fails with a normal, honest type error rather than
            // silently falling through to field-access handling that
            // would only go on to be mislowered anyway.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "join") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let result_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Task".to_string(), vec![result_ty.clone()]))
                    .map_err(|e| format!("`.join()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&result_ty), acc))
            }
            // `tx.send(v)` — same shape-only precedent as `.join()`
            // above. `base` unifies against `Sender[T]`, `v` against
            // that same `T`; always evaluates to `Unit`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "send") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Sender".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.send()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (val_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&val_ty), &acc.apply(&elem_ty))
                    .map_err(|e| format!("`.send()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Unit, acc))
            }
            // `rx.recv()` — `base` unifies against `Receiver[T]`,
            // evaluates to that `T`.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "recv") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Receiver".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.recv()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&elem_ty), acc))
            }
            // `channel[T]()` — a generic-instantiation callee named
            // `channel` with exactly one type argument, called with
            // zero value args (mirrors lower.rs's identical shape
            // check). Evaluates to `(Sender[T], Receiver[T])`.
            ast::Expr::Call { callee, args, span }
                if args.is_empty()
                    && matches!(
                        callee.as_ref(),
                        ast::Expr::GenericInst { callee, args, .. }
                            if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Ident(name, _) if name == "channel")
                    ) =>
            {
                let ast::Expr::GenericInst { args: type_args, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let elem_ty = ast_type_to_type(&type_args[0], &self.ctx, &[])
                    .map_err(|e| format!("`channel[..]` type argument at {span:?}: {e}"))?;
                Ok((
                    Type::Tuple(vec![
                        Type::Struct("Sender".to_string(), vec![elem_ty.clone()]),
                        Type::Struct("Receiver".to_string(), vec![elem_ty]),
                    ]),
                    Subst::empty(),
                ))
            }
            // `ref(v)` — a bare-Ident callee named `ref`, called with
            // exactly one value arg (mirrors lower.rs's identical
            // shape check, and the same "checked BEFORE the general
            // variant-construction fallback" precedent `channel[T]()`
            // already established). Evaluates to `Ref[T]`, `T` inferred
            // directly from `v` — no explicit type argument needed,
            // unlike `channel[T]()`, since there's already a real value
            // to infer from.
            ast::Expr::Call { callee, args, .. }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Ident(name, _) if name == "ref") =>
            {
                let (value_ty, acc) = self.infer_expr(&args[0], env)?;
                Ok((Type::Struct("Ref".to_string(), vec![value_ty]), acc))
            }
            // `r.get()` — `r` must be `Ref[T]` for some `T`; evaluates
            // to `T`.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "get") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Ref".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.get()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&elem_ty), acc))
            }
            // `r.set(v)` — `r` must be `Ref[T]`, `v` must be that SAME
            // `T`; evaluates to `Unit` (a genuine imperative mutation,
            // NOT the "returns a new value" convention every other
            // `.set()`/mutating-looking method uses — see `ir::Expr::
            // RefSet`'s doc comment).
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "set") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Ref".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.set()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (value_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&value_ty), &acc.apply(&elem_ty))
                    .map_err(|e| format!("`.set()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Unit, acc))
            }
            // `arr.len()` / `s.len()` — `arr`/`s` must be `Array[T]` for
            // SOME `T`, or `Str`; evaluates to `Int` either way. Same
            // "check the ALREADY-RESOLVED shape directly before falling
            // to the default unify" pattern `for x in arr` uses to tell
            // Array from Range apart — checked here for the identical
            // reason: blind trial-unifying against `Array[fresh]` first
            // would trivially succeed for a still-unresolved type
            // variable (e.g. an unannotated generic parameter), wrongly
            // ruling out the Str case for it.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "len") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                if acc.apply(&base_ty) == Type::Str {
                    return Ok((Type::Int, acc));
                }
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty]))
                    .map_err(|e| format!("`.len()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Int, acc))
            }
            // `s.concat(other)` — both `s` and `other` must be `Str`;
            // evaluates to `Str`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "concat") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.concat()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (other_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&other_ty), &Type::Str)
                    .map_err(|e| format!("`.concat()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Str, acc))
            }
            // `s.runes()` — `s` must be `Str`; evaluates to
            // `Array[Int]` (one Unicode codepoint per element — see
            // `ir::Expr::StrRunes`'s doc comment for why this exists
            // separately from byte-indexing `s[i]`).
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "runes") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.runes()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![Type::Int]), acc))
            }
            // `s.as_cstr()` — `s` must be `Str`; evaluates to `CStr`, a
            // type an ordinary `Str` value can never unify with (see
            // `Type::CStr`'s doc comment) — the explicit call is what
            // makes a value usable as an extern function's `CStr`
            // argument/return, matching DESIGN.md's "no implicit
            // string/allocation coercion at the boundary."
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "as_cstr") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.as_cstr()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::CStr, acc))
            }
            // `s.trim()` — `s` must be `Str`; evaluates to `Str`.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "trim") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.trim()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Str, acc))
            }
            // `s.split(sep)` — `s` and `sep` must both be `Str`;
            // evaluates to `Array[Str]`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "split") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.split()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (sep_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&sep_ty), &Type::Str)
                    .map_err(|e| format!("`.split()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![Type::Str]), acc))
            }
            // `s.to_upper()` / `s.to_lower()` — `s` must be `Str`;
            // evaluate to `Str`.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "to_upper") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.to_upper()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Str, acc))
            }
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "to_lower") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.to_lower()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Str, acc))
            }
            // `s.contains(needle)` / `s.starts_with(prefix)` /
            // `s.ends_with(suffix)` — both operands must be `Str`;
            // evaluate to `Bool`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "contains") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.contains()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (needle_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&needle_ty), &Type::Str)
                    .map_err(|e| format!("`.contains()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Bool, acc))
            }
            ast::Expr::Call { callee, args, span }
                if args.len() == 1
                    && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "starts_with") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.starts_with()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (prefix_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&prefix_ty), &Type::Str)
                    .map_err(|e| format!("`.starts_with()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Bool, acc))
            }
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "ends_with") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.ends_with()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (suffix_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&suffix_ty), &Type::Str)
                    .map_err(|e| format!("`.ends_with()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Bool, acc))
            }
            // `s.replace(from, to)` — `s`, `from`, `to` must all be
            // `Str`; evaluates to `Str`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "replace") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&base_ty), &Type::Str).map_err(|e| format!("`.replace()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (from_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&from_ty), &Type::Str)
                    .map_err(|e| format!("`.replace()` first argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (to_ty, s) = self.infer_expr(&args[1], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&to_ty), &Type::Str)
                    .map_err(|e| format!("`.replace()` second argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Str, acc))
            }
            // `x.to_string()` — `x` should resolve to `Int`, `Float`,
            // `Bool`, or `Str` (checked directly against the
            // ALREADY-RESOLVED type, not via `unify`, since there's no
            // single target type to unify against — see `ir::Expr::
            // ToString`'s doc comment for why this is scoped to just
            // these four types for now). Evaluates to `Str`.
            //
            // Deliberately PERMISSIVE when `base`'s type is STILL an
            // unresolved `Var` at this point (e.g. a closure parameter
            // whose type is only pinned by a LATER unification, as in
            // `[1,2,3].map(|x| x.to_string())` — inside the closure
            // body, `x` isn't unified against the array's element type
            // until after the closure's own inference finishes) —
            // erroring here would be a false rejection of valid code.
            // Only a CONCRETE, already-known-wrong type is rejected at
            // compile time; an unresolved var falls through to the
            // interpreter's own clear runtime error if it turns out to
            // be something unsupported (same "compile-time check when
            // possible, runtime fallback otherwise" split `Index`'s
            // out-of-bounds already uses).
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "to_string") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let acc = s;
                let resolved = acc.apply(&base_ty);
                let is_concrete_and_unsupported = !matches!(
                    resolved,
                    Type::Int | Type::Float | Type::Bool | Type::Str | Type::Var(_)
                );
                if is_concrete_and_unsupported {
                    return Err(format!(
                        "`.to_string()` at {span:?}: not yet supported for {resolved:?} (only Int/Float/Bool/Str)"
                    ));
                }
                Ok((Type::Str, acc))
            }
            // `arr.push(v)` — `arr` must be `Array[T]`, `v` must be
            // that SAME `T`; evaluates to a (new) `Array[T]`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "push") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.push()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (val_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&val_ty), &acc.apply(&elem_ty))
                    .map_err(|e| format!("`.push()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&elem_ty)]), acc))
            }
            // `arr.pop()` — `arr` must be `Array[T]`; evaluates to a
            // (new) `Array[T]`. Whether the array is actually non-empty
            // isn't checked here — same "runtime-checked, not compile-
            // time" split as `Index`'s out-of-bounds.
            ast::Expr::Call { callee, args, span }
                if args.is_empty() && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "pop") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.pop()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&elem_ty)]), acc))
            }
            // `arr.set(i, v)` — `arr` must be `Array[T]`, `i` must be
            // `Int`, `v` must be that SAME `T`; evaluates to a (new)
            // `Array[T]`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "set") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.set()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (idx_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&idx_ty), &Type::Int).map_err(|e| format!("`.set()` index at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (val_ty, s) = self.infer_expr(&args[1], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&val_ty), &acc.apply(&elem_ty))
                    .map_err(|e| format!("`.set()` argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&elem_ty)]), acc))
            }
            // `arr.remove(i)` — `arr` must be `Array[T]`, `i` must be
            // `Int`; evaluates to a (new) `Array[T]`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "remove") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.remove()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (idx_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&idx_ty), &Type::Int).map_err(|e| format!("`.remove()` index at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&elem_ty)]), acc))
            }
            // `arr.map(f)` — `arr` must be `Array[T]`, `f` must be a
            // ONE-argument function from `T` to some `U`; evaluates to
            // `Array[U]`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "map") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.map()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (f_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let out_ty = self.fresh();
                let s = unify(
                    &acc.apply(&f_ty),
                    &Type::Function(vec![acc.apply(&elem_ty)], Box::new(out_ty.clone())),
                )
                .map_err(|e| format!("`.map()` function argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&out_ty)]), acc))
            }
            // `arr.filter(f)` — `arr` must be `Array[T]`, `f` must be a
            // ONE-argument function from `T` to `Bool`; evaluates to
            // `Array[T]` (unchanged element type).
            ast::Expr::Call { callee, args, span }
                if args.len() == 1 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "filter") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.filter()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (f_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(
                    &acc.apply(&f_ty),
                    &Type::Function(vec![acc.apply(&elem_ty)], Box::new(Type::Bool)),
                )
                .map_err(|e| format!("`.filter()` function argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Struct("Array".to_string(), vec![acc.apply(&elem_ty)]), acc))
            }
            // `arr.fold(init, f)` — `arr` must be `Array[T]`, `f` must
            // be a TWO-argument function `(U, T) -> U` where `U` is
            // `init`'s type; evaluates to `U`.
            ast::Expr::Call { callee, args, span }
                if args.len() == 2 && matches!(callee.as_ref(), ast::Expr::Field { name, .. } if name == "fold") =>
            {
                let ast::Expr::Field { base, .. } = callee.as_ref() else {
                    unreachable!("just matched this shape above");
                };
                let (base_ty, s) = self.infer_expr(base, env)?;
                let mut acc = s;
                let elem_ty = self.fresh();
                let s = unify(&acc.apply(&base_ty), &Type::Struct("Array".to_string(), vec![elem_ty.clone()]))
                    .map_err(|e| format!("`.fold()` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (init_ty, s) = self.infer_expr(&args[0], &refined_env)?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);
                let (f_ty, s) = self.infer_expr(&args[1], &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(
                    &acc.apply(&f_ty),
                    &Type::Function(
                        vec![acc.apply(&init_ty), acc.apply(&elem_ty)],
                        Box::new(acc.apply(&init_ty)),
                    ),
                )
                .map_err(|e| format!("`.fold()` function argument at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&init_ty), acc))
            }
            ast::Expr::Call { callee, args, span } => {
                // `Circle(1.0)` / `Shape.Circle(1.0)` constructs a
                // variant if the callee names one, checked BEFORE
                // falling back to an ordinary call — the type-level
                // counterpart to lower.rs's identical `Call` handling.
                // The qualifier before `.` (`Shape`) is never validated
                // against the variant's real owning enum, matching that
                // same established precedent (tags are looked up by
                // name alone).
                // `sqrt(2.0)` where `sqrt` names a declared `extern
                // "C"` function — enforce `unsafe`-gating right here,
                // before falling through to the ordinary call-inference
                // path below (which, once this check passes, handles an
                // extern call exactly like any other named call: its
                // signature is already sitting in `global_env`, seeded
                // by `infer_program`'s own extern pre-declaration pass
                // — see that function's doc comment).
                if let ast::Expr::Ident(name, _) = callee.as_ref() {
                    if let Some((param_types, _)) = self.ctx.extern_fn(name).cloned() {
                        if !self.in_unsafe {
                            return Err(format!(
                                "calling extern function {name:?} requires being inside an unsafe block, at {span:?}"
                            ));
                        }
                        // A callback-typed argument must be a bare
                        // reference to a genuine TOP-LEVEL function —
                        // see `top_level_fns`'s doc comment for why
                        // this is checked by NAME-SET membership rather
                        // than a real capture analysis: a top-level
                        // function's `Interpreter::call` always builds
                        // a completely fresh environment from just its
                        // own params, so it's non-capturing BY
                        // CONSTRUCTION — no analysis needed, unlike a
                        // closure literal (rejected outright, since
                        // proving IT doesn't capture anything would
                        // need real free-variable analysis this v1
                        // doesn't do) or a local variable merely NAMING
                        // a function (rejected too — shadowing a
                        // `top_level_fns` name with an unrelated local
                        // binding of the same name is a known, narrow
                        // gap this check doesn't see through).
                        for (arg, param_ty) in args.iter().zip(&param_types) {
                            if !matches!(param_ty, Type::Function(..)) {
                                continue;
                            }
                            let is_bare_top_level_fn =
                                matches!(arg, ast::Expr::Ident(arg_name, _) if self.top_level_fns.contains(arg_name));
                            if !is_bare_top_level_fn {
                                return Err(format!(
                                    "extern function {name:?}: a callback argument must be a bare reference \
                                     to a top-level function, not a closure or other expression, at {:?}",
                                    arg.span()
                                ));
                            }
                        }
                    }
                }
                let variant_tag = match callee.as_ref() {
                    ast::Expr::Ident(name, _) => Some(name.as_str()),
                    ast::Expr::Field { name, .. } => Some(name.as_str()),
                    _ => None,
                };
                if let Some(tag) = variant_tag {
                    if let Some((enum_name, payload_types)) = self.ctx.variant(tag).cloned() {
                        if args.len() != payload_types.len() {
                            return Err(format!(
                                "variant {tag:?} expects {} field(s), found {} at {span:?}",
                                payload_types.len(),
                                args.len()
                            ));
                        }
                        let (payload_types, enum_args) = self.instantiate_generic(&enum_name, &payload_types);
                        if !enum_args.is_empty() {
                            self.record_site(SiteKind::Enum, tag, &enum_args, *span);
                        }
                        let mut acc = Subst::empty();
                        let mut refined_env = env.clone();
                        for (arg, expected_ty) in args.iter().zip(payload_types.iter()) {
                            let (t, s) = self.infer_expr(arg, &refined_env)?;
                            acc = s.compose(&acc);
                            refined_env = refined_env.apply_subst(&acc);
                            let s = unify(&acc.apply(&t), &acc.apply(expected_ty))
                                .map_err(|e| format!("variant {tag:?} argument: {e}"))?;
                            acc = s.compose(&acc);
                            refined_env = refined_env.apply_subst(&acc);
                        }
                        let enum_args: Vec<Type> = enum_args.iter().map(|a| acc.apply(a)).collect();
                        self.check_generic_bounds(&enum_name, &enum_args, *span)?;
                        return Ok((Type::Enum(enum_name, enum_args), acc));
                    }
                }
                // Calling a BOUNDED generic function by its bare name
                // directly (`identity_num(5)`, not through an alias —
                // see `instantiate`'s doc comment on that narrow, known
                // gap) — instantiate with bound-tracking instead of the
                // ordinary path, so `infer_call_with_callee` can check
                // each bound against the FINAL, fully-unified argument
                // types. Checked here rather than inside `infer_call`
                // itself so the ordinary (unbounded, the overwhelming
                // common case) call path never pays for a scheme
                // lookup it doesn't need.
                if let ast::Expr::Ident(name, ident_span) = callee.as_ref() {
                    if let Some(scheme) = env.lookup_scheme(name) {
                        if !scheme.bounds.is_empty() {
                            let scheme = scheme.clone();
                            let (callee_ty, pending_bounds, mapping) = self.instantiate_with_bounds(&scheme);
                            if !scheme.vars.is_empty() {
                                self.record_fn_call_site(name, &mapping, *ident_span);
                            }
                            let arg_refs: Vec<&ast::Expr> = args.iter().collect();
                            return self.infer_call_with_callee(
                                callee_ty,
                                Subst::empty(),
                                &arg_refs,
                                env,
                                pending_bounds,
                            );
                        }
                    }
                }
                let arg_refs: Vec<&ast::Expr> = args.iter().collect();
                self.infer_call(callee, &arg_refs, env)
            }
            ast::Expr::StructLiteral {
                path,
                fields,
                spread,
                span,
            } => self.infer_struct_literal(path, fields, spread, *span, env),
            ast::Expr::Match { scrutinee, arms, .. } => self.infer_match(scrutinee, arms, env),
            ast::Expr::Select { arms, span } => self.infer_select(arms, *span, env),
            ast::Expr::Closure { params, body, span } => self.infer_closure(params, body, env, *span),
            // `unsafe` doesn't change the block's TYPE (its type is
            // whatever the block's own type is, same as a plain block)
            // — but unlike before extern functions existed, it's no
            // longer a pure no-op: entering it is what makes an extern
            // call inside legal (see `in_unsafe`'s doc comment and the
            // `Call` case below). Saves/restores rather than assuming
            // `false` on the way out, so `unsafe { unsafe { .. } }`
            // (silly but not rejected) and an `unsafe` block nested
            // inside a closure defined outside one both behave
            // correctly.
            ast::Expr::Unsafe(block, _) => {
                let was_unsafe = self.in_unsafe;
                self.in_unsafe = true;
                let result = self.infer_block(block, env);
                self.in_unsafe = was_unsafe;
                result
            }
            // `spawn { block }` — DESIGN.md's heap-ownership-across-
            // tasks blocker is now Decided (deep-copy on crossing, see
            // `plum-interp`), so this just infers the block's type and
            // wraps it as `Task[T]`. `Task` is a BUILTIN pseudo-generic
            // type, not a real `TypeContext`-registered struct: it has
            // no declared fields (`.join()` is special-cased above, not
            // ordinary field access), so `Type::Struct("Task", vec![T])`
            // is used purely for its structural unify/subst/occurs
            // behavior (see unify.rs — nominal in name, structural in
            // arguments), never looked up in `self.ctx`.
            ast::Expr::Spawn(block, _) => {
                let (block_ty, acc) = self.infer_block(block, env)?;
                Ok((Type::Struct("Task".to_string(), vec![block_ty]), acc))
            }
            ast::Expr::For {
                pattern,
                iter,
                body,
                span,
            } => self.infer_for(pattern, iter, body, *span, env),
            // `p.x` — reads a single declared field straight off a
            // struct value. Requires `base`'s type to ALREADY resolve
            // to a KNOWN, concrete struct at this point (no row/
            // structural typing exists — two structs sharing a field
            // name are still unrelated types, matching every other
            // nominal-typing choice in this crate), so an
            // as-yet-unresolved type variable is a real, reported
            // error, not something deferred. On success, records
            // `span -> struct name` into `field_owners` — see this
            // struct's field doc comment for why lowering needs this.
            ast::Expr::Field { base, name, span } => {
                let (base_ty, s) = self.infer_expr(base, env)?;
                let acc = s;
                let resolved_base_ty = acc.apply(&base_ty);
                let Type::Struct(struct_name, struct_args) = &resolved_base_ty else {
                    return Err(format!(
                        "field access `.{name}` at {span:?} requires a struct value with a \
                         statically known type, found {resolved_base_ty:?}"
                    ));
                };
                let declared_fields = self
                    .ctx
                    .struct_fields(struct_name)
                    .ok_or_else(|| format!("unknown struct type {struct_name:?} at {span:?}"))?;
                let field_ty = declared_fields
                    .iter()
                    .find(|(field_name, _)| field_name == name)
                    .map(|(_, ty)| ty.clone())
                    .ok_or_else(|| format!("struct {struct_name:?} has no field named {name:?} (at {span:?})"))?;
                // The declared field type may mention the struct's OWN
                // generic parameters (`Type::Param`) — `base`'s type
                // already carries the CONCRETE argument for each one
                // (`struct_args`, in the same declared order), so this
                // substitutes those in directly rather than minting
                // fresh vars (there's nothing fresh to infer here: the
                // struct is already fully known).
                let param_names = self.ctx.generic_params(struct_name).unwrap_or(&[]).to_vec();
                let mapping: HashMap<String, Type> =
                    param_names.into_iter().zip(struct_args.iter().cloned()).collect();
                let field_ty = subst_params(&field_ty, &mapping);
                self.field_owners.insert(*span, struct_name.clone());
                // A field access on a GENERIC struct instance needs its
                // own site recorded too, even though it constructs
                // nothing — `lower.rs`'s field-access lowering resolves
                // the struct name to match against purely through
                // `field_owners` (a totally separate mechanism from the
                // `Ctor`/pattern arms `instantiate_generic`'s other call
                // sites feed), so without this, `plum_ir::monomorphize`
                // would have no way to know a `p.x` access needs its
                // generated `Match`'s tag MANGLED too, and would emit an
                // unmangled tag that never has a matching `tag_fields`
                // entry.
                if !struct_args.is_empty() {
                    self.record_site(SiteKind::Struct, struct_name, struct_args, *span);
                }
                Ok((acc.apply(&field_ty), acc))
            }
            other => Err(format!(
                "type inference not yet implemented for this expression form at {:?}",
                other.span()
            )),
        }
    }

    // `x |> rhs` type-checks by desugaring EXACTLY the way lower.rs's
    // `lower_pipe` does at the IR level — `x |> f` is a call to `f`
    // with `x`; `x |> f(a, b)` is a call to `f` with `(a, b, x)`, the
    // piped value appended as the LAST argument. Kept as its own
    // function so the two shapes share `infer_call` rather than
    // duplicating its unification logic.
    fn infer_pipe(&mut self, lhs: &ast::Expr, rhs: &ast::Expr, env: &TypeEnv) -> Result<(Type, Subst), String> {
        match rhs {
            ast::Expr::Call { callee, args, .. } => {
                let mut all_args: Vec<&ast::Expr> = args.iter().collect();
                all_args.push(lhs);
                self.infer_call(callee, &all_args, env)
            }
            other => self.infer_call(other, &[lhs], env),
        }
    }

    fn infer_call(&mut self, callee: &ast::Expr, args: &[&ast::Expr], env: &TypeEnv) -> Result<(Type, Subst), String> {
        let (callee_ty, s) = self.infer_expr(callee, env)?;
        self.infer_call_with_callee(callee_ty, s, args, env, Vec::new())
    }

    /// The shared core of "call this function type with these
    /// arguments" — `infer_call` is the ordinary case (an already-
    /// inferred callee, nothing to bound-check); the `Call` arm's
    /// bounded-generic special case (see its own comment) instead
    /// instantiates the callee itself via `instantiate_with_bounds`
    /// and passes the resulting `pending_bounds` through here, so
    /// they're checked with the FINAL, fully-unified argument types —
    /// exactly the point `instantiate_with_bounds`'s doc comment says
    /// they only become checkable.
    fn infer_call_with_callee(
        &mut self,
        callee_ty: Type,
        callee_subst: Subst,
        args: &[&ast::Expr],
        env: &TypeEnv,
        pending_bounds: Vec<(TypeVarId, Vec<String>)>,
    ) -> Result<(Type, Subst), String> {
        let mut acc = callee_subst;
        let mut refined_env = env.apply_subst(&acc);
        let mut arg_types = Vec::with_capacity(args.len());
        for arg in args {
            let (t, s) = self.infer_expr(arg, &refined_env)?;
            acc = s.compose(&acc);
            refined_env = refined_env.apply_subst(&acc);
            arg_types.push(acc.apply(&t));
        }
        let ret_var = self.fresh();
        let expected_fn_ty = Type::Function(arg_types, Box::new(ret_var.clone()));
        let s = unify(&acc.apply(&callee_ty), &expected_fn_ty).map_err(|e| format!("call: {e}"))?;
        acc = s.compose(&acc);
        for (var_id, bounds) in pending_bounds {
            let resolved = acc.apply(&Type::Var(var_id));
            // Still unresolved (nothing pinned this specific generic
            // down, even after unifying every argument) — nothing
            // concrete to check yet, same skip `check_generic_bounds`
            // already applies for struct/enum construction.
            if matches!(resolved, Type::Var(_)) {
                continue;
            }
            for bound in &bounds {
                if !satisfies_bound(&resolved, bound) {
                    return Err(format!(
                        "generic parameter requires `{bound}`, but {resolved:?} does not satisfy it"
                    ));
                }
            }
        }
        Ok((acc.apply(&ret_var), acc))
    }

    fn infer_struct_literal(
        &mut self,
        path: &[String],
        fields: &[ast::FieldInit],
        spread: &Option<Box<ast::Expr>>,
        span: plum_syntax::span::Span,
        env: &TypeEnv,
    ) -> Result<(Type, Subst), String> {
        let tag = path.last().cloned().expect("a path always has at least one segment");
        let declared_fields = self
            .ctx
            .struct_fields(&tag)
            .ok_or_else(|| format!("unknown struct type {tag:?} at {span:?}"))?
            .to_vec();
        let (declared_field_names, declared_field_types): (Vec<String>, Vec<Type>) =
            declared_fields.into_iter().unzip();
        // ONE shared instantiation for the whole literal: every field's
        // template type (and the spread check below) needs the SAME
        // fresh var per generic parameter — `struct Pair[T] { first: T,
        // second: T }` requires both fields AND the spread source to
        // agree on one `T`, not each pick their own.
        let (declared_field_types, struct_args) = self.instantiate_generic(&tag, &declared_field_types);
        if !struct_args.is_empty() {
            self.record_site(SiteKind::Struct, &tag, &struct_args, span);
        }
        let declared_fields: Vec<(String, Type)> =
            declared_field_names.into_iter().zip(declared_field_types).collect();

        let mut by_name: HashMap<&str, &ast::Expr> = HashMap::new();
        for f in fields {
            if by_name.insert(f.name.as_str(), &f.value).is_some() {
                return Err(format!("field {:?} specified more than once at {:?}", f.name, f.span));
            }
        }

        let mut acc = Subst::empty();

        // `..expr`: `expr` must itself be a `tag`, so any field NOT
        // given explicitly is guaranteed (by that unification alone)
        // to already have the right type — no per-field unification
        // needed for those, unlike the explicit fields below.
        if let Some(spread_expr) = spread {
            let (spread_ty, s) = self.infer_expr(spread_expr, env)?;
            acc = s.compose(&acc);
            let s = unify(&acc.apply(&spread_ty), &Type::Struct(tag.clone(), struct_args.clone()))
                .map_err(|e| format!("struct update `..` for {tag:?}: {e}"))?;
            acc = s.compose(&acc);
        }

        for (declared_name, declared_ty) in &declared_fields {
            let Some(value_expr) = by_name.remove(declared_name.as_str()) else {
                if spread.is_some() {
                    continue;
                }
                return Err(format!("missing field {declared_name:?} for struct {tag:?} at {span:?}"));
            };
            let (val_ty, s) = self.infer_expr(value_expr, env)?;
            acc = s.compose(&acc);
            let s = unify(&acc.apply(&val_ty), &acc.apply(declared_ty))
                .map_err(|e| format!("field {declared_name:?} of struct {tag:?}: {e}"))?;
            acc = s.compose(&acc);
        }
        if let Some((extra_name, _)) = by_name.into_iter().next() {
            return Err(format!("struct {tag:?} has no field named {extra_name:?} (at {span:?})"));
        }

        let struct_args: Vec<Type> = struct_args.iter().map(|a| acc.apply(a)).collect();
        self.check_generic_bounds(&tag, &struct_args, span)?;
        Ok((Type::Struct(tag, struct_args), acc))
    }

    // Only the simple `Path(bindings...)` shape is supported, same
    // Recursively binds `pattern` against `scrutinee_ty`, unifying as
    // needed and returning `env` extended with every name the pattern
    // introduces — including transitively, for nested tuple/struct/
    // variant patterns. The type-level counterpart to lower.rs's
    // `lower_tag_pattern`/`classify_subpattern`/`wrap_nested_destructures`
    // — unlike lowering, no synthetic names are needed here: type
    // inference doesn't need a runtime identifier for an intermediate
    // destructure, just to accumulate bindings into `env` directly.
    //
    // A `Variant` pattern matches either a real enum variant OR a whole
    // struct (lowering erases that distinction — both become a
    // positional `Ctor`/`Match` by tag; see lower.rs's `lower_tag_pattern`
    // doc comment), so `Point(x, y)` here falls back to `ctx.struct_fields`
    // exactly like `infer_match` always has.
    fn bind_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_ty: &Type,
        env: TypeEnv,
        acc: &mut Subst,
    ) -> Result<TypeEnv, String> {
        match pattern {
            ast::Pattern::Ident(name, _) => Ok(env.extend(name.clone(), acc.apply(scrutinee_ty))),
            ast::Pattern::Wildcard(_) => Ok(env),
            // Literal patterns bind no names (same as movecheck.rs's
            // own treatment) — just unify the scrutinee against the
            // literal's own type. See lower.rs's `lower_literal_match`
            // for why these can only appear as non-last arms of a
            // match that ends with a required catch-all.
            ast::Pattern::Int(_, span) => {
                let s = unify(&acc.apply(scrutinee_ty), &Type::Int).map_err(|e| format!("pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                Ok(env)
            }
            ast::Pattern::Float(_, span) => {
                let s = unify(&acc.apply(scrutinee_ty), &Type::Float).map_err(|e| format!("pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                Ok(env)
            }
            ast::Pattern::Bool(_, span) => {
                let s = unify(&acc.apply(scrutinee_ty), &Type::Bool).map_err(|e| format!("pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                Ok(env)
            }
            ast::Pattern::Str(_, span) => {
                let s = unify(&acc.apply(scrutinee_ty), &Type::Str).map_err(|e| format!("pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                Ok(env)
            }
            // `()` — the Unit pattern (an empty tuple pattern, same
            // grammar production as a real tuple pattern with 0
            // elements — see `Param`'s own `()` parsing). Binds no
            // names, just unifies the scrutinee against `Unit`.
            ast::Pattern::Tuple(elems, span) if elems.is_empty() => {
                let s = unify(&acc.apply(scrutinee_ty), &Type::Unit).map_err(|e| format!("pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                Ok(env)
            }
            ast::Pattern::Tuple(elems, span) => {
                let fresh_vars: Vec<Type> = elems.iter().map(|_| self.fresh()).collect();
                let s = unify(&acc.apply(scrutinee_ty), &Type::Tuple(fresh_vars.clone()))
                    .map_err(|e| format!("tuple pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                let mut env = env;
                for (elem_pat, var_ty) in elems.iter().zip(fresh_vars.iter()) {
                    env = self.bind_pattern(elem_pat, var_ty, env, acc)?;
                    env = env.apply_subst(acc);
                }
                Ok(env)
            }
            // `has_rest` (`..`) means "I don't care about the fields I
            // didn't mention" — it does NOT relax the unknown-field
            // check: naming a field the struct doesn't have is always
            // an error.
            ast::Pattern::Struct {
                path,
                fields,
                has_rest,
                span,
            } => {
                let tag = path.last().cloned().expect("a path always has at least one segment");
                let declared_fields = self
                    .ctx
                    .struct_fields(&tag)
                    .ok_or_else(|| format!("unknown struct type {tag:?} at {span:?}"))?
                    .to_vec();
                let (declared_field_names, declared_field_types): (Vec<String>, Vec<Type>) =
                    declared_fields.into_iter().unzip();
                let (declared_field_types, struct_args) = self.instantiate_generic(&tag, &declared_field_types);
                if !struct_args.is_empty() {
                    self.record_site(SiteKind::Struct, &tag, &struct_args, *span);
                }
                let declared_fields: Vec<(String, Type)> =
                    declared_field_names.into_iter().zip(declared_field_types).collect();
                let s = unify(&acc.apply(scrutinee_ty), &Type::Struct(tag.clone(), struct_args))
                    .map_err(|e| format!("struct pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);

                let mut by_name: HashMap<&str, &ast::Pattern> = HashMap::new();
                for f in fields {
                    if by_name.insert(f.name.as_str(), &f.pattern).is_some() {
                        return Err(format!("field {:?} specified more than once at {:?}", f.name, f.span));
                    }
                }
                let mut env = env;
                for (declared_name, declared_ty) in &declared_fields {
                    match by_name.remove(declared_name.as_str()) {
                        Some(sub_pattern) => {
                            env = self.bind_pattern(sub_pattern, declared_ty, env, acc)?;
                            env = env.apply_subst(acc);
                        }
                        None if *has_rest => {}
                        None => {
                            return Err(format!(
                                "missing field {declared_name:?} for struct {tag:?} pattern \
                                 at {span:?} (add `..` to ignore it)"
                            ));
                        }
                    }
                }
                if let Some((extra_name, _)) = by_name.into_iter().next() {
                    return Err(format!("struct {tag:?} has no field named {extra_name:?} (at {span:?})"));
                }
                Ok(env)
            }
            ast::Pattern::Variant { path, args, span } => {
                let tag = path.last().cloned().expect("a path always has at least one segment");
                let (owning_ty, payload_types) = match self.ctx.variant(&tag) {
                    Some((enum_name, payload_types)) => {
                        let enum_name = enum_name.clone();
                        let (payload_types, args) = self.instantiate_generic(&enum_name, &payload_types.clone());
                        if !args.is_empty() {
                            self.record_site(SiteKind::Enum, &tag, &args, *span);
                        }
                        (Type::Enum(enum_name, args), payload_types)
                    }
                    None => match self.ctx.struct_fields(&tag) {
                        Some(fields) => {
                            let field_types: Vec<Type> = fields.iter().map(|(_, ty)| ty.clone()).collect();
                            let (payload_types, args) = self.instantiate_generic(&tag, &field_types);
                            if !args.is_empty() {
                                self.record_site(SiteKind::Struct, &tag, &args, *span);
                            }
                            (Type::Struct(tag.clone(), args), payload_types)
                        }
                        None => return Err(format!("unknown variant {tag:?} at {span:?}")),
                    },
                };
                let s = unify(&acc.apply(scrutinee_ty), &owning_ty).map_err(|e| format!("pattern at {span:?}: {e}"))?;
                *acc = s.compose(acc);
                if args.len() != payload_types.len() {
                    return Err(format!(
                        "pattern at {span:?} expects {} field(s), found {} binding(s)",
                        payload_types.len(),
                        args.len()
                    ));
                }
                let mut env = env;
                for (arg_pat, payload_ty) in args.iter().zip(payload_types.iter()) {
                    env = self.bind_pattern(arg_pat, payload_ty, env, acc)?;
                    env = env.apply_subst(acc);
                }
                Ok(env)
            }
            // `Pattern::Or` is DELIBERATELY not handled here — see
            // `infer_or_pattern`'s own doc comment for why it's only
            // ever valid as a WHOLE top-level match arm (checked by
            // `infer_match` before it ever calls `bind_pattern`), never
            // nested inside a Variant/Tuple/Struct sub-pattern. Falling
            // through to the ordinary "not yet implemented" error here
            // is exactly right for that nested case.
            other => Err(format!(
                "type inference not yet implemented for this pattern shape at {:?}",
                other.span()
            )),
        }
    }

    // `A(v) | B(v) => ..` — mirrors lower.rs's `lower_match` Or-pattern
    // handling exactly, including its scope restriction: valid ONLY as
    // a WHOLE top-level match arm pattern, never nested inside another
    // pattern (lowering's `classify_subpattern` has no case for `Or`
    // at all, only `bind_pattern`'s TOP-level dispatch does — see
    // `infer_match`, the only caller). Every alternative must bind the
    // SAME names in the SAME order, none of them may contain a nested
    // Variant/Tuple/Struct sub-pattern (lowering's own synthetic-
    // placeholder destructuring mechanism doesn't compose across
    // multiple arms sharing one body), and same-named bindings across
    // alternatives must unify to one consistent type (e.g. `A(Int) |
    // B(Float)` binding `v` in both is a real type error: the shared
    // body can't be typed against two different types for the same
    // name).
    fn infer_or_pattern(
        &mut self,
        alts: &[ast::Pattern],
        span: plum_syntax::span::Span,
        scrutinee_ty: &Type,
        env: &TypeEnv,
        acc: &mut Subst,
    ) -> Result<TypeEnv, String> {
        if alts.is_empty() {
            return Err(format!("or-pattern has no alternatives at {span:?}"));
        }
        if alts.iter().any(pattern_has_nested_tag_subpattern) {
            return Err(format!(
                "type inference not yet implemented for a nested pattern inside an \
                 or-pattern alternative at {span:?}"
            ));
        }
        let before_len = env.0.len();
        let mut first_new: Option<Vec<(String, Type)>> = None;
        let mut result_env = env.clone();
        for alt in alts {
            let alt_env = self.bind_pattern(alt, scrutinee_ty, env.clone(), acc)?;
            let new_bindings: Vec<(String, Type)> =
                alt_env.0[before_len..].iter().map(|(name, scheme)| (name.clone(), scheme.ty.clone())).collect();
            match &first_new {
                None => first_new = Some(new_bindings),
                Some(first) => {
                    let names_match = first.len() == new_bindings.len()
                        && first.iter().zip(new_bindings.iter()).all(|((n1, _), (n2, _))| n1 == n2);
                    if !names_match {
                        return Err(format!(
                            "every alternative of an or-pattern must bind the same names in \
                             the same order at {span:?}"
                        ));
                    }
                    for ((_, t1), (_, t2)) in first.iter().zip(new_bindings.iter()) {
                        let s = unify(&acc.apply(t1), &acc.apply(t2)).map_err(|e| {
                            format!("or-pattern alternatives bind inconsistent types at {span:?}: {e}")
                        })?;
                        *acc = s.compose(acc);
                    }
                }
            }
            result_env = alt_env;
        }
        Ok(result_env.apply_subst(acc))
    }

    fn infer_match(&mut self, scrutinee: &ast::Expr, arms: &[ast::MatchArm], env: &TypeEnv) -> Result<(Type, Subst), String> {
        let (scrutinee_ty, s) = self.infer_expr(scrutinee, env)?;
        let mut acc = s;
        let mut result_ty: Option<Type> = None;

        // Two shapes lower.rs's `lower_match` now accepts a bare
        // identifier/wildcard for — see `lower_catchall_only_match`/
        // `lower_literal_match`'s own doc comments there for the full
        // reasoning; this mirrors those two functions' detection
        // exactly, and for the same reason the guard-nesting check
        // above exists: type-checking must reject anything that would
        // otherwise fail at the lowering gate right after.
        let is_single_catchall =
            arms.len() == 1 && is_catchall_pattern(&arms[0].pattern) && arms[0].guard.is_none();
        let is_literal = is_literal_match(arms);
        // A literal pattern ANYWHERE, without the whole match satisfying
        // `is_literal_match`'s shape (all-literal-plus-trailing-
        // catchall), means there's no valid trailing catch-all —
        // `bind_pattern`'s new Int/Float/Bool/Str cases would otherwise
        // happily accept each arm individually with no complaint, so
        // this has to be checked explicitly, same reasoning as the
        // guard check just below.
        let has_literal_arm = arms
            .iter()
            .any(|arm| matches!(arm.pattern, ast::Pattern::Int(..) | ast::Pattern::Float(..) | ast::Pattern::Bool(..) | ast::Pattern::Str(..)));
        if has_literal_arm && !is_literal {
            return Err(format!(
                "a `match` over literal patterns must end with a wildcard (`_`) or \
                 bare-identifier arm at {:?}",
                arms.last().expect("has_literal_arm implies at least one arm").span
            ));
        }
        if is_literal {
            let last = arms.last().expect("is_literal_match already checked arms is non-empty");
            if last.guard.is_some() {
                return Err(format!(
                    "the trailing wildcard/identifier arm of a literal `match` cannot have a \
                     guard (it must be able to unconditionally catch anything earlier arms \
                     didn't) at {:?}",
                    last.span
                ));
            }
        }

        for (i, arm) in arms.iter().enumerate() {
            // Mirrors lower.rs's `lower_match`/`classify_subpattern`
            // restriction: a guard can only see bindings introduced
            // directly by the arm's OWN pattern, not ones that only
            // exist deep inside `wrap_nested_destructures`'s Match
            // chain around the BODY — so a guard combined with a
            // pattern that itself contains a nested Variant/Tuple/
            // Struct sub-pattern isn't accepted here either, even
            // though `bind_pattern` itself has no trouble binding
            // nested names (it needs no synthetic names at all — see
            // `bind_pattern`'s doc comment). Keeping this restriction
            // in sync with lowering avoids code that type-checks but
            // then fails at the lowering gate right after.
            if arm.guard.is_some() && pattern_has_nested_tag_subpattern(&arm.pattern) {
                return Err(format!(
                    "type inference not yet implemented for a match guard combined with a \
                     nested pattern at {:?}",
                    arm.span
                ));
            }
            // `bind_pattern` accepts a bare identifier/wildcard fine —
            // correct for a NESTED sub-position, and now ALSO correct
            // as a WHOLE top-level arm in THREE shapes: a lone catch-
            // all, the required trailing arm of a literal match, OR
            // (mirroring lower.rs's `DEFAULT_ARM_TAG` sentinel) a
            // catch-all in the LAST position mixed among otherwise
            // Ctor-tag-shaped arms (e.g. `Circle(r) => .., _ => ..`) —
            // but still wrong anywhere else (a catch-all in a non-last
            // position has no lowering either way). Guard here so the
            // type checker doesn't accept more than what actually
            // lowers. `bind_pattern`'s existing `Ident`/`Wildcard` case
            // already does exactly the right thing for this new mixed
            // shape too — it binds the WHOLE scrutinee type under the
            // name (or nothing, for `_`), no separate handling needed.
            let is_last = i == arms.len() - 1;
            if matches!(arm.pattern, ast::Pattern::Ident(..) | ast::Pattern::Wildcard(..)) && !is_single_catchall && !is_last {
                return Err(format!(
                    "type inference not yet implemented for a bare identifier or wildcard \
                     mixed into an otherwise tag-shaped match anywhere but the LAST arm at {:?}",
                    arm.pattern.span()
                ));
            }
            // Every arm unifies its OWN pattern shape against the SAME
            // `scrutinee_ty`, via `bind_pattern` — this is what enforces
            // "every arm deconstructs the same type" for free, with no
            // separate cross-arm bookkeeping needed: arm 2 unifying
            // against a `scrutinee_ty` arm 1 has ALREADY pinned to some
            // shape naturally fails if arm 2's shape is incompatible,
            // and naturally succeeds (without over-constraining fresh
            // variables against each other) when it's compatible, e.g.
            // two arms that are both some N-tuple. `Or` is dispatched
            // separately, to `infer_or_pattern` — see that function's
            // own doc comment for why it's NOT one of `bind_pattern`'s
            // ordinary cases (it's only ever valid as a WHOLE top-level
            // arm, never nested).
            let arm_env = match &arm.pattern {
                ast::Pattern::Or(alts, span) => {
                    self.infer_or_pattern(alts, *span, &acc.apply(&scrutinee_ty), env, &mut acc)?
                }
                _ => self.bind_pattern(&arm.pattern, &acc.apply(&scrutinee_ty), env.clone(), &mut acc)?,
            };
            let arm_env = arm_env.apply_subst(&acc);

            if let Some(guard) = &arm.guard {
                let (guard_ty, s) = self.infer_expr(guard, &arm_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&guard_ty), &Type::Bool).map_err(|e| format!("match guard: {e}"))?;
                acc = s.compose(&acc);
            }

            let (body_ty, s) = self.infer_expr(&arm.body, &arm_env)?;
            acc = s.compose(&acc);

            match &result_ty {
                None => result_ty = Some(acc.apply(&body_ty)),
                Some(prev) => {
                    let s = unify(&acc.apply(prev), &acc.apply(&body_ty))
                        .map_err(|e| format!("match arms must produce the same type: {e}"))?;
                    acc = s.compose(&acc);
                    result_ty = Some(acc.apply(prev));
                }
            }
        }

        // Exhaustiveness: if the scrutinee is a known ENUM type, every
        // declared variant must be covered — by a real Ctor-tag arm
        // naming it directly, an or-pattern alternative naming it, or a
        // valid trailing catch-all (which is, by construction, already
        // proven to accept anything — see `is_catchall_pattern`). A
        // missing variant is a compile-time error here instead of the
        // SAME runtime "no match arm for tag" error Ctor-matching
        // already has for other reasons (a failed guard, for instance)
        // — see DESIGN.md's "Pattern grammar" section for the full
        // reasoning. Struct/Tuple scrutinees need no such check: their
        // single Ctor tag is trivially covered by any arm that type-
        // checks against them at all.
        //
        // A GUARDED arm still counts as covering its tag here — a
        // deliberate, discussed choice (not an oversight), matching
        // this project's established "prefer false negatives over
        // false positives" risk direction (same principle behind
        // movecheck.rs's permissiveness): a variant reachable ONLY
        // through a guarded arm whose guard fails still hits the
        // existing runtime error, unchanged — this check simply adds a
        // compile-time net for the MORE common case (a variant with NO
        // arm at all), without trying to reason about guard truth at
        // compile time. Flagged as a genuine revisit candidate, not a
        // settled-forever design: a future stricter mode requiring
        // UNGUARDED coverage (matching Rust's own rule) is real,
        // plausible follow-up work, not ruled out.
        let resolved_scrutinee_ty = acc.apply(&scrutinee_ty);
        if let Type::Enum(enum_name, _) = &resolved_scrutinee_ty {
            if let Some(all_tags) = self.ctx.enum_variant_tags(enum_name) {
                let has_catchall = arms.last().map(|last| is_catchall_pattern(&last.pattern)).unwrap_or(false);
                if !has_catchall {
                    let mut covered: HashSet<&str> = HashSet::new();
                    for arm in arms {
                        match &arm.pattern {
                            ast::Pattern::Variant { path, .. } => {
                                if let Some(tag) = path.last() {
                                    covered.insert(tag.as_str());
                                }
                            }
                            ast::Pattern::Or(alts, _) => {
                                for alt in alts {
                                    if let ast::Pattern::Variant { path, .. } = alt {
                                        if let Some(tag) = path.last() {
                                            covered.insert(tag.as_str());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let missing: Vec<&str> =
                        all_tags.iter().map(String::as_str).filter(|t| !covered.contains(t)).collect();
                    if !missing.is_empty() {
                        return Err(format!(
                            "match is not exhaustive — missing variant(s): {}",
                            missing.join(", ")
                        ));
                    }
                }
            }
        }

        let final_ty = result_ty.ok_or_else(|| "match with no arms has no result type".to_string())?;
        Ok((acc.apply(&final_ty), acc))
    }

    // `select { pattern = expr => body, ... }` — each arm gets its OWN
    // independent "scrutinee" (whatever ITS channel receives), unlike
    // `infer_match`'s arms which all deconstruct the SAME shared
    // scrutinee type. Because of that, a bare `Ident`/`Wildcard` arm
    // pattern is perfectly valid here (the common case, even —
    // `v = rx.recv() => ...`) and needs none of `infer_match`'s "no
    // default-arm concept" restriction; `bind_pattern` already handles
    // it correctly on its own. Every arm's `expr` is required to be an
    // `X.recv()` call shape — checked here the same way lower.rs's
    // `lower_select` checks it, so a program that type-checks never
    // fails at the lowering gate right after for a DIFFERENT reason.
    fn infer_select(&mut self, arms: &[ast::SelectArm], span: plum_syntax::span::Span, env: &TypeEnv) -> Result<(Type, Subst), String> {
        let mut acc = Subst::empty();
        let mut result_ty: Option<Type> = None;

        for arm in arms {
            let ast::Expr::Call {
                callee,
                args,
                span: call_span,
            } = &arm.expr
            else {
                return Err(format!("`select` arm requires an `expr.recv()` call at {:?}", arm.expr.span()));
            };
            let ast::Expr::Field { base, name, .. } = callee.as_ref() else {
                return Err(format!("`select` arm requires an `expr.recv()` call at {call_span:?}"));
            };
            if name != "recv" || !args.is_empty() {
                return Err(format!("`select` arm requires an `expr.recv()` call at {call_span:?}"));
            }

            let (base_ty, s) = self.infer_expr(base, env)?;
            acc = s.compose(&acc);
            let elem_ty = self.fresh();
            let s = unify(&acc.apply(&base_ty), &Type::Struct("Receiver".to_string(), vec![elem_ty.clone()]))
                .map_err(|e| format!("`select` arm at {call_span:?}: {e}"))?;
            acc = s.compose(&acc);

            let refined_env = env.apply_subst(&acc);
            let arm_env = self.bind_pattern(&arm.pattern, &acc.apply(&elem_ty), refined_env, &mut acc)?;
            let arm_env = arm_env.apply_subst(&acc);

            let (body_ty, s) = self.infer_expr(&arm.body, &arm_env)?;
            acc = s.compose(&acc);

            match &result_ty {
                None => result_ty = Some(acc.apply(&body_ty)),
                Some(prev) => {
                    let s = unify(&acc.apply(prev), &acc.apply(&body_ty))
                        .map_err(|e| format!("select arms must produce the same type: {e}"))?;
                    acc = s.compose(&acc);
                    result_ty = Some(acc.apply(prev));
                }
            }
        }

        let final_ty = result_ty.ok_or_else(|| format!("select with no arms has no result type at {span:?}"))?;
        Ok((acc.apply(&final_ty), acc))
    }

    // Unlike a named top-level function (which gets a totally fresh,
    // isolated environment — see plum-interp's `function_body_cannot_
    // see_the_caller_environment`), a closure DOES see the surrounding
    // scope — that's the actual definition of a closure. `closure_env`
    // extends the caller's `env`, not a fresh one, on purpose.
    fn infer_closure(
        &mut self,
        params: &[ast::ClosureParam],
        body: &ast::Expr,
        env: &TypeEnv,
        span: Span,
    ) -> Result<(Type, Subst), String> {
        let mut param_types = Vec::with_capacity(params.len());
        let mut closure_env = env.clone();
        for p in params {
            let ty = match &p.ty {
                Some(annotation) => ast_type_to_type(annotation, &self.ctx, &[])?,
                None => self.fresh(),
            };
            closure_env = closure_env.extend(p.name.clone(), ty.clone());
            param_types.push(ty);
        }
        let (body_ty, acc) = self.infer_expr(body, &closure_env)?;
        // Recorded RAW (pre-`acc`-application) — same "the program's
        // FINAL substitution, applied later by `resolve_closure_types`,
        // will chase through whatever unification history this Var
        // participates in anywhere else in the program" reasoning as
        // `empty_array_elem_types` uses; see that field's own doc
        // comment. Needed so `plum-ir`'s lowering can bake a concrete
        // param/return type into the `ir::Expr::Closure` node it
        // produces for THIS span — see `LoweringContext::closure_types`.
        self.closure_types.insert(span, (param_types.clone(), body_ty.clone()));
        let resolved_params = param_types.iter().map(|t| acc.apply(t)).collect();
        Ok((Type::Function(resolved_params, Box::new(acc.apply(&body_ty))), acc))
    }

    // `for pattern in iter { body }` — mirrors lower.rs's `lower_for`
    // restrictions exactly (plain-identifier pattern; `iter` must be
    // EITHER a literal `start..end` Range or any expression whose
    // inferred type is `Type::Range`), since accepting anything
    // lowering rejects would mean a program could pass type-checking
    // and then fail to lower — the type checker's job is to predict
    // what actually runs, not a superset of it. Always produces Unit:
    // `body`'s value is discarded every iteration, same as a
    // statement-expression in a block.
    fn infer_for(
        &mut self,
        pattern: &ast::Pattern,
        iter: &ast::Expr,
        body: &ast::Block,
        span: plum_syntax::span::Span,
        env: &TypeEnv,
    ) -> Result<(Type, Subst), String> {
        let var = match pattern {
            ast::Pattern::Ident(name, _) => name.clone(),
            other => {
                return Err(format!(
                    "type inference not yet implemented for destructuring `for` patterns at {:?}",
                    other.span()
                ));
            }
        };

        let mut acc = match iter {
            // The literal shape: check start/end are each Int directly
            // (a slightly more direct route to the same place as
            // falling through to the general Range-typed case below,
            // but with error messages that point at the literal bounds
            // rather than a generic "Range" mismatch).
            ast::Expr::Binary {
                op: ast::BinaryOp::Range,
                lhs,
                rhs,
                ..
            } => {
                let (start_ty, s) = self.infer_expr(lhs, env)?;
                let mut acc = s;
                let s = unify(&acc.apply(&start_ty), &Type::Int).map_err(|e| format!("`for` range start: {e}"))?;
                acc = s.compose(&acc);
                let refined_env = env.apply_subst(&acc);

                let (end_ty, s) = self.infer_expr(rhs, &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&end_ty), &Type::Int).map_err(|e| format!("`for` range end: {e}"))?;
                s.compose(&acc)
            }
            _ => {
                let (iter_ty, s) = self.infer_expr(iter, env)?;
                let mut acc = s;
                let resolved_iter_ty = acc.apply(&iter_ty);
                // `for x in arr` — the iterand is an `Array[T]`, not a
                // `Range`. Checked by matching the ALREADY-RESOLVED
                // shape directly, rather than by trying to `unify`
                // against `Array[fresh]` and seeing if it succeeds:
                // an unresolved type variable (e.g. a still-generic
                // function parameter's type, as in `let sum_range r =
                // for i in r {...}`) would trivially unify against
                // EITHER Array or Range, since unifying a bare var
                // just binds it — that would wrongly commit a
                // genuinely Range-typed polymorphic loop to the array
                // desugaring. Matching the resolved shape means only
                // an iterand that's ALREADY definitely `Array[T]`
                // takes this path; everything else (including a still-
                // unresolved var) falls through to the existing
                // Range-unifying behavior below, unchanged.
                if let Type::Struct(name, args) = &resolved_iter_ty {
                    if name == "Array" && args.len() == 1 {
                        self.array_for_loops.insert(span);
                        let elem_ty = args[0].clone();
                        let body_env = env.apply_subst(&acc).extend(var, elem_ty);
                        let (_, s) = self.infer_block(body, &body_env)?;
                        acc = s.compose(&acc);
                        return Ok((Type::Unit, acc));
                    }
                }
                let s = unify(&resolved_iter_ty, &Type::Range)
                    .map_err(|e| format!("`for` at {span:?}: {e}"))?;
                acc = s.compose(&acc);
                acc
            }
        };

        let body_env = env.apply_subst(&acc).extend(var, Type::Int);
        let (_, s) = self.infer_block(body, &body_env)?;
        acc = s.compose(&acc);

        Ok((Type::Unit, acc))
    }

    fn infer_unary(&mut self, op: &ast::UnaryOp, expr: &ast::Expr, env: &TypeEnv) -> Result<(Type, Subst), String> {
        let (operand_ty, mut acc) = self.infer_expr(expr, env)?;
        match op {
            ast::UnaryOp::Neg => {
                let resolved = acc.apply(&operand_ty);
                let (final_ty, s) = default_numeric(&resolved)?;
                acc = s.compose(&acc);
                Ok((final_ty, acc))
            }
            ast::UnaryOp::Not => {
                let s = unify(&acc.apply(&operand_ty), &Type::Bool).map_err(|e| format!("`!`: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Bool, acc))
            }
        }
    }

    fn infer_binary(
        &mut self,
        op: &ast::BinaryOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        env: &TypeEnv,
    ) -> Result<(Type, Subst), String> {
        use ast::BinaryOp::*;

        // `&&`/`||`: each operand independently must unify with Bool.
        if matches!(op, And | Or) {
            let (lty, s) = self.infer_expr(lhs, env)?;
            let mut acc = s;
            let s = unify(&acc.apply(&lty), &Type::Bool).map_err(|e| format!("logical operator: {e}"))?;
            acc = s.compose(&acc);
            let (rty, s) = self.infer_expr(rhs, &env.apply_subst(&acc))?;
            acc = s.compose(&acc);
            let s = unify(&acc.apply(&rty), &Type::Bool).map_err(|e| format!("logical operator: {e}"))?;
            acc = s.compose(&acc);
            return Ok((Type::Bool, acc));
        }

        // Every other operator handled here needs both operands to
        // unify with each other first.
        let (lty, s) = self.infer_expr(lhs, env)?;
        let mut acc = s;
        let (rty, s) = self.infer_expr(rhs, &env.apply_subst(&acc))?;
        acc = s.compose(&acc);
        let s = unify(&acc.apply(&lty), &acc.apply(&rty)).map_err(|e| format!("operator: {e}"))?;
        acc = s.compose(&acc);
        let operand_ty = acc.apply(&lty);

        match op {
            Eq | Ne => Ok((Type::Bool, acc)),
            Lt | Gt | Le | Ge => {
                let (_, s) = default_numeric(&operand_ty)?;
                acc = s.compose(&acc);
                Ok((Type::Bool, acc))
            }
            Add | Sub | Mul | Div | Rem => {
                let (final_ty, s) = default_numeric(&operand_ty)?;
                acc = s.compose(&acc);
                Ok((final_ty, acc))
            }
            And | Or | Pipe | Range => unreachable!("handled above or by the Binary match arm in infer_expr"),
        }
    }

    fn infer_if(
        &mut self,
        cond: &ast::Expr,
        then_branch: &ast::Block,
        else_branch: &Option<Box<ast::Expr>>,
        env: &TypeEnv,
    ) -> Result<(Type, Subst), String> {
        let (cond_ty, s) = self.infer_expr(cond, env)?;
        let mut acc = s;
        let s = unify(&acc.apply(&cond_ty), &Type::Bool).map_err(|e| format!("`if` condition: {e}"))?;
        acc = s.compose(&acc);
        let mut refined_env = env.apply_subst(&acc);

        let (then_ty, s) = self.infer_block(then_branch, &refined_env)?;
        acc = s.compose(&acc);
        refined_env = refined_env.apply_subst(&acc);

        match else_branch {
            Some(else_expr) => {
                let (else_ty, s) = self.infer_expr(else_expr, &refined_env)?;
                acc = s.compose(&acc);
                let s = unify(&acc.apply(&then_ty), &acc.apply(&else_ty))
                    .map_err(|e| format!("`if`/`else` branches must match: {e}"))?;
                acc = s.compose(&acc);
                Ok((acc.apply(&then_ty), acc))
            }
            None => {
                let s = unify(&acc.apply(&then_ty), &Type::Unit)
                    .map_err(|e| format!("`if` without `else` must produce Unit: {e}"))?;
                acc = s.compose(&acc);
                Ok((Type::Unit, acc))
            }
        }
    }

    fn infer_block(&mut self, block: &ast::Block, env: &TypeEnv) -> Result<(Type, Subst), String> {
        let mut acc = Subst::empty();
        let mut cur_env = env.clone();
        for stmt in &block.stmts {
            match stmt {
                ast::Stmt::Let {
                    pattern: ast::Pattern::Ident(name, _),
                    value,
                    ty,
                    ..
                } => {
                    // Self-referential closures (`let fib = |n| ..
                    // fib(n-1) ..;`) need `name` visible to `value`'s
                    // OWN inference when `value` is itself a closure
                    // literal — pre-bind it to a fresh placeholder type
                    // first, same trick the top-level global case in
                    // `infer_program` uses (see that loop's own doc
                    // comment for the full reasoning). Unlike the
                    // global case, this ALSO needs a matching runtime
                    // fix — see `Interpreter::eval`'s `Let` case — since
                    // a local closure's captured environment is a
                    // one-time SNAPSHOT taken at creation time, not a
                    // live lookup chain the way globals are.
                    let (val_ty, s) = if matches!(value, ast::Expr::Closure { .. }) {
                        let placeholder = self.fresh();
                        let rec_env = cur_env.extend(name.clone(), placeholder.clone());
                        let (body_ty, s) = self.infer_expr(value, &rec_env)?;
                        let mut acc2 = s;
                        let s2 = unify(&acc2.apply(&placeholder), &acc2.apply(&body_ty))
                            .map_err(|e| format!("recursive closure {name:?}: {e}"))?;
                        acc2 = s2.compose(&acc2);
                        (acc2.apply(&body_ty), acc2)
                    } else {
                        self.infer_expr(value, &cur_env)?
                    };
                    acc = s.compose(&acc);
                    let mut resolved = acc.apply(&val_ty);
                    if let Some(annotation) = ty {
                        let ann_ty = ast_type_to_type(annotation, &self.ctx, &[])?;
                        let s = unify(&resolved, &ann_ty)
                            .map_err(|e| format!("`let` annotation for {name:?}: {e}"))?;
                        acc = s.compose(&acc);
                        resolved = acc.apply(&resolved);
                    }
                    cur_env = cur_env.extend(name.clone(), resolved);
                    cur_env = cur_env.apply_subst(&acc);
                }
                // `let (a, b) = expr;` / `let Point { x, y } = expr;` —
                // `bind_pattern` unifies the value's type against
                // whatever shape the pattern requires and binds every
                // name it introduces, including nested ones. No type
                // annotation support here yet: `ast::Type` has no
                // Tuple case at the SURFACE grammar level at all (only
                // ever inferred, never spelled), and a struct pattern's
                // annotation would be redundant with its own path, so
                // neither has anything meaningful to unify against.
                ast::Stmt::Let {
                    pattern: pattern @ (ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. }),
                    value,
                    ty,
                    ..
                } => {
                    if ty.is_some() {
                        return Err(format!(
                            "type inference not yet implemented for type annotations on \
                             destructuring `let` at {:?}",
                            pattern.span()
                        ));
                    }
                    let (val_ty, s) = self.infer_expr(value, &cur_env)?;
                    acc = s.compose(&acc);
                    let scrutinee_ty = acc.apply(&val_ty);
                    cur_env = self.bind_pattern(pattern, &scrutinee_ty, cur_env, &mut acc)?;
                    cur_env = cur_env.apply_subst(&acc);
                }
                ast::Stmt::Let { pattern, .. } => {
                    return Err(format!(
                        "type inference not yet implemented for destructuring let-bindings \
                         of this shape at {:?}",
                        pattern.span()
                    ));
                }
                ast::Stmt::Expr(e) => {
                    let (_, s) = self.infer_expr(e, &cur_env)?;
                    acc = s.compose(&acc);
                    cur_env = cur_env.apply_subst(&acc);
                }
                ast::Stmt::Assign { name, value, span } => {
                    // The target must already be in scope, and the
                    // assigned value's type must match its EXISTING
                    // type — reassignment can't change what a variable
                    // holds. What's NOT checked: that `name` was
                    // actually declared `let mut` — no static
                    // mutability check exists at this layer yet, same
                    // deliberate gap as lowering (see ir.rs's `Assign`
                    // doc comment).
                    let existing = cur_env
                        .lookup_scheme(name)
                        .cloned()
                        .ok_or_else(|| format!("assignment to undefined variable {name:?} at {span:?}"))?;
                    let existing_ty = self.instantiate(&existing);
                    let (val_ty, s) = self.infer_expr(value, &cur_env)?;
                    acc = s.compose(&acc);
                    let s = unify(&acc.apply(&existing_ty), &acc.apply(&val_ty))
                        .map_err(|e| format!("assignment to {name:?}: {e}"))?;
                    acc = s.compose(&acc);
                    cur_env = cur_env.apply_subst(&acc);
                }
            }
        }
        match &block.tail {
            Some(tail) => {
                let (ty, s) = self.infer_expr(tail, &cur_env)?;
                acc = s.compose(&acc);
                Ok((acc.apply(&ty), acc))
            }
            None => Ok((Type::Unit, acc)),
        }
    }
}

/// Checks that `ty` is numeric, WITH a deliberate simplification: if
/// `ty` is still an unresolved type variable (nothing else pinned it —
/// e.g. `let add a b = a + b`, where neither `a` nor `b` is ever
/// compared against a literal), it gets DEFAULTED to `Int` rather than
/// rejected. No typeclass/constraint machinery exists yet — a real
/// system would infer a polymorphic `Num a => a -> a -> a` here
/// instead. This is the same spirit as Rust defaulting an ambiguous
/// integer literal's type, not a permanent design decision. Returns
/// the resolved type AND the substitution recording the default, if
/// one was applied — the caller must compose it into their own
/// accumulator, same as any other substitution.
fn default_numeric(ty: &Type) -> Result<(Type, Subst), String> {
    match ty {
        Type::Int | Type::Float => Ok((ty.clone(), Subst::empty())),
        Type::Var(id) => Ok((Type::Int, Subst::single(*id, Type::Int))),
        other => Err(format!("expected a numeric type (Int or Float), found {other:?}")),
    }
}

// Replaces every `Type::Param(name)` inside `ty` per `mapping`,
// recursing structurally — the declaration-template-scoped counterpart
// to `Subst::apply` (which only ever resolves `Type::Var` metavariables,
// never `Type::Param`s). A name missing from `mapping` is left as-is
// (should never happen in practice: only a declaration's OWN parameter
// names ever appear inside ITS OWN stored field/payload types — see
// `TypeContext::from_items`) rather than panicking, since a stray
// unresolved `Param` still gets caught later, as a clear internal-error
// message, by `unify.rs`.
pub(crate) fn subst_params(ty: &Type, mapping: &HashMap<String, Type>) -> Type {
    match ty {
        Type::Param(name) => mapping.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Type::Function(params, ret) => Type::Function(
            params.iter().map(|p| subst_params(p, mapping)).collect(),
            Box::new(subst_params(ret, mapping)),
        ),
        Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| subst_params(e, mapping)).collect()),
        Type::Struct(name, args) => {
            Type::Struct(name.clone(), args.iter().map(|a| subst_params(a, mapping)).collect())
        }
        Type::Enum(name, args) => Type::Enum(name.clone(), args.iter().map(|a| subst_params(a, mapping)).collect()),
        other => other.clone(),
    }
}

// DESIGN.md's "Ad-hoc polymorphism (v1)": a small, FIXED, compiler-
// known set of overloadable traits (`Num`, `Eq`, `Show`) — no user-
// definable typeclasses, so checking a bound is closed-set membership
// checking against this function, never general typeclass resolution.
// `Task`/`Sender`/`Receiver` are deliberately excluded from `Eq`/`Show`
// even though they're plain `Type::Struct` under the hood (see their
// own doc comments in `plum-interp`) — they're opaque runtime handles
// with no meaningful equality or display, unlike a real user struct.
// Mirrors lower.rs's identically-named free functions exactly — see
// `lower_catchall_only_match`/`lower_literal_match`'s doc comments
// there for the full reasoning. Duplicated rather than shared (this
// crate and plum-ir don't share code — the same established pattern
// every other shape-detection check in this file already follows,
// e.g. `.push()`/`.len()`/etc. are each independently checked in both
// lower.rs and infer.rs).
fn is_catchall_pattern(pattern: &ast::Pattern) -> bool {
    matches!(pattern, ast::Pattern::Wildcard(_) | ast::Pattern::Ident(..))
}

fn is_literal_match(arms: &[ast::MatchArm]) -> bool {
    match arms.split_last() {
        Some((last, rest)) => {
            is_catchall_pattern(&last.pattern)
                && rest.iter().all(|arm| {
                    matches!(
                        arm.pattern,
                        ast::Pattern::Int(..) | ast::Pattern::Float(..) | ast::Pattern::Bool(..) | ast::Pattern::Str(..)
                    )
                })
        }
        None => false,
    }
}

fn satisfies_bound(ty: &Type, bound: &str) -> bool {
    let is_opaque_runtime_handle =
        matches!(ty, Type::Struct(n, _) if n == "Task" || n == "Sender" || n == "Receiver" || n == "Ref");
    match bound {
        "Num" => matches!(ty, Type::Int | Type::Float),
        "Eq" | "Show" => !matches!(ty, Type::Function(..)) && !is_opaque_runtime_handle,
        // An unrecognized bound name isn't rejected here — DESIGN.md's
        // closed trait set is a small, fixed list, but a genuinely
        // unknown trait name is a separate, not-yet-implemented gap
        // (no parser/context-level validation of bound NAMES exists
        // yet either), not something this permissive fallback should
        // block on.
        _ => true,
    }
}

// Mirrors lower.rs's `classify_subpattern`: true if any DIRECT
// sub-position of `pattern` is itself a Variant/Tuple/Struct pattern —
// exactly the shapes that make lowering defer to a synthetic name and
// a follow-up `Match` (see `wrap_nested_destructures`), which is why a
// guard can't yet see bindings introduced that way. A bare top-level
// Ident/Wildcard pattern (already rejected earlier in `infer_match`
// for an unrelated reason) and a tag pattern with only plain
// Ident/Wildcard sub-positions both return `false`.
fn pattern_has_nested_tag_subpattern(pattern: &ast::Pattern) -> bool {
    fn is_nested_shape(p: &ast::Pattern) -> bool {
        matches!(p, ast::Pattern::Variant { .. } | ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. })
    }
    match pattern {
        ast::Pattern::Variant { args, .. } => args.iter().any(|a| is_nested_shape(a)),
        ast::Pattern::Tuple(elems, _) => elems.iter().any(|e| is_nested_shape(e)),
        ast::Pattern::Struct { fields, .. } => fields.iter().any(|f| is_nested_shape(&f.pattern)),
        _ => false,
    }
}

// pub(crate) so context.rs can reuse it for struct field / enum
// variant payload type annotations. Resolves a name against `ctx`'s
// known struct/enum names (see `TypeContext::from_items`'s two-phase
// construction — names are all collected BEFORE any field/payload type
// is resolved, so `struct Line { start: Point, end: Point }` and even
// forward/mutual references like `struct A { b: B } struct B { a: A }`
// both work regardless of declaration order).
//
// `in_scope_params` is the CURRENT struct/enum declaration's own
// generic parameter names (empty for every non-declaration call site —
// closures, `let` annotations, and any OTHER declaration's fields all
// pass `&[]`, since a parameter is scoped to its own declaration
// alone): a bare name matching one of them resolves to `Type::Param`
// instead of erroring, which is what lets `struct Pair[T] { first: T,
// second: T }` refer to its own `T`. This deliberately does NOT cover
// a generic annotation on a top-level FUNCTION (`let f[T] (x: T)`) —
// top-level function params have no annotation syntax at all yet (a
// separate, already-known gap), so that combination can't arise here.
pub(crate) fn ast_type_to_type(
    ty: &ast::Type,
    ctx: &crate::context::TypeContext,
    in_scope_params: &[String],
) -> Result<Type, String> {
    match ty {
        ast::Type::Path(segments, span) => match segments.last().map(String::as_str) {
            Some("Int") => Ok(Type::Int),
            Some("Float") => Ok(Type::Float),
            Some("Bool") => Ok(Type::Bool),
            Some("String") => Ok(Type::Str),
            Some("Unit") => Ok(Type::Unit),
            Some(name) if in_scope_params.iter().any(|p| p == name) => Ok(Type::Param(name.to_string())),
            Some(name) if ctx.is_struct(name) => Ok(Type::Struct(name.to_string(), Vec::new())),
            Some(name) if ctx.is_enum(name) => Ok(Type::Enum(name.to_string(), Vec::new())),
            _ => Err(format!(
                "type inference not yet implemented for this type annotation at {span:?}"
            )),
        },
        // `Thing[Arg, ...]` — `base` names a generic struct/enum,
        // `args` are its type arguments at THIS use (each resolved
        // recursively, so `Wrapper[Pair[Int]]` works). Arity is
        // checked against the declaration's own `generic_params`
        // count.
        ast::Type::Generic { base, args, span } => {
            let name = base.last().map(String::as_str).ok_or_else(|| {
                format!("type inference not yet implemented for this type annotation at {span:?}")
            })?;
            // Same opaque-pseudo-generic-builtin-types gap `resolve_
            // annotation` already had to check for, in exactly the
            // same way — this function is the OTHER `ast::Type ->
            // Type` converter (struct/enum field declarations,
            // generic type arguments, etc.), so it needs its own copy
            // of the same fixed-arity-one check. A real, pre-existing
            // gap for ALL FOUR names, not just `Ref` — caught here via
            // `struct Counter { value: Ref[Int] }`.
            if matches!(name, "Array" | "Task" | "Sender" | "Receiver" | "Ref") {
                if args.len() != 1 {
                    return Err(format!("{name:?} expects 1 generic argument, found {} at {span:?}", args.len()));
                }
                let resolved_arg = ast_type_to_type(&args[0], ctx, in_scope_params)?;
                return Ok(Type::Struct(name.to_string(), vec![resolved_arg]));
            }
            let Some(declared_params) = ctx.generic_params(name) else {
                return Err(format!(
                    "type inference not yet implemented for this type annotation at {span:?}"
                ));
            };
            if args.len() != declared_params.len() {
                return Err(format!(
                    "{name:?} expects {} generic argument(s), found {} at {span:?}",
                    declared_params.len(),
                    args.len()
                ));
            }
            let resolved_args = args
                .iter()
                .map(|a| ast_type_to_type(a, ctx, in_scope_params))
                .collect::<Result<Vec<_>, _>>()?;
            if ctx.is_struct(name) {
                Ok(Type::Struct(name.to_string(), resolved_args))
            } else if ctx.is_enum(name) {
                Ok(Type::Enum(name.to_string(), resolved_args))
            } else {
                Err(format!(
                    "type inference not yet implemented for this type annotation at {span:?}"
                ))
            }
        }
        // `(A, B) -> R` — resolves directly to the SAME `Type::Function`
        // ordinary closures/functions already carry, giving this
        // syntax real meaning as a general annotation (function param,
        // struct field, etc.), not just for `extern` callback
        // signatures — see ast.rs's `Type::Function` doc comment for
        // why this was worth implementing beyond the FFI use case.
        ast::Type::Function { params, ret, .. } => {
            let param_types = params
                .iter()
                .map(|p| ast_type_to_type(p, ctx, in_scope_params))
                .collect::<Result<Vec<_>, _>>()?;
            let ret_type = ast_type_to_type(ret, ctx, in_scope_params)?;
            Ok(Type::Function(param_types, Box::new(ret_type)))
        }
    }
}

impl Default for Infer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_syntax::lexer::Lexer;
    use plum_syntax::parser::Parser;

    #[test]
    fn if_condition_constraint_on_an_existing_binding_propagates_into_sibling_branches() {
        // Regression test: before `TypeEnv::apply_subst` was threaded
        // through infer_if between the condition and the branches, a
        // name's type learned from the condition (`n` pinned to Int by
        // `n == 0`) was invisible when inferring the then-branch, since
        // that branch still looked `n` up as its original, unconstrained
        // `Var`. A conflicting local unification there (`!n`, which
        // needs Bool) then silently WON when substitutions were
        // composed — `compose` favors the caller's already-established
        // binding for a repeated variable, so the fresh, uninformed one
        // from inside the branch just got discarded rather than caught
        // as a conflict. `n` can't be both Int and Bool; this must error.
        let mut infer = Infer::new();
        let n_var = infer.fresh();
        let env = TypeEnv::new().extend("n".to_string(), n_var);
        let tokens = Lexer::new("if n == 0 { !n } else { true }").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error: {e}"));
        let err = infer
            .infer_expr(&ast, &env)
            .expect_err("expected a type error: n can't be both Int (from `n == 0`) and Bool (from `!n`)");
        assert!(err.contains("`!`"), "expected the conflict caught at `!n`, got: {err}");
    }

    #[test]
    fn fresh_variables_are_distinct_and_increasing() {
        // Not exercised by any infer_expr case yet in this pass (no
        // unannotated parameters or polymorphism to instantiate) — the
        // next pass needs this, so it's built now and proven correct
        // in isolation rather than left unused until it's wired in.
        let mut infer = Infer::new();
        assert_eq!(infer.fresh(), Type::Var(0));
        assert_eq!(infer.fresh(), Type::Var(1));
        assert_eq!(infer.fresh(), Type::Var(2));
    }

    fn infer(src: &str) -> Type {
        infer_in(src, &TypeEnv::new())
    }

    fn infer_in(src: &str, env: &TypeEnv) -> Type {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let mut infer = Infer::new();
        let (ty, subst) = infer
            .infer_expr(&ast, env)
            .unwrap_or_else(|e| panic!("inference error for {src:?}: {e}"));
        subst.apply(&ty)
    }

    fn infer_err(src: &str) -> String {
        infer_err_in(src, &TypeEnv::new())
    }

    fn infer_err_in(src: &str, env: &TypeEnv) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let mut infer = Infer::new();
        infer
            .infer_expr(&ast, env)
            .expect_err(&format!("expected inference of {src:?} to fail"))
    }

    #[test]
    fn literals() {
        assert_eq!(infer("5"), Type::Int);
        assert_eq!(infer("3.14"), Type::Float);
        assert_eq!(infer("true"), Type::Bool);
        assert_eq!(infer("\"hi\""), Type::Str);
        assert_eq!(infer("()"), Type::Unit);
    }

    #[test]
    fn variable_looked_up_in_env() {
        let env = TypeEnv::new().extend("x".to_string(), Type::Int);
        assert_eq!(infer_in("x", &env), Type::Int);
    }

    #[test]
    fn unbound_variable_is_an_error() {
        infer_err("y");
    }

    // --- `for`/`unsafe` ---

    #[test]
    fn range_as_a_standalone_expression_infers_as_range() {
        assert_eq!(infer("0..5"), Type::Range);
    }

    #[test]
    fn range_bounds_must_be_int_even_as_a_standalone_expression() {
        infer_err("true..5");
        infer_err("0..false");
    }

    #[test]
    fn for_loop_over_a_literal_range_infers_as_unit() {
        assert_eq!(infer("for i in 0..5 { i }"), Type::Unit);
    }

    #[test]
    fn for_loop_variable_is_bound_as_int_inside_the_body() {
        assert_eq!(infer("for i in 0..5 { i + 1 }"), Type::Unit);
    }

    #[test]
    fn for_loop_variable_does_not_leak_past_the_loop() {
        infer_err("{ for i in 0..5 { i }; i }");
    }

    #[test]
    fn for_loop_range_bounds_must_be_int() {
        infer_err("for i in true..5 { i }");
        infer_err("for i in 0..false { i }");
    }

    #[test]
    fn for_loop_range_bounds_can_be_arbitrary_int_expressions() {
        let env = TypeEnv::new().extend("n".to_string(), Type::Int);
        assert_eq!(infer_in("for i in 0..(n + 1) { i }", &env), Type::Unit);
    }

    #[test]
    fn for_loop_body_type_errors_propagate() {
        infer_err("for i in 0..5 { 1 + true }");
    }

    #[test]
    fn for_loop_over_a_range_typed_variable_infers_as_unit() {
        let env = TypeEnv::new().extend("xs".to_string(), Type::Range);
        assert_eq!(infer_in("for i in xs { i }", &env), Type::Unit);
    }

    #[test]
    fn for_loop_over_a_non_range_typed_expression_is_still_rejected() {
        infer_err("for i in 5 { i }");
    }

    #[test]
    fn for_loop_destructuring_pattern_is_not_yet_supported() {
        infer_err("for (a, b) in 0..5 { a }");
    }

    #[test]
    fn unsafe_block_infers_like_its_inner_block() {
        assert_eq!(infer("unsafe { 1 + 2 }"), Type::Int);
        assert_eq!(infer("unsafe { true }"), Type::Bool);
    }

    // --- `spawn` / `.join()` ---

    #[test]
    fn spawn_infers_as_task_of_the_blocks_type() {
        assert_eq!(infer("spawn { 1 + 2 }"), Type::Struct("Task".to_string(), vec![Type::Int]));
    }

    #[test]
    fn task_join_infers_as_the_blocks_type() {
        assert_eq!(infer("spawn { 1 + 2 }.join()"), Type::Int);
    }

    #[test]
    fn task_join_on_a_non_task_value_is_an_error() {
        infer_err("5.join()");
    }

    #[test]
    fn joining_the_same_task_twice_type_checks_the_same_way_both_times() {
        // Whether joining TWICE is a runtime error is `plum-interp`'s
        // concern (a `JoinHandle` is consumed by its first `.join()`) —
        // nothing about the SECOND `.join()` is distinguishable at the
        // type level, so both must infer identically.
        let env = TypeEnv::new().extend("t".to_string(), Type::Struct("Task".to_string(), vec![Type::Bool]));
        assert_eq!(infer_in("t.join()", &env), Type::Bool);
        assert_eq!(infer_in("{ t.join(); t.join() }", &env), Type::Bool);
    }

    // --- `channel[T]()` / `.send()` / `.recv()` ---

    #[test]
    fn channel_instantiation_infers_a_sender_receiver_pair() {
        assert_eq!(
            infer("channel[Int]()"),
            Type::Tuple(vec![
                Type::Struct("Sender".to_string(), vec![Type::Int]),
                Type::Struct("Receiver".to_string(), vec![Type::Int]),
            ])
        );
    }

    #[test]
    fn channel_send_and_recv_round_trip_through_the_same_element_type() {
        let ty = infer("{ let (tx, rx) = channel[Bool](); tx.send(true); rx.recv() }");
        assert_eq!(ty, Type::Bool);
    }

    #[test]
    fn channel_send_argument_type_is_checked_against_the_element_type() {
        let env = TypeEnv::new().extend("tx".to_string(), Type::Struct("Sender".to_string(), vec![Type::Int]));
        infer_expr_with_err(&mut Infer::new(), "tx.send(true)", &env);
    }

    #[test]
    fn channel_recv_infers_as_the_element_type() {
        let env = TypeEnv::new().extend("rx".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Bool]));
        assert_eq!(infer_in("rx.recv()", &env), Type::Bool);
    }

    #[test]
    fn send_on_a_non_sender_is_an_error() {
        infer_err("5.send(1)");
    }

    #[test]
    fn recv_on_a_non_receiver_is_an_error() {
        infer_err("5.recv()");
    }

    #[test]
    fn a_struct_value_can_cross_a_channel() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let expr_src = "{ let (tx, rx) = channel[Point](); tx.send(Point { x: 1, y: 2 }); match rx.recv() { Point(a, b) => a + b } }";
        assert_eq!(infer_expr_with(&mut infer, expr_src, &TypeEnv::new()), Type::Int);
    }

    // --- Ad-hoc polymorphism: `[T: Num]` bounds on generic structs/enums ---

    #[test]
    fn a_bound_satisfying_generic_struct_literal_is_accepted() {
        let mut infer = Infer::with_context(context("struct Box[T: Num] { val: T }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Box { val: 5 }", &TypeEnv::new()),
            Type::Struct("Box".to_string(), vec![Type::Int])
        );
        assert_eq!(
            infer_expr_with(&mut infer, "Box { val: 1.5 }", &TypeEnv::new()),
            Type::Struct("Box".to_string(), vec![Type::Float])
        );
    }

    #[test]
    fn a_bound_violating_generic_struct_literal_is_an_error() {
        let mut infer = Infer::with_context(context("struct Box[T: Num] { val: T }"));
        let err = infer_expr_with_err(&mut infer, "Box { val: true }", &TypeEnv::new());
        assert!(err.contains("Num"), "expected a Num-bound error, got: {err}");
    }

    #[test]
    fn a_bound_violating_generic_variant_construction_is_an_error() {
        let mut infer = Infer::with_context(context("enum Numeric[T: Num] { Val(T) }"));
        let err = infer_expr_with_err(&mut infer, "Val(true)", &TypeEnv::new());
        assert!(err.contains("Num"), "expected a Num-bound error, got: {err}");
    }

    #[test]
    fn a_bound_satisfying_generic_variant_construction_is_accepted() {
        let mut infer = Infer::with_context(context("enum Numeric[T: Num] { Val(T) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Val(5)", &TypeEnv::new()),
            Type::Enum("Numeric".to_string(), vec![Type::Int])
        );
    }

    #[test]
    fn combined_bounds_reject_a_type_failing_either_one() {
        let mut infer = Infer::with_context(context("struct Box[T: Num + Eq] { val: T }"));
        // Bool satisfies Eq but not Num.
        infer_expr_with_err(&mut infer, "Box { val: true }", &TypeEnv::new());
    }

    #[test]
    fn a_generic_struct_bound_by_eq_rejects_a_function() {
        let mut infer = Infer::with_context(context("struct Box[T: Eq] { val: T }"));
        let env = TypeEnv::new().extend("f".to_string(), fn_ty(vec![Type::Int], Type::Int));
        infer_expr_with_err(&mut infer, "Box { val: f }", &env);
    }

    #[test]
    fn an_unbounded_generic_struct_accepts_anything() {
        let mut infer = Infer::with_context(context("struct Pair[T] { first: T, second: T }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Pair { first: true, second: false }", &TypeEnv::new()),
            Type::Struct("Pair".to_string(), vec![Type::Bool])
        );
    }

    #[test]
    fn unsafe_block_type_errors_propagate() {
        infer_err("unsafe { 1 + true }");
    }

    // --- Tuples ---

    #[test]
    fn tuple_literal_infers_element_types() {
        assert_eq!(infer("(1, true)"), Type::Tuple(vec![Type::Int, Type::Bool]));
    }

    #[test]
    fn three_element_tuple_literal() {
        assert_eq!(
            infer("(1, true, \"hi\")"),
            Type::Tuple(vec![Type::Int, Type::Bool, Type::Str])
        );
    }

    #[test]
    fn empty_tuple_is_unit_not_tuple() {
        assert_eq!(infer("()"), Type::Unit);
    }

    #[test]
    fn block_let_tuple_destructure() {
        assert_eq!(infer("{ let (a, b) = (1, true); if b { a } else { 0 } }"), Type::Int);
    }

    #[test]
    fn block_let_tuple_destructure_with_wildcard() {
        assert_eq!(infer("{ let (a, _) = (1, true); a }"), Type::Int);
    }

    #[test]
    fn block_let_tuple_arity_mismatch_is_an_error() {
        infer_err("{ let (a, b, c) = (1, 2); a }");
    }

    #[test]
    fn block_let_tuple_annotation_is_not_yet_supported() {
        // `ast::Type` has no Tuple case at the surface grammar level at
        // all — there's no syntax to even try to write here yet.
        infer_err("{ let (a, b): Int = (1, 2); a }");
    }

    #[test]
    fn match_arm_tuple_pattern() {
        let env = TypeEnv::new().extend("p".to_string(), Type::Tuple(vec![Type::Int, Type::Bool]));
        assert_eq!(infer_in("match p { (a, b) => if b { a } else { 0 } }", &env), Type::Int);
    }

    #[test]
    fn match_scrutinee_tuple_type_is_inferred_from_the_arm() {
        // `p`'s type isn't known ahead of time — matching it against a
        // tuple pattern is what pins it down, same as the existing
        // enum-scrutinee-inference test. `a + 1` forces `a` (the
        // tuple's first element) concretely to Int, giving a clean,
        // predictable result type instead of an arbitrary unresolved
        // fresh variable. `p`'s var id is picked far from 0 — unlike
        // the enum/struct case, a tuple ARM allocates its own fresh
        // vars (one per element) starting from the SAME counter, so a
        // low hand-picked id here could accidentally collide with one
        // of those and alias two logically-unrelated variables.
        let env = TypeEnv::new().extend("p".to_string(), Type::Var(999));
        assert_eq!(infer_in("match p { (a, b) => a + 1 }", &env), Type::Int);
    }

    #[test]
    fn two_tuple_match_arms_of_the_same_arity_are_not_mixed() {
        // Two arms that are BOTH 2-tuples get independent fresh
        // variables each — this must NOT be treated as "mixing
        // incompatible types" the way a real struct-vs-enum mix would.
        assert_eq!(
            infer("match (1, true) { (a, b) => a, (c, d) => c }"),
            Type::Int
        );
    }

    #[test]
    fn match_arms_mixing_a_tuple_and_a_struct_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Var(0));
        infer_expr_with_err(&mut infer, "match p { (a, b) => a, Point(x, y) => x }", &env);
    }

    #[test]
    fn tuple_destructuring_function_param() {
        // The flagship example from examples/overview.plum / DESIGN.md.
        // Exact fresh-variable IDs are an implementation detail, so
        // this checks the SHAPE (one tuple param, one tuple return,
        // both 2 elements, elements swapped and still Vars) rather than
        // hardcoding specific IDs.
        let types = infer_program("let swap (a, b) = (b, a)");
        match &types["swap"] {
            Type::Function(params, ret) => {
                let Type::Tuple(param_elems) = &params[0] else {
                    panic!("expected a tuple param, got {:?}", params[0]);
                };
                let Type::Tuple(ret_elems) = ret.as_ref() else {
                    panic!("expected a tuple return, got {ret:?}");
                };
                assert_eq!(param_elems.len(), 2);
                assert_eq!(ret_elems.len(), 2);
                assert!(matches!(param_elems[0], Type::Var(_)));
                assert!(matches!(param_elems[1], Type::Var(_)));
                // swapped: ret[0] is param[1], ret[1] is param[0]
                assert_eq!(ret_elems[0], param_elems[1]);
                assert_eq!(ret_elems[1], param_elems[0]);
            }
            other => panic!("expected a function type, got {other:?}"),
        }
    }

    #[test]
    fn tuple_destructuring_param_used_concretely() {
        // `swap((n, true))` returns `(true, n)` — so `y` (the SECOND
        // destructured element), not `x`, carries `n`'s type. Using `y`
        // in a Bool-only context (`if y {...}`) pins `n` concretely to
        // Bool via `swap`'s polymorphic instantiation.
        let types = infer_program(
            "let swap p = match p { (a, b) => (b, a) }\n\
             let use_it n = match swap((n, true)) { (x, y) => if y { 1 } else { 0 } }",
        );
        assert_eq!(types["use_it"], fn_ty(vec![Type::Bool], Type::Int));
    }

    #[test]
    fn unary_neg_on_numeric_types() {
        assert_eq!(infer("-5"), Type::Int);
        assert_eq!(infer("-3.14"), Type::Float);
    }

    #[test]
    fn unary_neg_on_non_numeric_is_an_error() {
        infer_err("-true");
    }

    #[test]
    fn unary_not_on_bool() {
        assert_eq!(infer("!true"), Type::Bool);
    }

    #[test]
    fn unary_not_on_non_bool_is_an_error() {
        infer_err("!5");
    }

    #[test]
    fn arithmetic_on_matching_numeric_types() {
        assert_eq!(infer("1 + 2"), Type::Int);
        assert_eq!(infer("1.5 + 2.5"), Type::Float);
        assert_eq!(infer("10 - 3"), Type::Int);
        assert_eq!(infer("4 * 5"), Type::Int);
    }

    #[test]
    fn arithmetic_on_mismatched_types_is_an_error() {
        infer_err("1 + 1.0");
    }

    #[test]
    fn arithmetic_on_non_numeric_types_is_an_error() {
        infer_err("true + false");
    }

    #[test]
    fn comparison_requires_numeric_and_produces_bool() {
        assert_eq!(infer("3 < 5"), Type::Bool);
        assert_eq!(infer("3.0 <= 5.0"), Type::Bool);
    }

    #[test]
    fn comparison_on_non_numeric_is_an_error() {
        // Would unify fine (Bool == Bool), but `<` specifically
        // requires numeric operands, unlike `==`.
        infer_err("true < false");
    }

    #[test]
    fn equality_works_on_any_matching_type_not_just_numeric() {
        assert_eq!(infer("3 == 3"), Type::Bool);
        assert_eq!(infer("true == false"), Type::Bool);
        assert_eq!(infer("\"a\" == \"a\""), Type::Bool);
    }

    #[test]
    fn equality_on_mismatched_types_is_an_error() {
        infer_err("3 == true");
    }

    #[test]
    fn logical_and_or_require_bool() {
        assert_eq!(infer("true && false"), Type::Bool);
        assert_eq!(infer("true || false"), Type::Bool);
    }

    #[test]
    fn logical_and_on_non_bool_is_an_error() {
        infer_err("1 && 2");
    }

    #[test]
    fn if_with_matching_branches() {
        assert_eq!(infer("if true { 1 } else { 2 }"), Type::Int);
    }

    #[test]
    fn if_with_mismatched_branches_is_an_error() {
        infer_err("if true { 1 } else { true }");
    }

    #[test]
    fn if_condition_must_be_bool() {
        infer_err("if 5 { 1 } else { 2 }");
    }

    #[test]
    fn if_without_else_must_be_unit() {
        infer_err("if true { 1 }");
        assert_eq!(infer("if true { () }"), Type::Unit);
    }

    #[test]
    fn block_let_and_use() {
        assert_eq!(infer("{ let x = 5; x + 1 }"), Type::Int);
    }

    #[test]
    fn block_let_shadowing_later_binding_wins() {
        assert_eq!(infer("{ let x = 1; let x = true; x }"), Type::Bool);
    }

    #[test]
    fn block_discarded_statement_does_not_constrain_result() {
        assert_eq!(infer("{ true; 5 }"), Type::Int);
    }

    #[test]
    fn empty_block_is_unit() {
        assert_eq!(infer("{}"), Type::Unit);
    }

    #[test]
    fn let_type_annotation_checked_against_inferred_type() {
        assert_eq!(infer("{ let x: Int = 5; x }"), Type::Int);
    }

    #[test]
    fn let_type_annotation_mismatch_is_an_error() {
        infer_err("{ let x: Bool = 5; x }");
    }

    // --- Mutation (`let mut` + assignment) ---

    #[test]
    fn assign_infers_as_unit_and_preserves_the_variables_type() {
        assert_eq!(infer("{ let mut x = 5; x = 6; x }"), Type::Int);
    }

    #[test]
    fn assign_value_can_reference_the_current_binding() {
        assert_eq!(infer("{ let mut x = 5; x = x + 1; x }"), Type::Int);
    }

    #[test]
    fn assign_type_mismatch_is_an_error() {
        infer_err("{ let mut x = 5; x = true; x }");
    }

    #[test]
    fn assign_to_an_undefined_variable_is_an_error() {
        infer_err("x = 5");
    }

    #[test]
    fn the_classic_for_loop_accumulator_type_checks() {
        // Same DESIGN.md motivating example proven at the type level:
        // `for`'s loop variable is Int, `sum`'s type is preserved
        // across every reassignment.
        assert_eq!(
            infer("{ let mut sum = 0; for i in 0..5 { sum = sum + i; }; sum }"),
            Type::Int
        );
    }

    #[test]
    fn combined_block_if_arithmetic_comparison() {
        let env = TypeEnv::new().extend("n".to_string(), Type::Int);
        assert_eq!(
            infer_in("{ let doubled = n * 2; if doubled > 10 { doubled } else { 0 } }", &env),
            Type::Int
        );
    }

    // --- Struct literals: need a TypeContext to resolve declared
    // field types, same shape as lower.rs's LoweringContext resolves
    // field ORDER. See context.rs.

    use crate::context::TypeContext;

    fn context(src: &str) -> TypeContext {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        TypeContext::from_items(&program.items).unwrap_or_else(|e| panic!("context error for {src:?}: {e}"))
    }

    fn infer_expr_with(infer: &mut Infer, src: &str, env: &TypeEnv) -> Type {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let (ty, subst) = infer
            .infer_expr(&ast, env)
            .unwrap_or_else(|e| panic!("inference error for {src:?}: {e}"));
        subst.apply(&ty)
    }

    fn infer_expr_with_err(infer: &mut Infer, src: &str, env: &TypeEnv) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        infer
            .infer_expr(&ast, env)
            .expect_err(&format!("expected inference of {src:?} to fail"))
    }

    // --- Variant construction ---

    #[test]
    fn bare_variant_call_infers_the_owning_enum_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Circle(1.0)", &TypeEnv::new()),
            Type::Enum("Shape".to_string(), vec![])
        );
    }

    #[test]
    fn qualified_variant_call_infers_the_owning_enum_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Shape.Circle(1.0)", &TypeEnv::new()),
            Type::Enum("Shape".to_string(), vec![])
        );
    }

    #[test]
    fn variant_call_argument_type_is_checked() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        infer_expr_with_err(&mut infer, "Circle(true)", &TypeEnv::new());
    }

    #[test]
    fn variant_call_wrong_arity_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        infer_expr_with_err(&mut infer, "Circle(1.0, 2.0)", &TypeEnv::new());
    }

    #[test]
    fn bare_zero_arity_variant_infers_the_owning_enum_type() {
        let mut infer = Infer::with_context(context("enum Shape { Empty }"));
        assert_eq!(infer_expr_with(&mut infer, "Empty", &TypeEnv::new()), Type::Enum("Shape".to_string(), vec![]));
    }

    #[test]
    fn bare_non_zero_arity_variant_infers_as_its_constructor_function_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Circle", &TypeEnv::new()),
            fn_ty(vec![Type::Float], Type::Enum("Shape".to_string(), vec![]))
        );
    }

    #[test]
    fn bare_multi_field_variant_infers_a_multi_arg_constructor_function_type() {
        let mut infer = Infer::with_context(context("enum Shape { Rectangle(Float, Float) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Rectangle", &TypeEnv::new()),
            fn_ty(vec![Type::Float, Type::Float], Type::Enum("Shape".to_string(), vec![]))
        );
    }

    #[test]
    fn a_bare_variant_constructor_can_be_used_as_a_higher_order_argument() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend(
            "apply".to_string(),
            fn_ty(vec![fn_ty(vec![Type::Float], Type::Enum("Shape".to_string(), vec![])), Type::Float], Type::Enum("Shape".to_string(), vec![])),
        );
        assert_eq!(infer_expr_with(&mut infer, "apply(Circle, 1.0)", &env), Type::Enum("Shape".to_string(), vec![]));
    }

    #[test]
    fn ordinary_function_calls_are_unaffected_by_variant_inference() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("double".to_string(), fn_ty(vec![Type::Int], Type::Int));
        assert_eq!(infer_expr_with(&mut infer, "double(5)", &env), Type::Int);
    }

    #[test]
    fn variant_construction_and_pattern_round_trip() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "match Circle(1.0) { Circle(r) => r }", &TypeEnv::new()),
            Type::Float
        );
    }

    #[test]
    fn struct_literal_basic() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Point { x: 1.0, y: 2.0 }", &TypeEnv::new()),
            Type::Struct("Point".to_string(), vec![])
        );
    }

    // --- Generic structs/enums ---

    #[test]
    fn generic_struct_literal_infers_its_type_argument_from_field_values() {
        let mut infer = Infer::with_context(context("struct Pair[T] { first: T, second: T }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Pair { first: 1, second: 2 }", &TypeEnv::new()),
            Type::Struct("Pair".to_string(), vec![Type::Int])
        );
    }

    #[test]
    fn generic_struct_requires_the_same_type_argument_across_every_field() {
        let mut infer = Infer::with_context(context("struct Pair[T] { first: T, second: T }"));
        infer_expr_with_err(&mut infer, "Pair { first: 1, second: true }", &TypeEnv::new());
    }

    #[test]
    fn generic_struct_with_independent_type_parameters() {
        let mut infer = Infer::with_context(context("struct Pair[A, B] { first: A, second: B }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Pair { first: 1, second: true }", &TypeEnv::new()),
            Type::Struct("Pair".to_string(), vec![Type::Int, Type::Bool])
        );
    }

    #[test]
    fn generic_struct_field_access_substitutes_the_concrete_argument() {
        let mut infer = Infer::with_context(context("struct Pair[T] { first: T, second: T }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Pair".to_string(), vec![Type::Bool]));
        assert_eq!(infer_expr_with(&mut infer, "p.first", &env), Type::Bool);
    }

    #[test]
    fn user_defined_option_some_and_none_share_the_same_enum_type() {
        // The exact shape DESIGN.md specs for the built-in `Option[T]`
        // — proven here as an ORDINARY user-declared generic enum,
        // since the compiler doesn't inject a real builtin yet.
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Some(5)", &TypeEnv::new()),
            Type::Enum("Option".to_string(), vec![Type::Int])
        );
    }

    #[test]
    fn bare_none_alone_has_an_unconstrained_type_argument() {
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        let ty = infer_expr_with(&mut infer, "None", &TypeEnv::new());
        match ty {
            Type::Enum(name, args) => {
                assert_eq!(name, "Option");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0], Type::Var(_)), "expected an unconstrained var, got {:?}", args[0]);
            }
            other => panic!("expected an Enum type, got {other:?}"),
        }
    }

    #[test]
    fn generic_variant_pattern_binds_the_instantiated_payload_type() {
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        let env = TypeEnv::new().extend("o".to_string(), Type::Enum("Option".to_string(), vec![Type::Bool]));
        assert_eq!(
            infer_expr_with(&mut infer, "match o { Some(x) => x, None => false }", &env),
            Type::Bool
        );
    }

    #[test]
    fn mismatched_generic_arguments_are_a_type_error() {
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        let env = TypeEnv::new().extend("o".to_string(), Type::Enum("Option".to_string(), vec![Type::Int]));
        infer_expr_with_err(&mut infer, "match o { Some(x) => x, None => true }", &env);
    }

    #[test]
    fn a_bare_variant_constructor_of_a_generic_enum_is_a_function_value() {
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        let ty = infer_expr_with(&mut infer, "Some", &TypeEnv::new());
        match ty {
            Type::Function(params, ret) => {
                assert_eq!(params.len(), 1);
                assert!(matches!(params[0], Type::Var(_)));
                assert!(matches!(*ret, Type::Enum(ref name, _) if name == "Option"));
            }
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    #[test]
    fn generic_type_annotation_is_checked_against_the_expression() {
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        let ty = infer_expr_with(&mut infer, "{ let x: Option[Int] = Some(5); x }", &TypeEnv::new());
        assert_eq!(ty, Type::Enum("Option".to_string(), vec![Type::Int]));
    }

    #[test]
    fn generic_type_annotation_mismatch_is_an_error() {
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        infer_expr_with_err(&mut infer, "{ let x: Option[Bool] = Some(5); x }", &TypeEnv::new());
    }

    #[test]
    fn struct_literal_field_order_independent() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Point { y: 2.0, x: 1.0 }", &TypeEnv::new()),
            Type::Struct("Point".to_string(), vec![])
        );
    }

    #[test]
    fn struct_literal_field_type_mismatch_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        infer_expr_with_err(&mut infer, "Point { x: 1, y: 2.0 }", &TypeEnv::new());
    }

    #[test]
    fn struct_literal_unknown_type_is_an_error() {
        let mut infer = Infer::new();
        infer_expr_with_err(&mut infer, "Foo { x: 1.0 }", &TypeEnv::new());
    }

    #[test]
    fn struct_literal_missing_field_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        infer_expr_with_err(&mut infer, "Point { x: 1.0 }", &TypeEnv::new());
    }

    #[test]
    fn struct_literal_unknown_field_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        infer_expr_with_err(&mut infer, "Point { x: 1.0, y: 2.0, z: 3.0 }", &TypeEnv::new());
    }

    #[test]
    fn struct_literal_duplicate_field_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        infer_expr_with_err(&mut infer, "Point { x: 1.0, x: 2.0 }", &TypeEnv::new());
    }

    #[test]
    fn struct_literal_spread_infers_the_struct_type() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Point".to_string(), vec![]));
        let ty = infer_expr_with(&mut infer, "Point { x: 1.0, ..other }", &env);
        assert_eq!(ty, Type::Struct("Point".to_string(), vec![]));
    }

    #[test]
    fn struct_literal_spread_requires_the_spread_expr_to_be_the_same_struct() {
        let mut infer = Infer::with_context(context(
            "struct Point { x: Float, y: Float }\nstruct Color { r: Int, g: Int, b: Int }",
        ));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Color".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "Point { x: 1.0, ..other }", &env);
    }

    #[test]
    fn struct_literal_spread_still_checks_explicit_field_types() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "Point { x: true, ..other }", &env);
    }

    #[test]
    fn struct_literal_spread_still_rejects_an_unknown_field() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "Point { z: 1.0, ..other }", &env);
    }

    // --- Field access (`p.x`) ---

    #[test]
    fn field_access_infers_the_declared_field_type() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Bool }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "p.x", &env), Type::Float);
        assert_eq!(infer_expr_with(&mut infer, "p.y", &env), Type::Bool);
    }

    #[test]
    fn field_access_records_the_owning_struct_by_span() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with(&mut infer, "p.x", &env);
        assert_eq!(infer.field_owners().len(), 1);
        assert_eq!(infer.field_owners().values().next().unwrap(), "Point");
    }

    #[test]
    fn field_access_on_an_unknown_field_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "p.z", &env);
    }

    #[test]
    fn field_access_on_a_non_struct_is_an_error() {
        let mut infer = Infer::new();
        let env = TypeEnv::new().extend("n".to_string(), Type::Int);
        infer_expr_with_err(&mut infer, "n.x", &env);
    }

    #[test]
    fn field_access_on_a_struct_literal_directly() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        assert_eq!(
            infer_expr_with(&mut infer, "(Point { x: 1.0, y: 2.0 }).x", &TypeEnv::new()),
            Type::Float
        );
    }

    #[test]
    fn field_access_referencing_another_struct_field_type() {
        let mut infer = Infer::with_context(context(
            "struct Point { x: Int, y: Int }\nstruct Line { start: Point, end: Point }",
        ));
        let env = TypeEnv::new().extend("l".to_string(), Type::Struct("Line".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "l.start", &env), Type::Struct("Point".to_string(), vec![]));
    }

    // --- Match: enum variant patterns, resolved against the SAME
    // TypeContext (enum variant tag -> owning enum + payload types).

    #[test]
    fn match_variant_arms_produce_a_common_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Rectangle(Float, Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(
                &mut infer,
                "match shape { Shape.Circle(r) => r, Shape.Rectangle(w, h) => w }",
                &env
            ),
            Type::Float
        );
    }

    #[test]
    fn match_scrutinee_type_is_inferred_from_the_arms() {
        // `x`'s type isn't known ahead of time (a fresh var) — matching
        // it against Shape's variants is what pins it down.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("x".to_string(), Type::Var(0));
        assert_eq!(
            infer_expr_with(&mut infer, "match x { Shape.Circle(r) => r }", &env),
            Type::Float
        );
    }

    #[test]
    fn match_can_destructure_a_struct_by_tag_like_a_variant() {
        // lower.rs erases the struct-vs-enum-variant distinction: both
        // `Point(x, y)` against a struct and `Shape.Circle(r)` against
        // an enum become the same positional `Ctor`/`Match` by tag. The
        // type checker needs to accept the same syntax, falling back to
        // struct_fields when a tag isn't a registered enum variant.
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point(x, y) => x }", &env), Type::Float);
    }

    // --- Struct patterns (`Point { x, y }`), as opposed to the
    // variant-call-syntax fallback above (`Point(x, y)`) ---

    #[test]
    fn match_arm_struct_pattern() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point { x, y } => x }", &env), Type::Float);
    }

    #[test]
    fn match_arm_struct_pattern_field_rename() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match p { Point { x: px, y: py } => px + py }", &env),
            Type::Float
        );
    }

    #[test]
    fn match_arm_struct_pattern_with_rest() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point { x, .. } => x }", &env), Type::Float);
    }

    #[test]
    fn match_arm_struct_pattern_missing_field_without_rest_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match p { Point { x } => x }", &env);
    }

    #[test]
    fn match_arm_struct_pattern_unknown_field_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match p { Point { x, y, z } => x }", &env);
    }

    #[test]
    fn match_arm_struct_pattern_unknown_struct_is_an_error() {
        let mut infer = Infer::new();
        let env = TypeEnv::new().extend("p".to_string(), Type::Var(0));
        infer_expr_with_err(&mut infer, "match p { Point { x, y } => x }", &env);
    }

    #[test]
    fn block_let_struct_destructure() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "{ let Point { x, y } = p; x + y }", &env), Type::Float);
    }

    #[test]
    fn block_let_struct_destructure_type_annotation_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "{ let Point { x, y }: Point = p; x }", &env);
    }

    #[test]
    fn struct_destructuring_function_param() {
        let types = infer_program("struct Point { x: Float, y: Float }\nlet area (Point { x, y }) = x * y");
        assert_eq!(types["area"], fn_ty(vec![Type::Struct("Point".to_string(), vec![])], Type::Float));
    }

    // --- Nested patterns ---

    #[test]
    fn struct_nested_inside_tuple_match_arm() {
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("pair".to_string(), Type::Tuple(vec![Type::Struct("Point".to_string(), vec![]), Type::Int]));
        assert_eq!(
            infer_expr_with(&mut infer, "match pair { (Point { x, y }, n) => x + y + n }", &env),
            Type::Int
        );
    }

    #[test]
    fn struct_nested_inside_struct_match_arm() {
        // Previously untestable here — needs a struct declared with a
        // FIELD whose type is another struct (`Line { start: Point,
        // end: Point }`), which depended on `ast_type_to_type`
        // resolving a struct/enum-valued field type. Now that that gap
        // is closed (see context.rs), this is real coverage, not just
        // struct-in-tuple/variant-in-tuple standing in for it.
        let ctx = context("struct Point { x: Int, y: Int }\nstruct Line { start: Point, end: Point }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("l".to_string(), Type::Struct("Line".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(
                &mut infer,
                "match l { Line { start: Point { x, .. }, end: Point { x: x2, .. } } => x2 - x }",
                &env
            ),
            Type::Int
        );
    }

    #[test]
    fn variant_pattern_nested_inside_tuple_match_arm() {
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("pair".to_string(), Type::Tuple(vec![Type::Struct("Point".to_string(), vec![]), Type::Int]));
        assert_eq!(
            infer_expr_with(&mut infer, "match pair { (Point(x, y), n) => x + y + n }", &env),
            Type::Int
        );
    }

    #[test]
    fn deeply_nested_pattern_three_levels() {
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend(
            "v".to_string(),
            Type::Tuple(vec![Type::Tuple(vec![Type::Struct("Point".to_string(), vec![]), Type::Int]), Type::Int]),
        );
        assert_eq!(
            infer_expr_with(&mut infer, "match v { ((Point { x, y }, a), b) => x + y + a + b }", &env),
            Type::Int
        );
    }

    #[test]
    fn nested_pattern_type_mismatch_is_an_error() {
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("pair".to_string(), Type::Tuple(vec![Type::Struct("Point".to_string(), vec![]), Type::Int]));
        // Second tuple position is Int, not a Point — the NESTED
        // destructure should fail, not be silently accepted.
        infer_expr_with_err(&mut infer, "match pair { (n, Point { x, y }) => x }", &env);
    }

    #[test]
    fn nested_tuple_destructuring_function_param() {
        // A tuple-in-tuple destructuring param, proving nesting flows
        // through `infer_program`'s param-binding loop too, not just
        // match arms and block-level `let`.
        let types = infer_program("let f ((a, b), c) = a + b + c");
        assert_eq!(types["f"], fn_ty(vec![Type::Tuple(vec![Type::Tuple(vec![Type::Int, Type::Int]), Type::Int])], Type::Int));
    }

    #[test]
    fn nested_struct_destructuring_function_param() {
        let types = infer_program(
            "struct Point { x: Int, y: Int }\n\
             struct Line { start: Point, end: Point }\n\
             let dx (Line { start: Point { x: x0, .. }, end: Point { x: x1, .. } }) = x1 - x0",
        );
        assert_eq!(types["dx"], fn_ty(vec![Type::Struct("Line".to_string(), vec![])], Type::Int));
    }

    #[test]
    fn nested_or_pattern_is_still_not_yet_supported() {
        // Nesting works for tag-based patterns (variant/tuple/struct)
        // — an or-pattern nested inside one is a genuinely separate,
        // still-unsupported gap, mirroring lower.rs's identical
        // restriction.
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match p { Point { x: 1 | 2, y } => y }", &env);
    }

    #[test]
    fn match_mixing_a_struct_and_an_unrelated_enum_is_an_error() {
        let mut infer =
            Infer::with_context(context("struct Point { x: Float, y: Float }\nenum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("x".to_string(), Type::Var(0));
        infer_expr_with_err(
            &mut infer,
            "match x { Point(a, b) => a, Shape.Circle(r) => r }",
            &env,
        );
    }

    #[test]
    fn match_arms_must_produce_the_same_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Rectangle(Float, Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(
            &mut infer,
            "match shape { Shape.Circle(r) => r, Shape.Rectangle(w, h) => true }",
            &env,
        );
    }

    #[test]
    fn match_unknown_variant_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Shape.Triangle(a) => a }", &env);
    }

    #[test]
    fn match_variant_wrong_arity_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(a, b) => a }", &env);
    }

    #[test]
    fn match_mixing_variants_from_different_enums_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }\nenum Color { Red, Blue }"));
        let env = TypeEnv::new().extend("x".to_string(), Type::Var(0));
        infer_expr_with_err(&mut infer, "match x { Shape.Circle(r) => 1, Color.Red => 2 }", &env);
    }

    #[test]
    fn match_single_bare_wildcard_arm_works_for_any_scrutinee_type() {
        // A LONE catch-all arm needs no tag inspection at all, so it
        // works for ANY scrutinee type, including an enum — `_` used
        // INSIDE a variant's args (e.g. `Shape.Rectangle(_, _)`) has
        // always worked; this is the WHOLE-arm case, now supported too.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match shape { _ => 1 }", &env), Type::Int);
    }

    #[test]
    fn match_trailing_wildcard_mixed_into_a_tag_shaped_match_now_works() {
        // A catch-all in the LAST position, mixed among otherwise
        // Ctor-tag-shaped arms, is now supported — see lower.rs's
        // `DEFAULT_ARM_TAG` sentinel.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match shape { Circle(r) => r, _ => 0.0 }", &env), Type::Float);
    }

    #[test]
    fn match_trailing_ident_mixed_into_a_tag_shaped_match_binds_the_whole_scrutinee() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match shape { Circle(r) => r, other => 0.0 }", &env),
            Type::Float
        );
    }

    #[test]
    fn match_wildcard_mixed_into_a_tag_shaped_match_in_a_non_last_position_is_still_an_error() {
        // A catch-all ANYWHERE except the last position still has no
        // lowering — only the trailing case desugars to a `DEFAULT_
        // ARM_TAG` sentinel arm.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { _ => 0.0, Circle(r) => r }", &env);
    }

    #[test]
    fn match_or_pattern_with_consistent_bindings_infers_correctly() {
        let mut infer = Infer::with_context(context(
            "enum Shape { Circle(Float), Square(Float), Triangle(Int) }",
        ));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match shape { Circle(v) | Square(v) => v, Triangle(n) => 0.0 }", &env),
            Type::Float
        );
    }

    #[test]
    fn match_or_pattern_with_a_guard_infers_correctly() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match shape { Circle(v) | Square(v) if v > 0.0 => v, _ => 0.0 }", &env),
            Type::Float
        );
    }

    #[test]
    fn match_or_pattern_with_mismatched_binding_names_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Circle(v) | Square(w) => v }", &env);
    }

    #[test]
    fn match_or_pattern_with_inconsistent_binding_types_is_an_error() {
        // `v` would need to be both `Float` (from Circle) and `Int`
        // (from Square) — a real type error, not something silently
        // resolved to one or the other.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Int) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Circle(v) | Square(v) => v }", &env);
    }

    #[test]
    fn match_or_pattern_across_different_enums_is_an_error() {
        let mut infer =
            Infer::with_context(context("enum Shape { Circle(Float) }\nenum Color { Red(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Circle(v) | Red(v) => v }", &env);
    }

    #[test]
    fn match_or_pattern_with_a_nested_sub_pattern_is_not_yet_supported() {
        let mut infer = Infer::with_context(context(
            "struct Point { x: Int, y: Int }\nenum Shape { A(Point), B(Point) }",
        ));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { A(Point { x, y }) | B(Point { x, y }) => x }", &env);
    }

    // --- Match exhaustiveness ---

    #[test]
    fn a_match_missing_a_variant_is_rejected() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        let err = infer_expr_with_err(&mut infer, "match shape { Circle(r) => r }", &env);
        assert!(err.contains("not exhaustive"), "expected an exhaustiveness error, got: {err}");
        assert!(err.contains("Square"), "expected the missing variant named, got: {err}");
    }

    #[test]
    fn a_match_covering_every_variant_is_accepted() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match shape { Circle(r) => r, Square(r) => r }", &env),
            Type::Float
        );
    }

    #[test]
    fn a_match_with_a_trailing_catchall_is_exempt_from_the_check() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match shape { Circle(r) => r, _ => 0.0 }", &env), Type::Float);
    }

    #[test]
    fn an_or_pattern_covering_the_remaining_variants_satisfies_exhaustiveness() {
        let mut infer =
            Infer::with_context(context("enum Shape { Circle(Float), Square(Float), Triangle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match shape { Circle(r) => r, Square(r) | Triangle(r) => r }", &env),
            Type::Float
        );
    }

    #[test]
    fn a_guarded_arm_still_counts_as_covering_its_variant() {
        // Deliberate, discussed choice — see `infer_match`'s own doc
        // comment on this exact tradeoff.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Square(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        assert_eq!(
            infer_expr_with(&mut infer, "match shape { Circle(r) if r > 0.0 => r, Square(r) => r }", &env),
            Type::Float
        );
    }

    #[test]
    fn multiple_missing_variants_are_all_named() {
        let mut infer =
            Infer::with_context(context("enum Shape { Circle(Float), Square(Float), Triangle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        let err = infer_expr_with_err(&mut infer, "match shape { Circle(r) => r }", &env);
        assert!(err.contains("Square"), "expected Square named as missing, got: {err}");
        assert!(err.contains("Triangle"), "expected Triangle named as missing, got: {err}");
    }

    #[test]
    fn struct_matches_need_no_exhaustiveness_check() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string(), vec![]));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point { x, y } => x + y }", &env), Type::Int);
    }

    #[test]
    fn option_match_missing_none_is_rejected() {
        // The prelude's `Option`/`Result` are ordinary enums under the
        // hood (see DESIGN.md's "Pattern grammar" section) — this
        // check applies to them exactly like any user-declared enum,
        // with no special-casing needed.
        let mut infer = Infer::with_context(context("enum Option[T] { Some(T), None }"));
        let env = TypeEnv::new().extend("opt".to_string(), Type::Enum("Option".to_string(), vec![Type::Int]));
        let err = infer_expr_with_err(&mut infer, "match opt { Some(x) => x }", &env);
        assert!(err.contains("not exhaustive"), "expected an exhaustiveness error, got: {err}");
    }

    #[test]
    fn match_over_int_literals_with_a_trailing_wildcard_infers_correctly() {
        assert_eq!(infer("match 2 { 1 => \"one\", 2 => \"two\", _ => \"many\" }"), Type::Str);
    }

    #[test]
    fn match_over_string_literals_with_a_trailing_ident_infers_correctly() {
        assert_eq!(infer("match \"b\" { \"a\" => 1, \"b\" => 2, other => 0 }"), Type::Int);
    }

    #[test]
    fn match_over_bool_literals_requires_the_scrutinee_to_be_bool() {
        infer_err("match 5 { true => 1, false => 2, _ => 0 }");
    }

    #[test]
    fn literal_match_without_a_trailing_catchall_is_an_error() {
        infer_err("match 2 { 1 => \"one\", 2 => \"two\" }");
    }

    #[test]
    fn literal_match_with_a_guarded_trailing_arm_is_an_error() {
        infer_err("match 2 { 1 => \"one\", n if n > 0 => \"positive\" }");
    }

    #[test]
    fn literal_match_arms_must_produce_the_same_type() {
        infer_err("match 2 { 1 => \"one\", _ => 2 }");
    }

    #[test]
    fn literal_match_arm_guards_are_checked_against_bool() {
        infer_err("match 2 { 1 if 5 => \"one\", _ => \"other\" }");
    }

    #[test]
    fn match_or_pattern_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Empty }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(a) | Shape.Empty => 1 }", &env);
    }

    #[test]
    fn match_guard_infers_using_the_arms_own_bindings() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        let ty = infer_expr_with(&mut infer, "match shape { Shape.Circle(r) if r > 0.0 => r, Shape.Circle(r) => 0.0 }", &env);
        assert_eq!(ty, Type::Float);
    }

    #[test]
    fn match_guard_must_be_a_bool() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string(), vec![]));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(r) if r => r, Shape.Circle(r) => 0.0 }", &env);
    }

    #[test]
    fn match_guard_combined_with_a_nested_pattern_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let env = TypeEnv::new().extend(
            "p".to_string(),
            Type::Tuple(vec![Type::Struct("Point".to_string(), vec![]), Type::Int]),
        );
        infer_expr_with_err(
            &mut infer,
            "match p { (Point { x, y }, n) if x > 0 => n, (Point { x, y }, n) => n }",
            &env,
        );
    }

    // --- Closures: unlike named top-level functions (which get a
    // totally fresh, isolated environment — see plum-interp's
    // `function_body_cannot_see_the_caller_environment`), a closure
    // CAN see the surrounding scope. That's the actual definition of a
    // closure, and it's a deliberate difference from function
    // inference, not an oversight.

    #[test]
    fn closure_annotated_param() {
        assert_eq!(infer("|x: Int| x + 1"), fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn closure_unannotated_param_inferred_from_body() {
        // No annotation on `x` — its type comes entirely from how it's
        // used inside the body (here, `+ 1` pins it to Int, same
        // defaulting rule as everywhere else).
        assert_eq!(infer("|x| x + 1"), fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn closure_multiple_params() {
        assert_eq!(infer("|a, b| a + b"), fn_ty(vec![Type::Int, Type::Int], Type::Int));
    }

    #[test]
    fn closure_can_see_the_surrounding_scope() {
        let env = TypeEnv::new().extend("outer".to_string(), Type::Int);
        assert_eq!(infer_in("|x| x + outer", &env), fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn a_named_function_can_take_a_closure_argument_and_call_it() {
        // `infer_call` doesn't special-case what the callee expression
        // IS — it just infers a type and unifies against a Function
        // shape, so `f(x)` inside `apply`'s body works identically
        // whether `f` names a top-level function or a closure-typed
        // parameter. This is the type-level counterpart to
        // plum-interp's `a_named_function_can_receive_a_closure_argument_and_call_it`.
        let types = infer_program(
            "let apply f x = f(x)\n\
             let use_it n = apply(|x| x + 1, n)",
        );
        assert_eq!(types["use_it"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn pipe_desugars_exactly_like_lower_rs_does() {
        // `x |> f` and `x |> f(a)` type-check by desugaring the same
        // way lower.rs's `lower_pipe` does at the IR level — proven
        // here at the type level, independently of lowering.
        let env = TypeEnv::new().extend(
            "f".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Bool)),
        );
        assert_eq!(infer_in("5 |> f", &env), Type::Bool);

        let env2 = TypeEnv::new().extend(
            "f".to_string(),
            Type::Function(vec![Type::Int, Type::Int], Box::new(Type::Bool)),
        );
        // `5 |> f(1)` desugars to `f(1, 5)` — piped value appended LAST.
        assert_eq!(infer_in("5 |> f(1)", &env2), Type::Bool);
    }

    // --- Call expressions, against a manually-populated env (no
    // program-level machinery needed to test this in isolation) ---

    #[test]
    fn call_with_matching_argument_types() {
        let env = TypeEnv::new().extend(
            "f".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Bool)),
        );
        assert_eq!(infer_in("f(5)", &env), Type::Bool);
    }

    #[test]
    fn call_with_wrong_argument_type_is_an_error() {
        let env = TypeEnv::new().extend(
            "f".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Bool)),
        );
        infer_err_in("f(true)", &env);
    }

    #[test]
    fn call_with_wrong_arity_is_an_error() {
        let env = TypeEnv::new().extend(
            "f".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Bool)),
        );
        infer_err_in("f(1, 2)", &env);
    }

    #[test]
    fn calling_a_non_function_is_an_error() {
        let env = TypeEnv::new().extend("x".to_string(), Type::Int);
        infer_err_in("x(1)", &env);
    }

    #[test]
    fn unconstrained_numeric_operator_defaults_to_int() {
        // No typeclass/constraint machinery exists — a real system
        // would infer `Num a => a -> a -> a` here. This is a
        // deliberate, documented simplification: when a numeric
        // operator's operand is still an unresolved type variable
        // (nothing else pinned it), default it to Int, the same
        // spirit as Rust defaulting an ambiguous integer literal.
        let env = TypeEnv::new()
            .extend("a".to_string(), Type::Var(0))
            .extend("b".to_string(), Type::Var(1));
        assert_eq!(infer_in("a + b", &env), Type::Int);
    }

    // --- Program-level inference: function signatures, including
    // self- and mutual recursion, pre-declared with fresh type
    // variables before any body is inferred — this is what lets a
    // recursive call type-check at all.

    // Builds the TypeContext from the SOURCE ITSELF (mirroring what
    // `plumc::typecheck_and_run` actually does) rather than an empty
    // one — needed for any program-level test whose struct/enum
    // declarations live in `src`, not passed in separately.
    fn infer_program(src: &str) -> HashMap<String, Type> {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = crate::context::TypeContext::from_items(&program.items)
            .unwrap_or_else(|e| panic!("context error for {src:?}: {e}"));
        let mut infer = Infer::with_context(ctx);
        infer
            .infer_program(&program)
            .unwrap_or_else(|e| panic!("program inference error for {src:?}: {e}"))
    }

    fn infer_program_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = crate::context::TypeContext::from_items(&program.items)
            .unwrap_or_else(|e| panic!("context error for {src:?}: {e}"));
        let mut infer = Infer::with_context(ctx);
        infer
            .infer_program(&program)
            .expect_err(&format!("expected inference of {src:?} to fail"))
    }

    fn fn_ty(params: Vec<Type>, ret: Type) -> Type {
        Type::Function(params, Box::new(ret))
    }

    #[test]
    fn infer_program_simple_function() {
        let types = infer_program("let double n = n * 2");
        assert_eq!(types["double"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn function_type_annotation_on_a_parameter_type_checks() {
        // A real, previously-missing gap this closes as a side effect
        // of implementing `(A, B) -> R` type-annotation syntax for
        // extern callbacks — see ast.rs's `Type::Function` doc comment.
        let types = infer_program(
            "let apply (f: (Int) -> Int) (x: Int): Int = f(x)\nlet double n = n * 2\nlet result = apply(double, 5)",
        );
        assert_eq!(types["result"], Type::Int);
    }

    #[test]
    fn function_type_annotation_with_wrong_argument_type_is_an_error() {
        infer_program_err("let apply (f: (Int) -> Int) (x: Bool): Int = f(x)");
    }

    #[test]
    fn infer_program_defaults_fully_generic_arithmetic_to_int() {
        let types = infer_program("let add a b = a + b");
        assert_eq!(types["add"], fn_ty(vec![Type::Int, Type::Int], Type::Int));
    }

    #[test]
    fn infer_program_self_recursion() {
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        let types = infer_program(src);
        assert_eq!(types["sum"], fn_ty(vec![Type::Int, Type::Int], Type::Int));
    }

    #[test]
    fn a_self_referential_closure_global_infers_correctly() {
        let src = "let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }";
        let types = infer_program(src);
        assert_eq!(types["fib"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn a_self_referential_local_closure_infers_correctly() {
        let src = "let use_it dummy = { let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }; fib(5) }";
        let types = infer_program(src);
        let Type::Function(_, ret) = &types["use_it"] else {
            panic!("expected a function type, got {:?}", types["use_it"]);
        };
        assert_eq!(*ret.as_ref(), Type::Int);
    }

    #[test]
    fn infer_program_cross_function_calls() {
        let src = "let square x = x * x\nlet sum_of_squares a b = square(a) + square(b)";
        let types = infer_program(src);
        assert_eq!(types["square"], fn_ty(vec![Type::Int], Type::Int));
        assert_eq!(types["sum_of_squares"], fn_ty(vec![Type::Int, Type::Int], Type::Int));
    }

    #[test]
    fn declared_return_type_matching_the_body_is_accepted() {
        let src = "let f x: Int = x + 1";
        let types = infer_program(src);
        assert_eq!(types["f"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn declared_return_type_conflicting_with_the_body_is_an_error() {
        // Was PREVIOUSLY silently accepted — `ret_ty` was parsed but
        // never consulted by inference at all.
        let err = infer_program_err("let f x: Bool = x + 1");
        assert!(err.contains("return type"), "expected a return-type error, got: {err}");
    }

    #[test]
    fn declared_return_type_constrains_an_otherwise_generic_body() {
        // Without consulting `ret_ty`, `identity`'s body alone gives no
        // reason to pick Bool over any other type — the annotation is
        // the ONLY source of that constraint.
        let src = "let f x: Bool = x";
        let types = infer_program(src);
        assert_eq!(types["f"], fn_ty(vec![Type::Bool], Type::Bool));
    }

    #[test]
    fn declared_return_type_referencing_a_struct_is_accepted_when_it_matches() {
        let src = "struct Point { x: Int, y: Int }\nlet origin dummy: Point = Point { x: 0, y: 0 }";
        let types = infer_program(src);
        let (_, ret) = match &types["origin"] {
            Type::Function(params, ret) => (params.clone(), (**ret).clone()),
            other => panic!("expected a function type, got {other:?}"),
        };
        assert_eq!(ret, Type::Struct("Point".to_string(), vec![]));
    }

    #[test]
    fn declared_return_type_referencing_the_wrong_struct_is_an_error() {
        let src = "struct Point { x: Int, y: Int }\nlet origin dummy: Int = Point { x: 0, y: 0 }";
        infer_program_err(src);
    }

    // --- Function parameter type annotations ---

    #[test]
    fn a_matching_parameter_annotation_is_accepted() {
        let types = infer_program("let f (x: Int) = x + 1");
        assert_eq!(types["f"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn a_mismatched_parameter_annotation_is_an_error() {
        // Previously silently accepted — the annotation was parsed but
        // never consulted, so `x` would just get a fresh, unconstrained
        // var regardless of what the annotation said. The annotation
        // itself always unifies fine against `x`'s still-fresh var (an
        // unconstrained var accepts anything); the actual conflict
        // correctly surfaces once the BODY tries to use the now-Bool
        // `x` numerically — proving the annotation genuinely took
        // effect rather than being silently dropped.
        infer_program_err("let f (x: Bool) = x + 1");
    }

    #[test]
    fn a_parameter_annotation_constrains_an_otherwise_generic_body() {
        // Nothing else in the body pins `x`'s type — the annotation is
        // the ONLY source of that constraint, same proof shape as the
        // earlier `ret_ty` gap's equivalent test.
        let types = infer_program("let f (x: Bool) = x");
        assert_eq!(types["f"], fn_ty(vec![Type::Bool], Type::Bool));
    }

    #[test]
    fn a_struct_typed_parameter_annotation_is_checked() {
        let src = "struct Point { x: Int, y: Int }\nlet dx (p: Point) = match p { Point(a, b) => a }";
        let types = infer_program(src);
        assert_eq!(types["dx"], fn_ty(vec![Type::Struct("Point".to_string(), vec![])], Type::Int));
    }

    #[test]
    fn a_second_parameter_annotation_is_also_checked() {
        infer_program_err("let f (x: Int) (y: Bool) = x + y");
    }

    #[test]
    fn an_array_typed_parameter_annotation_is_checked() {
        // `Array`/`Task`/`Sender`/`Receiver` are opaque pseudo-generic
        // builtins, deliberately never registered in `TypeContext` —
        // `resolve_annotation` needs its own fixed-arity-one case for
        // them rather than the ordinary ctx-registered-declaration path
        // real structs/enums go through.
        let types = infer_program("let f (arr: Array[Int]) = arr.len()");
        assert_eq!(
            types["f"],
            fn_ty(vec![Type::Struct("Array".to_string(), vec![Type::Int])], Type::Int)
        );
    }

    #[test]
    fn an_array_typed_parameter_annotation_constrains_the_body() {
        infer_program_err("let f (arr: Array[Bool]) = arr.push(1)");
    }

    #[test]
    fn an_array_typed_return_annotation_is_checked() {
        let types = infer_program("let f (arr: Array[Int]): Array[Int] = arr");
        assert_eq!(
            types["f"],
            fn_ty(
                vec![Type::Struct("Array".to_string(), vec![Type::Int])],
                Type::Struct("Array".to_string(), vec![Type::Int]),
            )
        );
    }

    #[test]
    fn a_mismatched_array_typed_return_annotation_is_an_error() {
        infer_program_err("let f (arr: Array[Int]): Array[Bool] = arr");
    }

    #[test]
    fn array_generic_annotation_arity_is_checked() {
        infer_program_err("let f (arr: Array[Int, Bool]) = arr");
    }

    #[test]
    fn an_unannotated_parameter_alongside_an_annotated_one_still_infers_normally() {
        let types = infer_program("let f x (y: Int) = x + y");
        assert_eq!(types["f"], fn_ty(vec![Type::Int, Type::Int], Type::Int));
    }

    #[test]
    fn a_destructuring_parameter_annotation_mismatched_with_its_own_pattern_is_an_error() {
        // `Point { x, y }` already implies `Point` — an explicit `:
        // Color` annotation on top of it is a real, checked
        // contradiction, not just redundant.
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Color { r: Int, g: Int, b: Int }\n\
                    let sum_of (Point { x, y }: Color) = x + y";
        infer_program_err(src);
    }

    // --- Function-level generic annotations ---

    #[test]
    fn a_generic_parameter_annotation_infers_a_polymorphic_identity() {
        let types = infer_program("let identity[T] (x: T): T = x");
        match &types["identity"] {
            Type::Function(params, ret) => {
                assert_eq!(params.len(), 1);
                assert_eq!(&params[0], ret.as_ref());
            }
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    #[test]
    fn a_generic_identity_is_still_usable_polymorphically_at_each_call_site() {
        // Proves the fresh-var-per-generic-name mechanism doesn't
        // accidentally pin `identity` to ONE concrete type — ordinary
        // let-polymorphism (`generalize`/`instantiate`) still applies
        // to the RESULT of inferring its (now-annotated) signature.
        let src = "let identity[T] (x: T): T = x\n\
                    let use_it dummy = { let a = identity(1); let b = identity(true); a }";
        let types = infer_program(src);
        match &types["use_it"] {
            Type::Function(params, ret) => {
                assert_eq!(params.len(), 1);
                assert!(matches!(params[0], Type::Var(_)), "expected dummy to stay unconstrained, got {:?}", params[0]);
                assert_eq!(**ret, Type::Int);
            }
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    #[test]
    fn two_parameters_sharing_a_generic_name_must_share_one_type() {
        let types = infer_program("let pair[T] (a: T) (b: T): T = a");
        match &types["pair"] {
            Type::Function(params, ret) => {
                assert_eq!(params.len(), 2);
                assert_eq!(&params[0], &params[1]);
                assert_eq!(&params[0], ret.as_ref());
            }
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    #[test]
    fn two_parameters_sharing_a_generic_name_reject_mismatched_arguments_at_the_call_site() {
        let src = "let pair[T] (a: T) (b: T): T = a\nlet use_it dummy = pair(1, true)";
        infer_program_err(src);
    }

    #[test]
    fn a_generic_annotation_referencing_a_generic_struct_resolves() {
        let src = "struct Pair[A, B] { first: A, second: B }\n\
                    let wrap[T] (x: T): Pair[T, T] = Pair { first: x, second: x }";
        let types = infer_program(src);
        // Only the SHAPE (both args equal each other, both are Vars,
        // and both match the param) matters — the exact id isn't
        // meaningful, so check structurally rather than hardcoding one.
        match &types["wrap"] {
            Type::Function(params, ret) => match ret.as_ref() {
                Type::Struct(name, args) => {
                    assert_eq!(name, "Pair");
                    assert_eq!(args.len(), 2);
                    assert_eq!(&args[0], &args[1]);
                    assert_eq!(&args[0], &params[0]);
                }
                other => panic!("expected a Struct return type, got {other:?}"),
            },
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    // --- Function generic bounds (`[T: Num]`), checked at call sites ---

    #[test]
    fn a_bound_satisfying_call_site_is_accepted() {
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(5)";
        let types = infer_program(src);
        match &types["use_it"] {
            Type::Function(_, ret) => assert_eq!(**ret, Type::Int),
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    #[test]
    fn a_bound_violating_call_site_is_an_error() {
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(true)";
        let err = infer_program_err(src);
        assert!(err.contains("Num"), "expected a Num-bound error, got: {err}");
    }

    #[test]
    fn a_generic_still_unresolved_at_definition_time_is_checked_later_at_the_call_site() {
        // Nothing inside `f`'s OWN body pins `T` to anything — the
        // bound can only be checked once a REAL call supplies a
        // concrete argument.
        let ok_src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(5)";
        infer_program(ok_src);
        let err_src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = f(true)";
        infer_program_err(err_src);
    }

    #[test]
    fn a_generic_already_pinned_inside_the_functions_own_body_is_checked_immediately() {
        // `x + 1` pins `T = Int` before any call site even exists —
        // checked right at generalization time, not deferred.
        infer_program("let f[T: Num] (x: T): T = x + 1");
        let err = infer_program_err("let f[T: Num] (x: T) = x && true");
        assert!(err.contains("Num"), "expected a Num-bound error, got: {err}");
    }

    #[test]
    fn a_bounded_generic_function_stays_genuinely_polymorphic_across_call_sites() {
        // Proves bound-tracking doesn't accidentally pin `f` to ONE
        // concrete type the way a mistaken implementation might —
        // ordinary let-polymorphism still applies as long as EVERY
        // call site's argument satisfies the bound.
        let src = "let f[T: Num] (x: T): T = x\nlet use_it dummy = { let a = f(1); let b = f(2.5); a }";
        let types = infer_program(src);
        match &types["use_it"] {
            Type::Function(_, ret) => assert_eq!(**ret, Type::Int),
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    #[test]
    fn an_unbounded_generic_function_accepts_any_call_site() {
        let src = "let identity[T] (x: T): T = x\nlet use_it dummy = identity(true)";
        let types = infer_program(src);
        match &types["use_it"] {
            Type::Function(_, ret) => assert_eq!(**ret, Type::Bool),
            other => panic!("expected a Function type, got {other:?}"),
        }
    }

    // --- `select` ---

    #[test]
    fn select_infers_the_shared_element_type_across_arms() {
        let env = TypeEnv::new()
            .extend("rx1".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Int]))
            .extend("rx2".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Int]));
        let ty = infer_in("select { v = rx1.recv() => v, w = rx2.recv() => w }", &env);
        assert_eq!(ty, Type::Int);
    }

    #[test]
    fn select_arms_can_have_different_element_types_but_must_agree_on_result_type() {
        let env = TypeEnv::new()
            .extend("rx1".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Int]))
            .extend("rx2".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Bool]));
        let ty = infer_in("select { v = rx1.recv() => 1, w = rx2.recv() => 2 }", &env);
        assert_eq!(ty, Type::Int);
    }

    #[test]
    fn select_arms_with_mismatched_result_types_are_an_error() {
        let env = TypeEnv::new()
            .extend("rx1".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Int]))
            .extend("rx2".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Int]));
        infer_expr_with_err(&mut Infer::new(), "select { v = rx1.recv() => v, w = rx2.recv() => true }", &env);
    }

    #[test]
    fn select_wildcard_arm_ignores_the_received_value() {
        let env = TypeEnv::new().extend("rx".to_string(), Type::Struct("Receiver".to_string(), vec![Type::Int]));
        assert_eq!(infer_in("select { _ = rx.recv() => 0 }", &env), Type::Int);
    }

    #[test]
    fn select_on_a_non_receiver_value_is_an_error() {
        let env = TypeEnv::new().extend("x".to_string(), Type::Int);
        infer_expr_with_err(&mut Infer::new(), "select { v = x.recv() => v }", &env);
    }

    #[test]
    fn select_arm_not_shaped_like_a_recv_call_is_an_error() {
        infer_expr_with_err(&mut Infer::new(), "select { v = 5 => v }", &TypeEnv::new());
    }

    #[test]
    fn a_struct_value_can_be_received_through_select() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let env = TypeEnv::new().extend(
            "rx".to_string(),
            Type::Struct("Receiver".to_string(), vec![Type::Struct("Point".to_string(), vec![])]),
        );
        let ty = infer_expr_with(&mut infer, "select { p = rx.recv() => match p { Point(a, b) => a } }", &env);
        assert_eq!(ty, Type::Int);
    }

    // --- Arrays ---

    #[test]
    fn array_literal_infers_the_shared_element_type() {
        assert_eq!(infer("[1, 2, 3]"), Type::Struct("Array".to_string(), vec![Type::Int]));
    }

    #[test]
    fn empty_array_literal_has_an_unconstrained_element_type() {
        match infer("[]") {
            Type::Struct(name, args) => {
                assert_eq!(name, "Array");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0], Type::Var(_)), "expected an unconstrained var, got {:?}", args[0]);
            }
            other => panic!("expected an Array type, got {other:?}"),
        }
    }

    #[test]
    fn array_literal_with_mismatched_element_types_is_an_error() {
        infer_err("[1, true]");
    }

    #[test]
    fn array_index_infers_the_element_type() {
        assert_eq!(infer("[1, 2, 3][0]"), Type::Int);
    }

    #[test]
    fn array_index_requires_an_int_index() {
        infer_err("[1, 2, 3][true]");
    }

    #[test]
    fn indexing_a_non_array_is_an_error() {
        infer_err("5[0]");
    }

    #[test]
    fn string_index_infers_as_int() {
        assert_eq!(infer("\"hello\"[0]"), Type::Int);
    }

    #[test]
    fn string_index_requires_an_int_index() {
        infer_err("\"hello\"[true]");
    }

    #[test]
    fn string_runes_infers_as_array_of_int() {
        assert_eq!(infer("\"hello\".runes()"), Type::Struct("Array".to_string(), vec![Type::Int]));
    }

    #[test]
    fn runes_on_a_non_string_is_an_error() {
        infer_err("5.runes()");
    }

    #[test]
    fn string_trim_infers_as_str() {
        assert_eq!(infer("\"  hi  \".trim()"), Type::Str);
    }

    #[test]
    fn trim_on_a_non_string_is_an_error() {
        infer_err("5.trim()");
    }

    #[test]
    fn string_split_infers_as_array_of_str() {
        assert_eq!(infer("\"a,b,c\".split(\",\")"), Type::Struct("Array".to_string(), vec![Type::Str]));
    }

    #[test]
    fn string_split_argument_type_is_checked() {
        infer_err("\"a,b,c\".split(5)");
    }

    #[test]
    fn split_on_a_non_string_is_an_error() {
        infer_err("5.split(\",\")");
    }

    #[test]
    fn string_to_upper_infers_as_str() {
        assert_eq!(infer("\"hi\".to_upper()"), Type::Str);
    }

    #[test]
    fn to_upper_on_a_non_string_is_an_error() {
        infer_err("5.to_upper()");
    }

    #[test]
    fn string_to_lower_infers_as_str() {
        assert_eq!(infer("\"HI\".to_lower()"), Type::Str);
    }

    #[test]
    fn to_lower_on_a_non_string_is_an_error() {
        infer_err("5.to_lower()");
    }

    #[test]
    fn string_contains_infers_as_bool() {
        assert_eq!(infer("\"hi\".contains(\"i\")"), Type::Bool);
    }

    #[test]
    fn string_contains_argument_type_is_checked() {
        infer_err("\"hi\".contains(5)");
    }

    #[test]
    fn contains_on_a_non_string_is_an_error() {
        infer_err("5.contains(\"i\")");
    }

    #[test]
    fn string_starts_with_infers_as_bool() {
        assert_eq!(infer("\"hi\".starts_with(\"h\")"), Type::Bool);
    }

    #[test]
    fn starts_with_on_a_non_string_is_an_error() {
        infer_err("5.starts_with(\"h\")");
    }

    #[test]
    fn string_ends_with_infers_as_bool() {
        assert_eq!(infer("\"hi\".ends_with(\"i\")"), Type::Bool);
    }

    #[test]
    fn ends_with_on_a_non_string_is_an_error() {
        infer_err("5.ends_with(\"i\")");
    }

    #[test]
    fn string_replace_infers_as_str() {
        assert_eq!(infer("\"hi\".replace(\"h\", \"H\")"), Type::Str);
    }

    #[test]
    fn string_replace_argument_types_are_checked() {
        infer_err("\"hi\".replace(5, \"H\")");
        infer_err("\"hi\".replace(\"h\", 5)");
    }

    #[test]
    fn replace_on_a_non_string_is_an_error() {
        infer_err("5.replace(\"h\", \"H\")");
    }

    #[test]
    fn int_to_string_infers_as_str() {
        assert_eq!(infer("5.to_string()"), Type::Str);
    }

    #[test]
    fn float_to_string_infers_as_str() {
        assert_eq!(infer("3.14.to_string()"), Type::Str);
    }

    #[test]
    fn bool_to_string_infers_as_str() {
        assert_eq!(infer("true.to_string()"), Type::Str);
    }

    #[test]
    fn str_to_string_infers_as_str() {
        assert_eq!(infer("\"hi\".to_string()"), Type::Str);
    }

    #[test]
    fn to_string_on_an_array_is_not_yet_supported() {
        infer_err("[1, 2].to_string()");
    }

    #[test]
    fn to_string_on_a_still_unresolved_generic_parameter_is_permitted() {
        // Deliberately PERMISSIVE, not an error — see the real
        // regression this caught: `[1,2,3].map(|x| x.to_string())`
        // needs `x`'s still-unresolved type inside the closure body to
        // pass through here, since it's only pinned by unifying the
        // closure's OWN function type against the array's element type
        // afterward. A genuinely wrong concrete type (e.g. calling
        // `f` with an Array) is still caught — just not until that
        // concrete type is known.
        let types = infer_program("let f x = x.to_string()");
        let Type::Function(params, ret) = &types["f"] else {
            panic!("expected a function type, got {:?}", types["f"]);
        };
        assert_eq!(*ret.as_ref(), Type::Str);
        assert!(matches!(&params[0], Type::Var(_)), "expected an unresolved param type, got {:?}", params[0]);
    }

    #[test]
    fn ref_new_infers_the_ref_of_the_values_type() {
        assert_eq!(infer("ref(5)"), Type::Struct("Ref".to_string(), vec![Type::Int]));
    }

    #[test]
    fn ref_get_infers_the_underlying_type() {
        assert_eq!(infer("ref(5).get()"), Type::Int);
    }

    #[test]
    fn ref_set_infers_as_unit() {
        assert_eq!(infer("ref(5).set(6)"), Type::Unit);
    }

    #[test]
    fn ref_set_argument_type_is_checked() {
        infer_err("ref(5).set(true)");
    }

    #[test]
    fn get_on_a_non_ref_is_an_error() {
        infer_err("5.get()");
    }

    #[test]
    fn set_with_one_argument_on_a_non_ref_is_an_error() {
        infer_err("5.set(6)");
    }

    #[test]
    fn ref_of_a_struct_round_trips() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let tokens = Lexer::new("ref(Point { x: 1, y: 2 }).get()").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap();
        let (ty, subst) = infer.infer_expr(&ast, &TypeEnv::new()).unwrap_or_else(|e| panic!("inference error: {e}"));
        assert_eq!(subst.apply(&ty), Type::Struct("Point".to_string(), vec![]));
    }

    #[test]
    fn ref_annotation_is_accepted_in_a_parameter_position() {
        let types = infer_program("let f (r: Ref[Int]) = r.get()");
        assert_eq!(types["f"], fn_ty(vec![Type::Struct("Ref".to_string(), vec![Type::Int])], Type::Int));
    }

    #[test]
    fn builtin_types_are_accepted_in_struct_field_declarations() {
        // Regression coverage for a real, pre-existing gap caught while
        // testing `Ref`: `ast_type_to_type` (used for struct/enum field
        // declarations) needed its OWN copy of the same opaque-
        // pseudo-generic-builtin check `resolve_annotation` (used for
        // function param/return annotations) already had — this
        // affected `Array`/`Task`/`Sender`/`Receiver` too, not just
        // `Ref`, since none of them were ever handled here before.
        let ctx = context("struct Counter { value: Ref[Int] }");
        assert_eq!(
            ctx.struct_fields("Counter"),
            Some(&[("value".to_string(), Type::Struct("Ref".to_string(), vec![Type::Int]))][..])
        );
    }

    #[test]
    fn array_len_infers_as_int() {
        assert_eq!(infer("[1, 2, 3].len()"), Type::Int);
    }

    #[test]
    fn len_on_a_non_array_is_an_error() {
        infer_err("5.len()");
    }

    #[test]
    fn string_len_infers_as_int() {
        assert_eq!(infer("\"hello\".len()"), Type::Int);
    }

    #[test]
    fn string_concat_infers_as_str() {
        assert_eq!(infer("\"a\".concat(\"b\")"), Type::Str);
    }

    #[test]
    fn string_concat_argument_type_is_checked() {
        infer_err("\"a\".concat(5)");
    }

    #[test]
    fn concat_on_a_non_string_is_an_error() {
        infer_err("5.concat(\"a\")");
    }

    #[test]
    fn a_still_generic_len_call_still_defaults_to_array() {
        // Regression coverage for the same class of bug `for x in arr`
        // caught: `.len()` must check the ALREADY-RESOLVED shape, not
        // trial-unify against Array first — but for a still-unresolved
        // var (no Str/Array info at all yet), the pre-existing
        // Array-unify default must still apply unchanged.
        let types = infer_program("let f arr = arr.len()");
        let Type::Function(params, ret) = &types["f"] else {
            panic!("expected a function type, got {:?}", types["f"]);
        };
        assert_eq!(*ret.as_ref(), Type::Int);
        assert!(
            matches!(&params[0], Type::Struct(name, args) if name == "Array" && args.len() == 1),
            "expected an Array[_] parameter, got {:?}",
            params[0]
        );
    }

    #[test]
    fn array_push_infers_as_the_same_array_type() {
        assert_eq!(infer("[1, 2].push(3)"), Type::Struct("Array".to_string(), vec![Type::Int]));
    }

    #[test]
    fn array_push_argument_type_is_checked() {
        infer_err("[1, 2].push(true)");
    }

    #[test]
    fn push_on_a_non_array_is_an_error() {
        infer_err("5.push(1)");
    }

    #[test]
    fn array_pop_infers_as_the_same_array_type() {
        assert_eq!(infer("[1, 2].pop()"), Type::Struct("Array".to_string(), vec![Type::Int]));
    }

    #[test]
    fn pop_on_a_non_array_is_an_error() {
        infer_err("5.pop()");
    }

    #[test]
    fn array_set_infers_as_the_same_array_type() {
        assert_eq!(infer("[1, 2].set(0, 9)"), Type::Struct("Array".to_string(), vec![Type::Int]));
    }

    #[test]
    fn array_set_requires_an_int_index() {
        infer_err("[1, 2].set(true, 9)");
    }

    #[test]
    fn array_set_argument_type_is_checked() {
        infer_err("[1, 2].set(0, true)");
    }

    #[test]
    fn set_on_a_non_array_is_an_error() {
        infer_err("5.set(0, 1)");
    }

    #[test]
    fn array_remove_infers_as_the_same_array_type() {
        assert_eq!(infer("[1, 2].remove(0)"), Type::Struct("Array".to_string(), vec![Type::Int]));
    }

    #[test]
    fn array_remove_requires_an_int_index() {
        infer_err("[1, 2].remove(true)");
    }

    #[test]
    fn remove_on_a_non_array_is_an_error() {
        infer_err("5.remove(0)");
    }

    #[test]
    fn array_map_infers_as_an_array_of_the_functions_return_type() {
        let env = TypeEnv::new().extend(
            "to_str".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Str)),
        );
        assert_eq!(
            infer_in("[1, 2, 3].map(to_str)", &env),
            Type::Struct("Array".to_string(), vec![Type::Str])
        );
    }

    #[test]
    fn array_map_function_argument_type_is_checked() {
        let env = TypeEnv::new().extend(
            "to_str".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Str)),
        );
        infer_err_in("[true, false].map(to_str)", &env);
    }

    #[test]
    fn map_on_a_non_array_is_an_error() {
        infer_err("5.map(f)");
    }

    #[test]
    fn array_filter_infers_as_the_same_array_type() {
        let env = TypeEnv::new().extend(
            "is_pos".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Bool)),
        );
        assert_eq!(
            infer_in("[1, 2, 3].filter(is_pos)", &env),
            Type::Struct("Array".to_string(), vec![Type::Int])
        );
    }

    #[test]
    fn array_filter_function_must_return_bool() {
        let env = TypeEnv::new().extend(
            "to_str".to_string(),
            Type::Function(vec![Type::Int], Box::new(Type::Str)),
        );
        infer_err_in("[1, 2, 3].filter(to_str)", &env);
    }

    #[test]
    fn filter_on_a_non_array_is_an_error() {
        infer_err("5.filter(f)");
    }

    #[test]
    fn array_fold_infers_as_the_accumulator_type() {
        let env = TypeEnv::new().extend(
            "add".to_string(),
            Type::Function(vec![Type::Int, Type::Int], Box::new(Type::Int)),
        );
        assert_eq!(infer_in("[1, 2, 3].fold(0, add)", &env), Type::Int);
    }

    #[test]
    fn array_fold_function_argument_types_are_checked() {
        let env = TypeEnv::new().extend(
            "add".to_string(),
            Type::Function(vec![Type::Int, Type::Int], Box::new(Type::Int)),
        );
        infer_err_in("[true, false].fold(0, add)", &env);
    }

    #[test]
    fn fold_on_a_non_array_is_an_error() {
        infer_err("5.fold(0, f)");
    }

    #[test]
    fn for_over_an_array_infers_as_unit_and_binds_the_element_type() {
        assert_eq!(infer("for x in [1, 2, 3] { x + 1 }"), Type::Unit);
    }

    #[test]
    fn for_over_an_array_of_structs_binds_the_struct_type() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let tokens = Lexer::new("for p in [Point { x: 1, y: 2 }] { p.x }").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap();
        let (ty, subst) = infer.infer_expr(&ast, &TypeEnv::new()).unwrap_or_else(|e| panic!("inference error: {e}"));
        assert_eq!(subst.apply(&ty), Type::Unit);
    }

    #[test]
    fn for_over_an_array_element_type_mismatch_inside_the_body_is_an_error() {
        infer_err("for x in [true, false] { x + 1 }");
    }

    #[test]
    fn for_over_an_array_records_the_loops_span_in_array_for_loops() {
        let tokens = Lexer::new("for x in [1, 2, 3] { x }").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap();
        let mut infer = Infer::new();
        infer.infer_expr(&ast, &TypeEnv::new()).unwrap();
        assert_eq!(infer.array_for_loops().len(), 1);
    }

    #[test]
    fn for_over_a_range_does_not_record_anything_in_array_for_loops() {
        let tokens = Lexer::new("for x in 0..5 { x }").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap();
        let mut infer = Infer::new();
        infer.infer_expr(&ast, &TypeEnv::new()).unwrap();
        assert!(infer.array_for_loops().is_empty());
    }

    #[test]
    fn a_polymorphic_for_loop_over_a_still_unresolved_var_still_defaults_to_range() {
        // Regression test for a real bug caught while implementing
        // array iteration: unifying a still-unresolved type variable
        // against `Array[fresh]` trivially succeeds (it just binds the
        // var), which would wrongly commit a genuinely Range-typed
        // polymorphic loop to the array desugaring. `sum_range`'s `r`
        // parameter has no annotation, so its type is a bare `Var` at
        // the point `for i in r` is inferred.
        let src = "let sum_range r = { let mut sum = 0; for i in r { sum = sum + i; }; sum }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let mut infer = Infer::new();
        infer.infer_program(&program).unwrap_or_else(|e| panic!("inference error: {e}"));
        assert!(infer.array_for_loops().is_empty());
    }

    #[test]
    fn a_struct_value_can_live_in_an_array() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let ty = infer_expr_with(
            &mut infer,
            "match [Point { x: 1, y: 2 }][0] { Point(a, b) => a }",
            &TypeEnv::new(),
        );
        assert_eq!(ty, Type::Int);
    }

    // --- Duplicate top-level declarations ---

    #[test]
    fn redeclaring_a_function_is_an_error() {
        let err = infer_program_err("let f x = x + 1\nlet f x = x + 2");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }

    #[test]
    fn redeclaring_a_global_is_an_error() {
        let err = infer_program_err("let x = 1\nlet x = 2");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }

    #[test]
    fn a_function_and_a_global_sharing_a_name_is_an_error() {
        let err = infer_program_err("let f = 1\nlet f x = x");
        assert!(err.contains("already declared"), "expected an already-declared error, got: {err}");
    }

    #[test]
    fn infer_program_mutual_recursion() {
        // is_even references is_odd BEFORE is_odd has been inferred —
        // only works because signatures are pre-declared in Phase 1
        // before either body is processed in Phase 2.
        let src = "let is_even n = if n == 0 { true } else { is_odd(n - 1) }\n\
                   let is_odd n = if n == 0 { false } else { is_even(n - 1) }";
        let types = infer_program(src);
        assert_eq!(types["is_even"], fn_ty(vec![Type::Int], Type::Bool));
        assert_eq!(types["is_odd"], fn_ty(vec![Type::Int], Type::Bool));
    }

    #[test]
    fn a_global_aliasing_a_function_declared_earlier_resolves_calls_through_it_fully() {
        // Regression test for a real bug: `body_env` for a function was
        // built from a bare `global_env.clone()`, never re-applying the
        // accumulated `acc` — so a GLOBAL whose initializer merely
        // copied another function's type (`let f = square`, no call)
        // captured that function's still-unresolved Phase-1 placeholder
        // type variables verbatim. Any LATER function calling through
        // that global (`f(5)`) unified against those stale, disconnected
        // variable ids instead of the REAL, by-then-fully-resolved
        // signature — silently leaving the caller's own return type an
        // unresolved variable instead of `Int`. Fixed by building
        // `body_env` via `global_env.apply_subst(&acc)` instead of a
        // bare `.clone()`.
        let src = "let square x = x * x\nlet f = square\nlet g dummy = f(5)";
        let types = infer_program(src);
        assert_eq!(types["f"], fn_ty(vec![Type::Int], Type::Int));
        let (_, g_ret) = match &types["g"] {
            Type::Function(params, ret) => (params.clone(), (**ret).clone()),
            other => panic!("expected a function type, got {other:?}"),
        };
        assert_eq!(g_ret, Type::Int);
    }

    #[test]
    fn top_level_functions_are_generalized_and_used_polymorphically_at_each_call_site() {
        // Without let-polymorphism, `identity`'s single type variable
        // would get pinned to Bool by the first call and then conflict
        // with the second call's Int, even though `identity` is
        // obviously generic. Real let-polymorphism instantiates a
        // FRESH copy of a previously-inferred function's type at each
        // call site.
        let types = infer_program(
            "let identity x = x\n\
             let use_it n = if identity(true) { identity(n) } else { 0 }",
        );
        assert_eq!(types["use_it"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn a_generic_functions_own_signature_still_shares_one_variable_across_its_own_uses() {
        // identity's own representative type (returned for external
        // inspection, before generalization hides the variable behind
        // a scheme) still ties its parameter and return type to the
        // SAME variable — it's specifically FUTURE call sites that each
        // get an independent, freshly-instantiated copy, not identity's
        // own signature.
        let types = infer_program("let identity x = x");
        match &types["identity"] {
            Type::Function(params, ret) => {
                assert_eq!(params.len(), 1);
                assert_eq!(&params[0], ret.as_ref());
                assert!(matches!(params[0], Type::Var(_)));
            }
            other => panic!("expected a function type, got {other:?}"),
        }
    }

    #[test]
    fn infer_program_call_arity_mismatch_is_an_error() {
        infer_program_err("let double n = n * 2\nlet caller x = double(x, x)");
    }

    #[test]
    fn infer_program_call_type_mismatch_is_an_error() {
        infer_program_err("let want_bool b = !b\nlet caller x = want_bool(x + 1)");
    }

    #[test]
    fn a_unit_pattern_function_param_type_checks() {
        // `()` — the Unit pattern, e.g. `let main () = 5` — a
        // genuinely different case from a zero-PARAMETER `let` (which
        // makes it a global, not a function): this is a function
        // taking exactly one argument, of type `Unit`, binding no name.
        let types = infer_program("let go () = 5");
        assert_eq!(types["go"], fn_ty(vec![Type::Unit], Type::Int));
    }

    #[test]
    fn infer_program_zero_param_let_is_a_global() {
        let types = infer_program("let x = 5");
        assert_eq!(types["x"], Type::Int);
    }

    // --- extern "C" / unsafe ---

    #[test]
    fn extern_call_inside_unsafe_type_checks() {
        let types = infer_program(
            r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let result = unsafe { sqrt(16.0) }
            "#,
        );
        assert_eq!(types["result"], Type::Float);
    }

    #[test]
    fn extern_call_outside_unsafe_is_an_error() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let result = sqrt(16.0)
            "#,
        );
        assert!(err.contains("unsafe"), "unexpected error: {err}");
    }

    #[test]
    fn extern_call_with_wrong_argument_type_inside_unsafe_is_still_an_error() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let result = unsafe { sqrt(true) }
            "#,
        );
        assert!(!err.contains("unsafe"), "should be a type mismatch, not an unsafe-gating error: {err}");
    }

    #[test]
    fn extern_function_with_no_return_type_is_unit() {
        let types = infer_program(
            r#"
            extern "C" {
                fn srand(seed: Int);
            }
            let result = unsafe { srand(1) }
            "#,
        );
        assert_eq!(types["result"], Type::Unit);
    }

    #[test]
    fn extern_function_name_colliding_with_a_global_is_an_error() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn sqrt(x: Float) -> Float;
            }
            let sqrt = 1
            "#,
        );
        assert!(err.contains("already declared"), "unexpected error: {err}");
    }

    #[test]
    fn extern_function_with_an_unsupported_type_is_rejected() {
        // An ordinary `String` (not `CStr`) isn't FFI-safe — it must go
        // through `.as_cstr()` first (see DESIGN.md's "no implicit
        // string/allocation coercion" stance).
        let tokens = Lexer::new(
            r#"
            extern "C" {
                fn foo(x: String) -> Int;
            }
            "#,
        )
        .tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap();
        let err = crate::context::TypeContext::from_items(&program.items)
            .expect_err("expected an unsupported extern param type to be rejected");
        assert!(err.contains("Int, Float, Bool, CStr"), "unexpected error: {err}");
    }

    #[test]
    fn extern_function_with_an_int_float_bool_struct_type_is_accepted() {
        let types = infer_program(
            r#"
            struct Point { x: Int, y: Float }
            extern "C" {
                fn make_point(x: Int, y: Float) -> Point;
            }
            let result = unsafe { make_point(1, 2.0) }
            "#,
        );
        assert_eq!(types["result"], Type::Struct("Point".to_string(), vec![]));
    }

    #[test]
    fn extern_function_with_a_struct_field_that_is_itself_ffi_safe_is_accepted() {
        let types = infer_program(
            r#"
            struct Inner { a: Int, b: Int }
            struct Outer { inner: Inner, c: Int }
            extern "C" {
                fn nested_sum(o: Outer) -> Int;
            }
            let result = unsafe { nested_sum(Outer { inner: Inner { a: 1, b: 2 }, c: 3 }) }
            "#,
        );
        assert_eq!(types["result"], Type::Int);
    }

    #[test]
    fn extern_function_with_a_struct_containing_an_enum_field_is_rejected() {
        let program = Parser::new(
                Lexer::new(
                    r#"
                    enum Color { Red, Blue }
                    struct Shape { color: Color }
                    extern "C" {
                        fn foo(s: Shape) -> Int;
                    }
                    "#,
                )
                .tokenize(),
            )
            .parse_program()
            .unwrap();
        let err = TypeContext::from_items(&program.items).expect_err("expected a struct with an enum field to be rejected");
        assert!(err.contains("Int, Float, Bool, CStr"), "unexpected error: {err}");
    }

    #[test]
    fn extern_function_with_a_self_referential_struct_is_rejected() {
        let program = Parser::new(
                Lexer::new(
                    r#"
                    struct Node { next: Node }
                    extern "C" {
                        fn foo(n: Node) -> Int;
                    }
                    "#,
                )
                .tokenize(),
            )
            .parse_program()
            .unwrap();
        let err = TypeContext::from_items(&program.items).expect_err("expected a self-referential struct to be rejected");
        assert!(err.contains("self-referential"), "unexpected error: {err}");
    }

    #[test]
    fn extern_function_with_a_cstr_field_inside_a_struct_is_rejected() {
        // `CStr` isn't a resolvable ordinary type annotation at all
        // (only `.as_cstr()` ever produces it) — so a struct can't even
        // DECLARE a `CStr`-typed field, which already makes this
        // rejected before `check_ffi_safe`'s own defensive "CStr is
        // only supported as a top-level extern parameter/return type"
        // restriction is even reached. That restriction stays in place
        // as a second line of defense should `CStr` ever become a
        // generally-resolvable annotation later.
        let program = Parser::new(
                Lexer::new(
                    r#"
                    struct Labeled { label: CStr }
                    extern "C" {
                        fn foo(l: Labeled) -> Int;
                    }
                    "#,
                )
                .tokenize(),
            )
            .parse_program()
            .unwrap();
        assert!(TypeContext::from_items(&program.items).is_err());
    }

    #[test]
    fn extern_function_with_a_generic_struct_type_is_rejected() {
        let program = Parser::new(
                Lexer::new(
                    r#"
                    struct Box[T] { value: T }
                    extern "C" {
                        fn foo(b: Box[Int]) -> Int;
                    }
                    "#,
                )
                .tokenize(),
            )
            .parse_program()
            .unwrap();
        let err = TypeContext::from_items(&program.items).expect_err("expected a generic struct type to be rejected");
        assert!(err.contains("Int, Float, Bool, CStr"), "unexpected error: {err}");
    }

    #[test]
    fn callback_argument_naming_a_top_level_function_type_checks() {
        let types = infer_program(
            r#"
            extern "C" {
                fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int;
            }
            let add (a: Int) (b: Int): Int = a + b
            let result = unsafe { call_with_10_and_20(add) }
            "#,
        );
        assert_eq!(types["result"], Type::Int);
    }

    #[test]
    fn callback_argument_as_a_closure_literal_is_rejected() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int;
            }
            let result = unsafe { call_with_10_and_20(|a, b| a + b) }
            "#,
        );
        assert!(err.contains("bare reference"), "unexpected error: {err}");
    }

    #[test]
    fn callback_argument_as_a_local_variable_is_rejected() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int;
            }
            let add (a: Int) (b: Int): Int = a + b
            let go x = { let f = add; unsafe { call_with_10_and_20(f) } }
            "#,
        );
        assert!(err.contains("bare reference"), "unexpected error: {err}");
    }

    #[test]
    fn callback_argument_with_a_mismatched_signature_is_a_type_error() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn call_with_10_and_20(f: (Int, Int) -> Int) -> Int;
            }
            let concat (a: Str) (b: Str): Str = a
            let result = unsafe { call_with_10_and_20(concat) }
            "#,
        );
        assert!(!err.is_empty());
    }

    #[test]
    fn callback_return_type_is_rejected() {
        let program = Parser::new(
            Lexer::new(
                r#"
                extern "C" {
                    fn foo() -> (Int) -> Int;
                }
                "#,
            )
            .tokenize(),
        )
        .parse_program()
        .unwrap();
        let err = TypeContext::from_items(&program.items).expect_err("expected a callback return type to be rejected");
        assert!(err.contains("return type"), "unexpected error: {err}");
    }

    #[test]
    fn nested_callback_type_is_rejected() {
        let program = Parser::new(
            Lexer::new(
                r#"
                extern "C" {
                    fn foo(f: ((Int) -> Int) -> Int) -> Int;
                }
                "#,
            )
            .tokenize(),
        )
        .parse_program()
        .unwrap();
        assert!(TypeContext::from_items(&program.items).is_err());
    }

    #[test]
    fn extern_function_with_an_unrecognized_type_name_is_rejected() {
        let tokens = Lexer::new(
            r#"
            extern "C" {
                fn foo(x: NotARealType) -> Int;
            }
            "#,
        )
        .tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap();
        assert!(crate::context::TypeContext::from_items(&program.items).is_err());
    }

    #[test]
    fn as_cstr_on_a_string_evaluates_to_cstr() {
        assert_eq!(infer_in("\"hi\".as_cstr()", &TypeEnv::new()), Type::CStr);
    }

    #[test]
    fn as_cstr_on_a_non_string_is_an_error() {
        infer_err_in("5.as_cstr()", &TypeEnv::new());
    }

    #[test]
    fn an_ordinary_str_cannot_be_passed_where_extern_expects_cstr() {
        let err = infer_program_err(
            r#"
            extern "C" {
                fn strlen(s: CStr) -> Int;
            }
            let result = unsafe { strlen("hi") }
            "#,
        );
        assert!(!err.is_empty());
    }

    #[test]
    fn as_cstr_result_passed_to_a_cstr_extern_param_type_checks() {
        let types = infer_program(
            r#"
            extern "C" {
                fn strlen(s: CStr) -> Int;
            }
            let result = unsafe { strlen("hi".as_cstr()) }
            "#,
        );
        assert_eq!(types["result"], Type::Int);
    }

    #[test]
    fn infer_program_global_can_reference_an_earlier_global() {
        let types = infer_program("let a = 1\nlet b = a + 1");
        assert_eq!(types["a"], Type::Int);
        assert_eq!(types["b"], Type::Int);
    }

    #[test]
    fn infer_program_global_type_error_is_reported() {
        infer_program_err("let x = 1 + true");
    }

    #[test]
    fn infer_program_global_referencing_a_later_global_is_an_error() {
        // No forward reference — a global can only see EARLIER globals.
        infer_program_err("let a = b\nlet b = 1");
    }

    #[test]
    fn infer_program_a_global_can_call_a_function_regardless_of_declaration_order() {
        let types = infer_program("let x = double(5)\nlet double n = n * 2");
        assert_eq!(types["x"], Type::Int);
        assert_eq!(types["double"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn infer_program_a_function_can_reference_an_earlier_global() {
        let types = infer_program("let pi_ish = 3\nlet area r = pi_ish * r * r");
        assert_eq!(types["area"], fn_ty(vec![Type::Int], Type::Int));
    }

    #[test]
    fn infer_program_globals_and_functions_can_be_interleaved() {
        let types = infer_program("let a = 1\nlet double n = n * 2\nlet b = double(a)");
        assert_eq!(types["a"], Type::Int);
        assert_eq!(types["b"], Type::Int);
        assert_eq!(types["double"], fn_ty(vec![Type::Int], Type::Int));
    }

    // --- generic instantiation site capture / resolve_generic_sites ---

    fn infer_with(src: &str) -> Infer {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ctx = crate::context::TypeContext::from_items(&program.items)
            .unwrap_or_else(|e| panic!("context error for {src:?}: {e}"));
        let mut infer = Infer::with_context(ctx);
        infer
            .infer_program(&program)
            .unwrap_or_else(|e| panic!("program inference error for {src:?}: {e}"));
        infer
    }

    #[test]
    fn resolve_generic_sites_resolves_a_top_level_struct_construction_to_a_concrete_arg() {
        let infer = infer_with("struct Pair[T] { first: T, second: T }\nlet go () = Pair { first: 1, second: 2 }");
        let sites = infer.resolve_generic_sites().unwrap();
        assert_eq!(sites.len(), 1);
        let site = sites.values().next().unwrap();
        assert_eq!(site.kind, SiteKind::Struct);
        assert_eq!(site.decl_name, "Pair");
        assert_eq!(site.args, vec![Type::Int]);
        assert_eq!(site.enclosing_fn, Some("go".to_string()));
    }

    #[test]
    fn resolve_generic_sites_resolves_two_different_instantiations_independently() {
        let src = "struct Pair[T] { first: T, second: T }\n\
                   let go_int () = Pair { first: 1, second: 2 }\n\
                   let go_bool () = Pair { first: true, second: false }";
        let infer = infer_with(src);
        let sites = infer.resolve_generic_sites().unwrap();
        assert_eq!(sites.len(), 2);
        let mut args: Vec<Vec<Type>> = sites.values().map(|s| s.args.clone()).collect();
        args.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(args, vec![vec![Type::Bool], vec![Type::Int]]);
    }

    #[test]
    fn resolve_generic_sites_records_a_generic_function_call_in_declared_generic_order() {
        let src = "let identity[T] (x: T): T = x\nlet go () = identity(5)";
        let infer = infer_with(src);
        let sites = infer.resolve_generic_sites().unwrap();
        assert_eq!(sites.len(), 1);
        let site = sites.values().next().unwrap();
        assert_eq!(site.kind, SiteKind::Function);
        assert_eq!(site.decl_name, "identity");
        assert_eq!(site.args, vec![Type::Int]);
    }

    #[test]
    fn resolve_generic_sites_marks_a_tier_two_site_as_a_param_template() {
        // `wrap`'s own body constructs `Box[T]` from ITS OWN generic `T`
        // — never pinned to anything concrete inside `wrap` itself, so
        // this site must resolve to `Type::Param("T")`, a template for
        // `monomorphize::plan` to substitute later, not an ambiguity
        // error.
        let src = "struct Box[T] { val: T }\n\
                   let wrap[T] (x: T): Box[T] = Box { val: x }\n\
                   let go () = wrap(5)";
        let infer = infer_with(src);
        let sites = infer.resolve_generic_sites().unwrap();
        let box_site = sites.values().find(|s| s.kind == SiteKind::Struct).unwrap();
        assert_eq!(box_site.enclosing_fn, Some("wrap".to_string()));
        assert_eq!(box_site.args, vec![Type::Param("T".to_string())]);
    }

    #[test]
    fn resolve_generic_sites_rejects_a_type_parameter_never_pinned_anywhere() {
        // `None` alone never pins `Option`'s own `T` to anything
        // concrete — a genuine ambiguity, not a tier-2 template (no
        // enclosing generic function's own parameter to blame it on).
        let infer = infer_with("enum MyOption[T] { MySome(T), MyNone }\nlet go () = MyNone");
        let err = infer.resolve_generic_sites().expect_err("expected an ambiguous-type-parameter error");
        assert!(err.contains("never pinned"), "unexpected error: {err}");
    }

    #[test]
    fn fn_generics_records_declared_order_for_a_multi_param_generic_function() {
        let infer = infer_with("let pair[A, B] (a: A) (b: B): A = a\nlet go () = pair(1, true)");
        let names: Vec<&str> = infer.fn_generics()["pair"].iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
    }
}
