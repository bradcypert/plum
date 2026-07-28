use crate::types::{Type, TypeVarId};
use std::collections::HashMap;

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
        let mut result: HashMap<TypeVarId, Type> =
            other.0.iter().map(|(k, v)| (*k, self.apply(v))).collect();
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
}
