// The type representation Hindley-Milner inference solves equations
// over. `Var` is a metavariable — "some type we haven't determined
// yet" — and inference's job is ultimately to find a Subst (see
// subst.rs) that resolves every Var to a concrete type, or to report
// where that's impossible.
//
// Scope note: primitives matching DESIGN.md's unboxed set, Function,
// and NOMINAL struct/enum types (identified by declared name only, no
// structural comparison — two structs with identical fields are still
// different types, matching Rust/most ML languages). Deliberately NOT
// generic: `Struct`/`Enum` carry no type parameters, and
// `TypeContext::from_items` (context.rs) rejects any struct/enum
// declared with generics rather than pretending to erase them — unlike
// a function's unused generic parameter, a generic struct FIELD's type
// genuinely depends on the parameter, so there's nothing safe to erase.

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
    Struct(String),
    Enum(String),
    // `start..end` as a first-class value, not just `for`'s iterand.
    // No type parameter: every range is an `Int` range (there's no
    // `Float`/other-bound range anywhere in the language yet — see
    // `infer_expr`'s Range handling and `lower_for`'s bound-type
    // restriction), so this is a plain nominal-like marker, the same
    // shape as `Unit`.
    Range,
    // Unlike Struct/Enum, tuples are STRUCTURAL, not nominal — there's
    // no declaration to name them, so two tuple types are equal
    // exactly when their element types are (see unify.rs). The empty
    // tuple isn't represented here at all: `()` is Unit itself (see
    // DESIGN.md's "Tuples and closures"), not `Tuple(vec![])` — a
    // separate, redundant zero-element case would just be two spellings
    // of the same type.
    Tuple(Vec<Type>),
}
