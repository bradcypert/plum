// The type representation Hindley-Milner inference solves equations
// over. `Var` is a metavariable — "some type we haven't determined
// yet" — and inference's job is ultimately to find a Subst (see
// subst.rs) that resolves every Var to a concrete type, or to report
// where that's impossible.
//
// Scope note: primitives matching DESIGN.md's unboxed set, plus
// Function — enough to make unification's occurs-check and recursive
// substitution genuinely meaningful (see subst.rs/unify.rs), without
// yet reaching generics, structs, or enums as their own type formers.
// Those come once real expression-by-expression inference exists on
// top of this.

pub type TypeVarId = usize;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    Var(TypeVarId),
    Function(Vec<Type>, Box<Type>),
}
