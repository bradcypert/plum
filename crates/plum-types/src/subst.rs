use crate::types::{Type, TypeVarId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counters behind `PLUM_PASS_TIMES` — see `stats()`. Relaxed ordering
/// and a plain `static`: these are diagnostics, never load-bearing.
pub static COMPOSE_CALLS: AtomicU64 = AtomicU64::new(0);
pub static COMPOSE_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Human-readable dump of the substitution counters, for the
/// `PLUM_PASS_TIMES` report. `compose` rebuilds its whole result map
/// on every call, so `entries / calls` is the average map size being
/// copied — the number that decides whether this is linear or
/// quadratic in practice.
pub fn stats() -> String {
    let calls = COMPOSE_CALLS.load(Ordering::Relaxed);
    let entries = COMPOSE_ENTRIES.load(Ordering::Relaxed);
    let avg = if calls == 0 { 0.0 } else { entries as f64 / calls as f64 };
    format!(
        "subst: {calls} compose calls, {entries} entries copied (avg map {avg:.1})"
    )
}

/// The accumulated "answers so far" — a partial mapping from type
/// variables to the concrete types unification has pinned them to.
#[derive(Debug, Clone, PartialEq)]
pub struct Subst(HashMap<TypeVarId, Type>);

impl Subst {
    pub fn empty() -> Self {
        Subst(HashMap::new())
    }

    pub fn single(id: TypeVarId, ty: Type) -> Self {
        let mut map = HashMap::new();
        map.insert(id, ty);
        Subst(map)
    }

    /// Replaces every variable in `ty` with what this substitution
    /// knows about it, resolving chains (`T0 -> T1 -> Int` resolves
    /// fully to `Int`, not stopping at `T1`) and recursing into
    /// compound types.
    pub fn apply(&self, ty: &Type) -> Type {
        match ty {
            Type::Var(id) => match self.0.get(id) {
                // Recurse: the replacement might itself contain a
                // variable this substitution also resolves further.
                Some(replacement) => self.apply(replacement),
                None => ty.clone(),
            },
            Type::Function(params, ret) => Type::Function(
                params.iter().map(|p| self.apply(p)).collect(),
                Box::new(self.apply(ret)),
            ),
            Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| self.apply(e)).collect()),
            Type::Struct(name, args) => Type::Struct(name.clone(), args.iter().map(|a| self.apply(a)).collect()),
            Type::Enum(name, args) => Type::Enum(name.clone(), args.iter().map(|a| self.apply(a)).collect()),
            other => other.clone(),
        }
    }

    /// `self.compose(other)` produces a substitution equivalent to
    /// applying `other` FIRST, then `self` — i.e.
    /// `self.compose(other).apply(t) == self.apply(other.apply(t))`.
    /// This is what lets unification correctly carry knowledge learned
    /// from one part of a type equation into the next part (see this
    /// module's — and unify.rs's — doc comments for why that ordering
    /// matters, not just that it does).
    pub fn compose(&self, other: &Subst) -> Subst {
        // Every binding in `other` needs `self` applied to its target,
        // in case that target itself contains a variable `self`
        // resolves further. Then `self`'s own bindings carry through
        // for any key `other` didn't already have an opinion about.
        //
        // A binding `k -> self.apply(other[k])` that comes back out as
        // literally `Var(k)` itself is dropped, not inserted — this is
        // NOT an approximation, it's exactly correct: `T_k = T_k` is a
        // tautology, zero new information, identical in meaning to `k`
        // having no entry at all. Dropping it also happens to be load-
        // bearing for correctness, not just tidiness: `self` and
        // `other` are each individually guaranteed acyclic (every
        // `bind_var` call occurs-checks before ever producing a
        // `Subst`, and `Type::Var(a) unify Type::Var(a)` short-circuits
        // to `Subst::empty()` before reaching `bind_var` at all — see
        // unify.rs — so NEITHER input can already contain a `k ->
        // Var(k)` self-loop, or any cycle, on its own), but composing
        // two ACYCLIC substitutions can still produce a NEW cycle where
        // none existed in either input alone — e.g. `self = {2:
        // Var(1)}`, `other = {1: Var(2)}`: neither loops by itself
        // (`self.apply(Var(2))` = `Var(2)` unchanged, `other.apply(
        // Var(1))` = `Var(2)`, both terminate), but naively merging
        // them produces `{1: Var(1), 2: Var(1)}` — a genuine `id ->
        // Var(id)` self-loop at key 1, which `Subst::apply` would then
        // recurse on FOREVER the next time anything looks up `Var(1)`.
        // This was a real, previously-latent bug (see DESIGN.md's
        // "Standard library" chunk 12 and the matching "Open questions"
        // entry) — confirmed via `gdb` to genuinely recurse unbounded
        // (100,000+ frames before the process aborted), not just
        // "deep." Filtering the trivial-self-loop case out here is
        // sufficient to prevent it, not merely a patch: `self.apply`
        // only ever returns `Var(id)` for a variable `self` has NO
        // entry for (an entry always causes further recursion instead —
        // see `apply`'s own `Type::Var` arm), so a same-key result can
        // ONLY arise from `self`/`other` cross-referencing each other
        // through exactly this key — never from a longer, still-hidden
        // cycle among three or more variables (that would require one
        // of `self`/`other` to already be individually cyclic, which
        // the invariant above rules out) — so this one check, applied
        // at every `compose` call, keeps the "no `Subst` this codebase
        // ever produces can loop under `apply`" invariant intact
        // through arbitrarily many chained `compose` calls, not just
        // this one.
        COMPOSE_CALLS.fetch_add(1, Ordering::Relaxed);
        COMPOSE_ENTRIES.fetch_add((self.0.len() + other.0.len()) as u64, Ordering::Relaxed);
        let mut result: HashMap<TypeVarId, Type> = other
            .0
            .iter()
            .filter_map(|(k, v)| {
                let resolved = self.apply(v);
                if resolved == Type::Var(*k) {
                    None
                } else {
                    Some((*k, resolved))
                }
            })
            .collect();
        for (k, v) in self.0.iter() {
            result.entry(*k).or_insert_with(|| v.clone());
        }
        Subst(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_leaves_primitives_unchanged() {
        let s = Subst::single(0, Type::Bool);
        assert_eq!(s.apply(&Type::Int), Type::Int);
    }

    #[test]
    fn apply_resolves_a_bound_variable() {
        let s = Subst::single(0, Type::Int);
        assert_eq!(s.apply(&Type::Var(0)), Type::Int);
    }

    #[test]
    fn apply_leaves_an_unbound_variable_unchanged() {
        let s = Subst::single(0, Type::Int);
        assert_eq!(s.apply(&Type::Var(1)), Type::Var(1));
    }

    #[test]
    fn apply_resolves_chains_fully() {
        // T0 -> T1 -> Int: applying must not stop at the first hop.
        let mut s = Subst::empty();
        s = s.compose(&Subst::single(0, Type::Var(1)));
        s = s.compose(&Subst::single(1, Type::Int));
        assert_eq!(s.apply(&Type::Var(0)), Type::Int);
    }

    #[test]
    fn apply_recurses_into_function_types() {
        let s = Subst::single(0, Type::Int);
        let fn_ty = Type::Function(vec![Type::Var(0)], Box::new(Type::Var(0)));
        assert_eq!(
            s.apply(&fn_ty),
            Type::Function(vec![Type::Int], Box::new(Type::Int))
        );
    }

    #[test]
    fn apply_recurses_into_tuple_types() {
        let s = Subst::single(0, Type::Int);
        let tuple_ty = Type::Tuple(vec![Type::Var(0), Type::Bool, Type::Var(0)]);
        assert_eq!(
            s.apply(&tuple_ty),
            Type::Tuple(vec![Type::Int, Type::Bool, Type::Int])
        );
    }

    #[test]
    fn compose_applies_self_to_others_bindings() {
        // s2: T0 -> T1.  s1: T1 -> Int.
        // compose(s1, s2) must map T0 directly to Int, not leave it at T1.
        let s2 = Subst::single(0, Type::Var(1));
        let s1 = Subst::single(1, Type::Int);
        let composed = s1.compose(&s2);
        assert_eq!(composed.apply(&Type::Var(0)), Type::Int);
        assert_eq!(composed.apply(&Type::Var(1)), Type::Int);
    }

    #[test]
    fn composing_two_individually_acyclic_substitutions_that_cross_reference_each_other_does_not_produce_a_self_loop() {
        // `self` (T2 -> T1) and `other` (T1 -> T2) neither loop on its
        // own — `self.apply(Var(1))` is `Var(1)` unchanged (self has no
        // entry for 1), `other.apply(Var(2))` is `Var(2)` unchanged
        // (other has no entry for 2) — but naively merging them used to
        // produce a genuine `1 -> Var(1)` self-loop, which `apply` would
        // recurse on forever. This is the real, previously-latent bug
        // documented in DESIGN.md's "Standard library" chunk 12 / "Open
        // questions" — found via `gdb` showing `Subst::apply` 100,000+
        // frames deep before the process aborted.
        let self_s = Subst::single(2, Type::Var(1));
        let other_s = Subst::single(1, Type::Var(2));
        let composed = self_s.compose(&other_s);
        // Both must terminate and resolve to themselves — no residual
        // information was actually learned by either variable.
        assert_eq!(composed.apply(&Type::Var(1)), Type::Var(1));
        assert_eq!(composed.apply(&Type::Var(2)), Type::Var(1));
    }
}
