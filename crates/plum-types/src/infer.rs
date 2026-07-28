use crate::subst::Subst;
use crate::types::{Type, TypeVarId};
use crate::unify::unify;
use plum_syntax::ast;
use std::collections::{HashMap, HashSet};

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
}

impl Scheme {
    fn monomorphic(ty: Type) -> Scheme {
        Scheme { vars: Vec::new(), ty }
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
        Type::Int | Type::Float | Type::Bool | Type::Str | Type::Unit | Type::Range | Type::Struct(_) | Type::Enum(_) => {
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
    Scheme { vars, ty: ty.clone() }
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
}

impl Infer {
    pub fn new() -> Self {
        Infer {
            next_var: 0,
            ctx: crate::context::TypeContext::new(),
        }
    }

    /// For inferring anything that touches struct literals or `match`
    /// — see context.rs. Plain `new()` still works for everything that
    /// doesn't (an empty context just means struct/enum lookups always
    /// fail with "unknown type").
    pub fn with_context(ctx: crate::context::TypeContext) -> Self {
        Infer { next_var: 0, ctx }
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
    /// to exactly its own type, unchanged.
    fn instantiate(&mut self, scheme: &Scheme) -> Type {
        if scheme.vars.is_empty() {
            return scheme.ty.clone();
        }
        let mut subst = Subst::empty();
        for &v in &scheme.vars {
            let fresh = self.fresh();
            subst = Subst::single(v, fresh).compose(&subst);
        }
        subst.apply(&scheme.ty)
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

        for item in &program.items {
            if let ast::ItemKind::Let(def) = &item.kind {
                if def.params.is_empty() {
                    global_defs.push(def);
                    continue;
                }
                let param_vars: Vec<Type> = def.params.iter().map(|_| self.fresh()).collect();
                let ret_var = self.fresh();
                let fn_ty = Type::Function(param_vars.clone(), Box::new(ret_var.clone()));
                global_env = global_env.extend(def.name.clone(), fn_ty);
                signatures.insert(def.name.clone(), (param_vars, ret_var));
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
            let (ty, s) = self.infer_expr(&def.body, &global_env)?;
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
            let mut body_env = global_env.clone();
            for (param, param_ty) in def.params.iter().zip(param_vars.iter()) {
                match &param.kind {
                    ast::ParamKind::Ident(name) | ast::ParamKind::Pattern(ast::Pattern::Ident(name, _), _) => {
                        body_env = body_env.extend(name.clone(), acc.apply(param_ty));
                    }
                    // `bind_pattern` unifies `param_ty` (the single
                    // fresh var Phase 1 gave this flat-arity parameter)
                    // against whatever shape the pattern requires and
                    // binds every name it introduces, including nested
                    // ones — see `bind_pattern`'s doc comment.
                    ast::ParamKind::Pattern(pattern @ (ast::Pattern::Tuple(..) | ast::Pattern::Struct { .. }), _) => {
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
            let (body_ty, s) = self.infer_expr(&def.body, &body_env)?;
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
                let annotated_ty = ast_type_to_type(annotated, &self.ctx)?;
                let s = unify(&acc.apply(&ret_var), &annotated_ty).map_err(|e| {
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
            let scheme = generalize(&resolved_fn_ty, &outer_env);
            global_env = global_env.extend_scheme(def.name.clone(), scheme);
        }

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
            // A bare capitalized name referencing a zero-arity variant
            // (`None`, not `None()`) constructs it directly — mirrors
            // lower.rs's identical `Ident` case. A non-zero-arity
            // variant referenced bare falls through to the ordinary
            // `Ident` lookup below unchanged (still an unbound-variable
            // error, same as a bare top-level function name — neither
            // is a first-class value yet).
            ast::Expr::Ident(name, _) if matches!(self.ctx.variant(name), Some((_, p)) if p.is_empty()) => {
                let (enum_name, _) = self.ctx.variant(name).expect("just matched Some above").clone();
                Ok((Type::Enum(enum_name), Subst::empty()))
            }
            ast::Expr::Ident(name, span) => {
                let scheme = env
                    .lookup_scheme(name)
                    .cloned()
                    .ok_or_else(|| format!("unbound variable: {name} at {span:?}"))?;
                Ok((self.instantiate(&scheme), Subst::empty()))
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
            ast::Expr::Call { callee, args, span } => {
                // `Circle(1.0)` / `Shape.Circle(1.0)` constructs a
                // variant if the callee names one, checked BEFORE
                // falling back to an ordinary call — the type-level
                // counterpart to lower.rs's identical `Call` handling.
                // The qualifier before `.` (`Shape`) is never validated
                // against the variant's real owning enum, matching that
                // same established precedent (tags are looked up by
                // name alone).
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
                        return Ok((Type::Enum(enum_name), acc));
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
            ast::Expr::Closure { params, body, .. } => self.infer_closure(params, body, env),
            // `unsafe` gates nothing at the type level either — see
            // lower.rs's identical reasoning for why it lowers
            // transparently. Whatever the block's type is, that's this
            // expression's type too.
            ast::Expr::Unsafe(block, _) => self.infer_block(block, env),
            ast::Expr::For {
                pattern,
                iter,
                body,
                span,
            } => self.infer_for(pattern, iter, body, *span, env),
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
        let mut acc = s;
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
            let s = unify(&acc.apply(&spread_ty), &Type::Struct(tag.clone()))
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

        Ok((Type::Struct(tag), acc))
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
            ast::Pattern::Tuple(elems, span) => {
                if elems.is_empty() {
                    return Err(format!(
                        "type inference not yet implemented for destructuring against the \
                         empty tuple pattern at {span:?}"
                    ));
                }
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
                let s = unify(&acc.apply(scrutinee_ty), &Type::Struct(tag.clone()))
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
                    Some((enum_name, payload_types)) => (Type::Enum(enum_name.clone()), payload_types.clone()),
                    None => match self.ctx.struct_fields(&tag) {
                        Some(fields) => (
                            Type::Struct(tag.clone()),
                            fields.iter().map(|(_, ty)| ty.clone()).collect(),
                        ),
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
            other => Err(format!(
                "type inference not yet implemented for this pattern shape at {:?}",
                other.span()
            )),
        }
    }

    fn infer_match(&mut self, scrutinee: &ast::Expr, arms: &[ast::MatchArm], env: &TypeEnv) -> Result<(Type, Subst), String> {
        let (scrutinee_ty, s) = self.infer_expr(scrutinee, env)?;
        let mut acc = s;
        let mut result_ty: Option<Type> = None;

        for arm in arms {
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
            // correct for a NESTED sub-position, but wrong for a WHOLE
            // top-level arm: the IR's `Match` dispatches strictly by
            // tag (no "default arm" concept exists — see ir.rs's scope
            // note), so lower.rs's `lower_tag_pattern` has no case for
            // Ident/Wildcard at this level and would reject it. Guard
            // here so the type checker doesn't accept more than what
            // actually lowers.
            if matches!(arm.pattern, ast::Pattern::Ident(..) | ast::Pattern::Wildcard(..)) {
                return Err(format!(
                    "type inference not yet implemented for a bare identifier or wildcard as \
                     a whole match arm (no default-arm concept exists yet) at {:?}",
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
            // two arms that are both some N-tuple.
            let arm_env = self.bind_pattern(&arm.pattern, &acc.apply(&scrutinee_ty), env.clone(), &mut acc)?;
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

        let final_ty = result_ty.ok_or_else(|| "match with no arms has no result type".to_string())?;
        Ok((acc.apply(&final_ty), acc))
    }

    // Unlike a named top-level function (which gets a totally fresh,
    // isolated environment — see plum-interp's `function_body_cannot_
    // see_the_caller_environment`), a closure DOES see the surrounding
    // scope — that's the actual definition of a closure. `closure_env`
    // extends the caller's `env`, not a fresh one, on purpose.
    fn infer_closure(&mut self, params: &[ast::ClosureParam], body: &ast::Expr, env: &TypeEnv) -> Result<(Type, Subst), String> {
        let mut param_types = Vec::with_capacity(params.len());
        let mut closure_env = env.clone();
        for p in params {
            let ty = match &p.ty {
                Some(annotation) => ast_type_to_type(annotation, &self.ctx)?,
                None => self.fresh(),
            };
            closure_env = closure_env.extend(p.name.clone(), ty.clone());
            param_types.push(ty);
        }
        let (body_ty, acc) = self.infer_expr(body, &closure_env)?;
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
                let s = unify(&acc.apply(&iter_ty), &Type::Range)
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
                    let (val_ty, s) = self.infer_expr(value, &cur_env)?;
                    acc = s.compose(&acc);
                    let mut resolved = acc.apply(&val_ty);
                    if let Some(annotation) = ty {
                        let ann_ty = ast_type_to_type(annotation, &self.ctx)?;
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
// both work regardless of declaration order) — still primitive-and-
// nominal-only: a GENERIC type annotation remains a real, separate,
// deferred gap.
pub(crate) fn ast_type_to_type(ty: &ast::Type, ctx: &crate::context::TypeContext) -> Result<Type, String> {
    match ty {
        ast::Type::Path(segments, span) => match segments.last().map(String::as_str) {
            Some("Int") => Ok(Type::Int),
            Some("Float") => Ok(Type::Float),
            Some("Bool") => Ok(Type::Bool),
            Some("String") => Ok(Type::Str),
            Some("Unit") => Ok(Type::Unit),
            Some(name) if ctx.is_struct(name) => Ok(Type::Struct(name.to_string())),
            Some(name) if ctx.is_enum(name) => Ok(Type::Enum(name.to_string())),
            _ => Err(format!(
                "type inference not yet implemented for this type annotation at {span:?}"
            )),
        },
        ast::Type::Generic { span, .. } => Err(format!(
            "type inference not yet implemented for generic type annotations at {span:?}"
        )),
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
            Type::Enum("Shape".to_string())
        );
    }

    #[test]
    fn qualified_variant_call_infers_the_owning_enum_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Shape.Circle(1.0)", &TypeEnv::new()),
            Type::Enum("Shape".to_string())
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
        assert_eq!(infer_expr_with(&mut infer, "Empty", &TypeEnv::new()), Type::Enum("Shape".to_string()));
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
            Type::Struct("Point".to_string())
        );
    }

    #[test]
    fn struct_literal_field_order_independent() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        assert_eq!(
            infer_expr_with(&mut infer, "Point { y: 2.0, x: 1.0 }", &TypeEnv::new()),
            Type::Struct("Point".to_string())
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
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Point".to_string()));
        let ty = infer_expr_with(&mut infer, "Point { x: 1.0, ..other }", &env);
        assert_eq!(ty, Type::Struct("Point".to_string()));
    }

    #[test]
    fn struct_literal_spread_requires_the_spread_expr_to_be_the_same_struct() {
        let mut infer = Infer::with_context(context(
            "struct Point { x: Float, y: Float }\nstruct Color { r: Int, g: Int, b: Int }",
        ));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Color".to_string()));
        infer_expr_with_err(&mut infer, "Point { x: 1.0, ..other }", &env);
    }

    #[test]
    fn struct_literal_spread_still_checks_explicit_field_types() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Point".to_string()));
        infer_expr_with_err(&mut infer, "Point { x: true, ..other }", &env);
    }

    #[test]
    fn struct_literal_spread_still_rejects_an_unknown_field() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("other".to_string(), Type::Struct("Point".to_string()));
        infer_expr_with_err(&mut infer, "Point { z: 1.0, ..other }", &env);
    }

    // --- Match: enum variant patterns, resolved against the SAME
    // TypeContext (enum variant tag -> owning enum + payload types).

    #[test]
    fn match_variant_arms_produce_a_common_type() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Rectangle(Float, Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
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
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point(x, y) => x }", &env), Type::Float);
    }

    // --- Struct patterns (`Point { x, y }`), as opposed to the
    // variant-call-syntax fallback above (`Point(x, y)`) ---

    #[test]
    fn match_arm_struct_pattern() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point { x, y } => x }", &env), Type::Float);
    }

    #[test]
    fn match_arm_struct_pattern_field_rename() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        assert_eq!(
            infer_expr_with(&mut infer, "match p { Point { x: px, y: py } => px + py }", &env),
            Type::Float
        );
    }

    #[test]
    fn match_arm_struct_pattern_with_rest() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        assert_eq!(infer_expr_with(&mut infer, "match p { Point { x, .. } => x }", &env), Type::Float);
    }

    #[test]
    fn match_arm_struct_pattern_missing_field_without_rest_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        infer_expr_with_err(&mut infer, "match p { Point { x } => x }", &env);
    }

    #[test]
    fn match_arm_struct_pattern_unknown_field_is_an_error() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
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
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        assert_eq!(infer_expr_with(&mut infer, "{ let Point { x, y } = p; x + y }", &env), Type::Float);
    }

    #[test]
    fn block_let_struct_destructure_type_annotation_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
        infer_expr_with_err(&mut infer, "{ let Point { x, y }: Point = p; x }", &env);
    }

    #[test]
    fn struct_destructuring_function_param() {
        let types = infer_program("struct Point { x: Float, y: Float }\nlet area (Point { x, y }) = x * y");
        assert_eq!(types["area"], fn_ty(vec![Type::Struct("Point".to_string())], Type::Float));
    }

    // --- Nested patterns ---

    #[test]
    fn struct_nested_inside_tuple_match_arm() {
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("pair".to_string(), Type::Tuple(vec![Type::Struct("Point".to_string()), Type::Int]));
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
        let env = TypeEnv::new().extend("l".to_string(), Type::Struct("Line".to_string()));
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
        let env = TypeEnv::new().extend("pair".to_string(), Type::Tuple(vec![Type::Struct("Point".to_string()), Type::Int]));
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
            Type::Tuple(vec![Type::Tuple(vec![Type::Struct("Point".to_string()), Type::Int]), Type::Int]),
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
        let env = TypeEnv::new().extend("pair".to_string(), Type::Tuple(vec![Type::Struct("Point".to_string()), Type::Int]));
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
        assert_eq!(types["dx"], fn_ty(vec![Type::Struct("Line".to_string())], Type::Int));
    }

    #[test]
    fn nested_or_pattern_is_still_not_yet_supported() {
        // Nesting works for tag-based patterns (variant/tuple/struct)
        // — an or-pattern nested inside one is a genuinely separate,
        // still-unsupported gap, mirroring lower.rs's identical
        // restriction.
        let ctx = context("struct Point { x: Int, y: Int }");
        let mut infer = Infer::with_context(ctx);
        let env = TypeEnv::new().extend("p".to_string(), Type::Struct("Point".to_string()));
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
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(
            &mut infer,
            "match shape { Shape.Circle(r) => r, Shape.Rectangle(w, h) => true }",
            &env,
        );
    }

    #[test]
    fn match_unknown_variant_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(&mut infer, "match shape { Shape.Triangle(a) => a }", &env);
    }

    #[test]
    fn match_variant_wrong_arity_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(a, b) => a }", &env);
    }

    #[test]
    fn match_mixing_variants_from_different_enums_is_an_error() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }\nenum Color { Red, Blue }"));
        let env = TypeEnv::new().extend("x".to_string(), Type::Var(0));
        infer_expr_with_err(&mut infer, "match x { Shape.Circle(r) => 1, Color.Red => 2 }", &env);
    }

    #[test]
    fn match_bare_wildcard_arm_is_not_yet_supported() {
        // No "default arm" concept exists yet — same limitation as
        // lower.rs's IR Match. `_` used INSIDE a variant's args (e.g.
        // `Shape.Rectangle(_, _)`) is fine; a bare `_` as a WHOLE arm
        // isn't.
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(&mut infer, "match shape { _ => 1 }", &env);
    }

    #[test]
    fn match_or_pattern_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float), Empty }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(a) | Shape.Empty => 1 }", &env);
    }

    #[test]
    fn match_guard_infers_using_the_arms_own_bindings() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        let ty = infer_expr_with(&mut infer, "match shape { Shape.Circle(r) if r > 0.0 => r, Shape.Circle(r) => 0.0 }", &env);
        assert_eq!(ty, Type::Float);
    }

    #[test]
    fn match_guard_must_be_a_bool() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(r) if r => r, Shape.Circle(r) => 0.0 }", &env);
    }

    #[test]
    fn match_guard_combined_with_a_nested_pattern_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("struct Point { x: Int, y: Int }"));
        let env = TypeEnv::new().extend(
            "p".to_string(),
            Type::Tuple(vec![Type::Struct("Point".to_string()), Type::Int]),
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
        assert_eq!(ret, Type::Struct("Point".to_string()));
    }

    #[test]
    fn declared_return_type_referencing_the_wrong_struct_is_an_error() {
        let src = "struct Point { x: Int, y: Int }\nlet origin dummy: Int = Point { x: 0, y: 0 }";
        infer_program_err(src);
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
    fn infer_program_zero_param_let_is_a_global() {
        let types = infer_program("let x = 5");
        assert_eq!(types["x"], Type::Int);
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
}
