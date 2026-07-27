use crate::subst::Subst;
use crate::types::{Type, TypeVarId};
use crate::unify::unify;
use plum_syntax::ast;
use std::collections::HashMap;

/// A simple, monomorphic scope stack — same shadowing/scoping shape as
/// plum-interp's `env`, just mapping names to Types instead of Values.
/// Deliberately NOT polymorphic yet: a `let`-bound name gets exactly
/// one concrete (possibly still-a-variable) type, not a generalized
/// scheme that can be instantiated differently at each use. That's
/// `let`-polymorphism, a real and separate next step — see this
/// module's scope note.
#[derive(Clone)]
pub struct TypeEnv(Vec<(String, Type)>);

impl TypeEnv {
    pub fn new() -> Self {
        TypeEnv(Vec::new())
    }

    pub fn extend(&self, name: String, ty: Type) -> TypeEnv {
        let mut v = self.0.clone();
        v.push((name, ty));
        TypeEnv(v)
    }

    pub fn lookup(&self, name: &str) -> Option<&Type> {
        self.0.iter().rev().find(|(n, _)| n == name).map(|(_, t)| t)
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
    pub fn apply_subst(&self, subst: &Subst) -> TypeEnv {
        TypeEnv(self.0.iter().map(|(n, t)| (n.clone(), subst.apply(t))).collect())
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

        for item in &program.items {
            if let ast::ItemKind::Let(def) = &item.kind {
                if def.params.is_empty() {
                    return Err(format!(
                        "type inference not yet implemented for zero-parameter top-level \
                         `let` at {:?}",
                        def.span
                    ));
                }
                let param_vars: Vec<Type> = def.params.iter().map(|_| self.fresh()).collect();
                let ret_var = self.fresh();
                let fn_ty = Type::Function(param_vars.clone(), Box::new(ret_var.clone()));
                global_env = global_env.extend(def.name.clone(), fn_ty);
                signatures.insert(def.name.clone(), (param_vars, ret_var));
                defs.push(def);
            }
        }

        // Phase 2: infer each body against the SHARED global env (so
        // it can call itself or any sibling function), threading ONE
        // substitution accumulator across every function — a call from
        // function A into function B's still-fresh signature has to be
        // able to constrain B before B's own body gets processed.
        let mut acc = Subst::empty();
        for def in &defs {
            let (param_vars, ret_var) = signatures.get(&def.name).cloned().expect("just inserted above");
            let mut body_env = global_env.clone();
            for (param, param_ty) in def.params.iter().zip(param_vars.iter()) {
                let name = plain_param_name(param)?;
                body_env = body_env.extend(name, acc.apply(param_ty));
            }
            let (body_ty, s) = self.infer_expr(&def.body, &body_env)?;
            acc = s.compose(&acc);
            let s = unify(&acc.apply(&body_ty), &acc.apply(&ret_var)).map_err(|e| {
                format!("function {:?}: body type does not match its return type: {e}", def.name)
            })?;
            acc = s.compose(&acc);

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
            global_env = global_env.extend(def.name.clone(), resolved_fn_ty);
        }

        let mut result = HashMap::new();
        for (name, (param_vars, ret_var)) in &signatures {
            let params = param_vars.iter().map(|t| acc.apply(t)).collect();
            let ret = acc.apply(ret_var);
            result.insert(name.clone(), Type::Function(params, Box::new(ret)));
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
            ast::Expr::Tuple(_, span) => Err(format!(
                "type inference not yet implemented for non-empty tuples at {span:?}"
            )),
            ast::Expr::Ident(name, span) => {
                let ty = env
                    .lookup(name)
                    .cloned()
                    .ok_or_else(|| format!("unbound variable: {name} at {span:?}"))?;
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
                span,
                ..
            } => Err(format!("type inference not yet implemented for ranges at {span:?}")),
            ast::Expr::Binary { op, lhs, rhs, .. } => self.infer_binary(op, lhs, rhs, env),
            ast::Expr::Block(block, _) => self.infer_block(block, env),
            ast::Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => self.infer_if(cond, then_branch, else_branch, env),
            ast::Expr::Call { callee, args, .. } => {
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
        if spread.is_some() {
            return Err(format!(
                "type inference not yet implemented for struct update/spread syntax at {span:?}"
            ));
        }
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
        for (declared_name, declared_ty) in &declared_fields {
            let Some(value_expr) = by_name.remove(declared_name.as_str()) else {
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
    // restriction as lower.rs's `lower_variant_pattern` — see that
    // function's comment for exactly why (no nested patterns, no
    // "default arm" concept for a bare `_`, etc.).
    fn variant_pattern_info(pattern: &ast::Pattern) -> Result<(String, Vec<String>), String> {
        match pattern {
            ast::Pattern::Variant { path, args, .. } => {
                let tag = path.last().cloned().expect("a path always has at least one segment");
                let mut bindings = Vec::with_capacity(args.len());
                for arg in args {
                    match arg {
                        ast::Pattern::Ident(name, _) => bindings.push(name.clone()),
                        ast::Pattern::Wildcard(_) => bindings.push("_".to_string()),
                        other => {
                            return Err(format!(
                                "type inference not yet implemented for nested patterns inside \
                                 a variant arm's arguments at {:?}",
                                other.span()
                            ));
                        }
                    }
                }
                Ok((tag, bindings))
            }
            other => Err(format!(
                "type inference not yet implemented for this pattern shape as a match arm at {:?}",
                other.span()
            )),
        }
    }

    fn infer_match(&mut self, scrutinee: &ast::Expr, arms: &[ast::MatchArm], env: &TypeEnv) -> Result<(Type, Subst), String> {
        let (scrutinee_ty, s) = self.infer_expr(scrutinee, env)?;
        let mut acc = s;

        let mut result_ty: Option<Type> = None;
        // What the scrutinee is being deconstructed as. A tag matches
        // either a real enum variant OR a whole struct, since lowering
        // erases that distinction (both become a positional `Ctor`/
        // `Match` by tag) — see lower.rs's Pattern::Variant handling,
        // which the parser also uses for `Point(x, y)` against a
        // *struct* named Point, not just real enum variants.
        let mut owning_type: Option<Type> = None;

        for arm in arms {
            if arm.guard.is_some() {
                return Err(format!(
                    "type inference not yet implemented for match guards at {:?}",
                    arm.span
                ));
            }
            let (tag, bindings) = Self::variant_pattern_info(&arm.pattern)?;
            let (this_type, payload_types) = match self.ctx.variant(&tag) {
                Some((enum_name, payload_types)) => (Type::Enum(enum_name.clone()), payload_types.clone()),
                None => match self.ctx.struct_fields(&tag) {
                    Some(fields) => (
                        Type::Struct(tag.clone()),
                        fields.iter().map(|(_, ty)| ty.clone()).collect(),
                    ),
                    None => {
                        return Err(format!("unknown variant {tag:?} at {:?}", arm.pattern.span()));
                    }
                },
            };

            match &owning_type {
                None => owning_type = Some(this_type.clone()),
                Some(prev) if *prev != this_type => {
                    return Err(format!(
                        "match arms mix incompatible types ({prev:?} and {this_type:?})"
                    ));
                }
                _ => {}
            }

            if bindings.len() != payload_types.len() {
                return Err(format!(
                    "variant {tag:?} expects {} field(s), found {} binding(s)",
                    payload_types.len(),
                    bindings.len()
                ));
            }

            let mut arm_env = env.clone();
            for (binding_name, payload_ty) in bindings.iter().zip(payload_types.iter()) {
                arm_env = arm_env.extend(binding_name.clone(), acc.apply(payload_ty));
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

        if let Some(ty) = owning_type {
            let s = unify(&acc.apply(&scrutinee_ty), &ty).map_err(|e| format!("match scrutinee: {e}"))?;
            acc = s.compose(&acc);
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
                Some(annotation) => ast_type_to_type(annotation)?,
                None => self.fresh(),
            };
            closure_env = closure_env.extend(p.name.clone(), ty.clone());
            param_types.push(ty);
        }
        let (body_ty, acc) = self.infer_expr(body, &closure_env)?;
        let resolved_params = param_types.iter().map(|t| acc.apply(t)).collect();
        Ok((Type::Function(resolved_params, Box::new(acc.apply(&body_ty))), acc))
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
                ast::Stmt::Let { pattern, value, ty, .. } => {
                    let name = plain_ident(pattern)?;
                    let (val_ty, s) = self.infer_expr(value, &cur_env)?;
                    acc = s.compose(&acc);
                    let mut resolved = acc.apply(&val_ty);
                    if let Some(annotation) = ty {
                        let ann_ty = ast_type_to_type(annotation)?;
                        let s = unify(&resolved, &ann_ty)
                            .map_err(|e| format!("`let` annotation for {name:?}: {e}"))?;
                        acc = s.compose(&acc);
                        resolved = acc.apply(&resolved);
                    }
                    cur_env = cur_env.extend(name, resolved);
                    cur_env = cur_env.apply_subst(&acc);
                }
                ast::Stmt::Expr(e) => {
                    let (_, s) = self.infer_expr(e, &cur_env)?;
                    acc = s.compose(&acc);
                    cur_env = cur_env.apply_subst(&acc);
                }
                ast::Stmt::Assign { span, .. } => {
                    return Err(format!(
                        "type inference not yet implemented for assignment statements at {span:?}"
                    ));
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

fn plain_ident(pattern: &ast::Pattern) -> Result<String, String> {
    match pattern {
        ast::Pattern::Ident(name, _) => Ok(name.clone()),
        other => Err(format!(
            "type inference not yet implemented for destructuring let-bindings at {:?}",
            other.span()
        )),
    }
}

fn plain_param_name(param: &ast::Param) -> Result<String, String> {
    match &param.kind {
        ast::ParamKind::Ident(name) => Ok(name.clone()),
        ast::ParamKind::Pattern(ast::Pattern::Ident(name, _), _) => Ok(name.clone()),
        _ => Err(format!(
            "type inference not yet implemented for destructuring function parameters at {:?}",
            param.span
        )),
    }
}

// pub(crate) so context.rs can reuse it for struct field / enum
// variant payload type annotations. Deliberately primitive-only for
// now — a field/payload type referencing ANOTHER struct or enum (or a
// generic) is a real, deferred gap (nested declaration ordering is a
// separate problem this pass doesn't solve), not silently mishandled.
pub(crate) fn ast_type_to_type(ty: &ast::Type) -> Result<Type, String> {
    match ty {
        ast::Type::Path(segments, span) => match segments.last().map(String::as_str) {
            Some("Int") => Ok(Type::Int),
            Some("Float") => Ok(Type::Float),
            Some("Bool") => Ok(Type::Bool),
            Some("String") => Ok(Type::Str),
            Some("Unit") => Ok(Type::Unit),
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
    fn struct_literal_spread_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("struct Point { x: Float, y: Float }"));
        infer_expr_with_err(&mut infer, "Point { x: 1.0, ..other }", &TypeEnv::new());
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
    fn match_guard_is_not_yet_supported() {
        let mut infer = Infer::with_context(context("enum Shape { Circle(Float) }"));
        let env = TypeEnv::new().extend("shape".to_string(), Type::Enum("Shape".to_string()));
        infer_expr_with_err(&mut infer, "match shape { Shape.Circle(r) if r > 0.0 => r }", &env);
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

    #[test]
    fn assign_statement_is_not_yet_supported() {
        infer_err("{ let mut x = 5; x = 6; x }");
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

    fn infer_program(src: &str) -> HashMap<String, Type> {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let mut infer = Infer::new();
        infer
            .infer_program(&program)
            .unwrap_or_else(|e| panic!("program inference error for {src:?}: {e}"))
    }

    fn infer_program_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let mut infer = Infer::new();
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
    fn infer_program_call_arity_mismatch_is_an_error() {
        infer_program_err("let double n = n * 2\nlet caller x = double(x, x)");
    }

    #[test]
    fn infer_program_call_type_mismatch_is_an_error() {
        infer_program_err("let want_bool b = !b\nlet caller x = want_bool(x + 1)");
    }

    #[test]
    fn infer_program_zero_param_let_is_not_yet_supported() {
        infer_program_err("let x = 5");
    }
}
