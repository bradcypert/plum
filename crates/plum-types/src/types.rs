// The type representation Hindley-Milner inference solves equations
// over. `Var` is a metavariable — "some type we haven't determined
// yet" — and inference's job is ultimately to find a Subst (see
// subst.rs) that resolves every Var to a concrete type, or to report
// where that's impossible.
//
// Scope note: primitives matching DESIGN.md's unboxed set, Function,
// and NOMINAL struct/enum types (identified by declared name AND type
// arguments — two structs with the same name and matching arguments
// are the same type; identical fields under a DIFFERENT name are
// still different types, matching Rust/most ML languages). `Struct`/
// `Enum` DO carry type parameters (`struct Pair[T] { .. }` is
// `Type::Struct("Pair", vec![T's argument])` at each use) — see
// `Param`'s own doc comment for how a declaration's parameter NAMES
// (`T`) relate to a use site's concrete/metavariable ARGUMENTS.

pub type TypeVarId = usize;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    // The result of `s.as_cstr()` — a null-terminated C string ready to
    // cross the FFI boundary, distinct from `Str` so an ordinary Plum
    // string can't be passed directly to an extern function declared
    // with a `CStr` parameter (see DESIGN.md's "FFI and C interop"
    // section: explicit conversion, not implicit coercion, is the
    // whole point). Otherwise a completely inert type — no operation
    // is defined on it besides being produced by `.as_cstr()` and
    // consumed by a `CStr`-typed extern parameter/return.
    CStr,
    Unit,
    Var(TypeVarId),
    Function(Vec<Type>, Box<Type>),
    // The `Vec<Type>` is the struct's type ARGUMENTS at this use site —
    // empty for a non-generic struct (matching `Enum`/every other
    // nominal type before generics existed), one entry per declared
    // parameter for a generic one, in DECLARED order. See `Param`'s
    // doc comment for how a declaration turns into these.
    Struct(String, Vec<Type>),
    Enum(String, Vec<Type>),
    // A generic struct/enum DECLARATION's own type parameter, named
    // exactly as written (`T` in `struct Pair[T] { first: T, second: T
    // }`) — this is what `TypeContext::struct_fields`/`variants`
    // stores a field/payload's type AS, whenever that field's
    // annotation mentions one of the struct's own declared params.
    // Deliberately NOT a `Var`: a declaration is inferred exactly
    // ONCE, up front, with no unification of its own to solve — `Var`
    // is reserved for actual inference metavariables a `Subst` will
    // eventually resolve, and reusing it here would risk exactly the
    // kind of accidental id collision this codebase has hit before
    // (see plum-types' test-authoring notes on hand-picked `Var` ids).
    // A `Param` is only ever meant to exist TRANSIENTLY inside a
    // stored declaration template — every real USE (a struct literal,
    // pattern, or variant construction) immediately substitutes each
    // `Param` for a FRESH `Var` scoped to that one use (mirroring how
    // a polymorphic function's `Scheme` is instantiated fresh at each
    // call site), so a `Param` should never reach `unify`/`Subst` in
    // practice — `unify.rs` treats seeing one as an internal-error
    // defensive case, not a real type mismatch.
    Param(String),
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
