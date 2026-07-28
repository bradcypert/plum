use crate::subst::Subst;
use crate::types::{Type, TypeVarId};

/// Finds a substitution that makes `a` and `b` equal, or reports why
/// none exists. This is the equation-solving step Hindley-Milner
/// inference is built on — see this crate's module docs / DESIGN.md
/// for the bigger picture.
pub fn unify(a: &Type, b: &Type) -> Result<Subst, String> {
    match (a, b) {
        (Type::Int, Type::Int)
        | (Type::Float, Type::Float)
        | (Type::Bool, Type::Bool)
        | (Type::Str, Type::Str)
        | (Type::Unit, Type::Unit)
        | (Type::Range, Type::Range) => Ok(Subst::empty()),

        (Type::Struct(a_name), Type::Struct(b_name)) if a_name == b_name => Ok(Subst::empty()),
        (Type::Enum(a_name), Type::Enum(b_name)) if a_name == b_name => Ok(Subst::empty()),

        // Checked before the general Var arms below so that unifying a
        // variable with itself is recognized as a no-op rather than
        // routed through bind_var (which would still handle it
        // correctly via its own self-binding check, but matching it
        // here directly is clearer about why it's fine).
        (Type::Var(a_id), Type::Var(b_id)) if a_id == b_id => Ok(Subst::empty()),

        (Type::Var(id), other) | (other, Type::Var(id)) => bind_var(*id, other),

        (Type::Function(p1, r1), Type::Function(p2, r2)) => {
            if p1.len() != p2.len() {
                return Err(format!(
                    "function arity mismatch: expected {} argument(s), found {}",
                    p1.len(),
                    p2.len()
                ));
            }
            let mut subst = Subst::empty();
            for (t1, t2) in p1.iter().zip(p2.iter()) {
                let step = unify(&subst.apply(t1), &subst.apply(t2))?;
                subst = step.compose(&subst);
            }
            let step = unify(&subst.apply(r1), &subst.apply(r2))?;
            Ok(step.compose(&subst))
        }

        // Structural, unlike Struct/Enum: two tuple types unify exactly
        // when they have the same arity AND every element unifies
        // pairwise — no declared name to compare instead.
        (Type::Tuple(e1), Type::Tuple(e2)) => {
            if e1.len() != e2.len() {
                return Err(format!(
                    "tuple arity mismatch: expected {}-tuple, found {}-tuple",
                    e1.len(),
                    e2.len()
                ));
            }
            let mut subst = Subst::empty();
            for (t1, t2) in e1.iter().zip(e2.iter()) {
                let step = unify(&subst.apply(t1), &subst.apply(t2))?;
                subst = step.compose(&subst);
            }
            Ok(subst)
        }

        (a, b) => Err(format!("type mismatch: expected {a:?}, found {b:?}")),
    }
}

fn bind_var(id: TypeVarId, ty: &Type) -> Result<Subst, String> {
    if occurs(id, ty) {
        return Err(format!("infinite type: T{id} occurs within {ty:?}"));
    }
    Ok(Subst::single(id, ty.clone()))
}

/// True if the type variable `id` appears anywhere inside `ty`.
/// Binding a variable to a type that contains itself would describe an
/// infinite type (`T = T -> Int` has no finite solution) — this is
/// what unify rejects instead of looping forever or producing nonsense.
fn occurs(id: TypeVarId, ty: &Type) -> bool {
    match ty {
        Type::Var(other) => *other == id,
        Type::Function(params, ret) => params.iter().any(|p| occurs(id, p)) || occurs(id, ret),
        Type::Tuple(elems) => elems.iter().any(|e| occurs(id, e)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fn_ty(params: Vec<Type>, ret: Type) -> Type {
        Type::Function(params, Box::new(ret))
    }

    #[test]
    fn identical_primitives_unify_with_no_bindings() {
        assert_eq!(unify(&Type::Int, &Type::Int).unwrap(), Subst::empty());
        assert_eq!(unify(&Type::Bool, &Type::Bool).unwrap(), Subst::empty());
    }

    #[test]
    fn mismatched_primitives_are_a_type_error() {
        assert!(unify(&Type::Int, &Type::Bool).is_err());
    }

    #[test]
    fn a_variable_unifies_with_a_concrete_type_by_binding() {
        let s = unify(&Type::Var(0), &Type::Int).unwrap();
        assert_eq!(s.apply(&Type::Var(0)), Type::Int);
    }

    #[test]
    fn unification_is_symmetric() {
        let s = unify(&Type::Int, &Type::Var(0)).unwrap();
        assert_eq!(s.apply(&Type::Var(0)), Type::Int);
    }

    #[test]
    fn a_variable_unified_with_itself_binds_nothing() {
        assert_eq!(unify(&Type::Var(0), &Type::Var(0)).unwrap(), Subst::empty());
    }

    #[test]
    fn two_different_variables_unify_by_binding_the_left_to_the_right() {
        let s = unify(&Type::Var(0), &Type::Var(1)).unwrap();
        assert_eq!(s.apply(&Type::Var(0)), Type::Var(1));
    }

    #[test]
    fn occurs_check_rejects_infinite_types() {
        // T0 unified with (T0 -> Int) would mean T0 = (T0 -> Int),
        // which has no finite solution.
        let err = unify(&Type::Var(0), &fn_ty(vec![Type::Var(0)], Type::Int))
            .expect_err("expected an infinite-type error");
        assert!(err.contains("infinite"), "expected an infinite-type error, got: {err}");
    }

    #[test]
    fn matching_function_types_unify() {
        let f = fn_ty(vec![Type::Int], Type::Bool);
        assert_eq!(unify(&f, &f).unwrap(), Subst::empty());
    }

    #[test]
    fn matching_struct_types_unify() {
        assert_eq!(
            unify(&Type::Struct("Point".to_string()), &Type::Struct("Point".to_string())).unwrap(),
            Subst::empty()
        );
    }

    #[test]
    fn different_struct_types_are_an_error() {
        assert!(unify(&Type::Struct("Point".to_string()), &Type::Struct("Vector".to_string())).is_err());
    }

    #[test]
    fn matching_enum_types_unify() {
        assert_eq!(
            unify(&Type::Enum("Shape".to_string()), &Type::Enum("Shape".to_string())).unwrap(),
            Subst::empty()
        );
    }

    #[test]
    fn struct_and_enum_with_the_same_name_do_not_unify() {
        // Nominal typing is per-KIND, not just per-name — a struct and
        // an enum sharing a name (unusual, but not forbidden) are
        // still different types.
        assert!(unify(&Type::Struct("Thing".to_string()), &Type::Enum("Thing".to_string())).is_err());
    }

    #[test]
    fn matching_tuple_types_unify() {
        let t = Type::Tuple(vec![Type::Int, Type::Bool]);
        assert_eq!(unify(&t, &t).unwrap(), Subst::empty());
    }

    #[test]
    fn tuples_unify_structurally_element_by_element() {
        let a = Type::Tuple(vec![Type::Var(0), Type::Int]);
        let b = Type::Tuple(vec![Type::Bool, Type::Var(1)]);
        let s = unify(&a, &b).unwrap();
        assert_eq!(s.apply(&Type::Var(0)), Type::Bool);
        assert_eq!(s.apply(&Type::Var(1)), Type::Int);
    }

    #[test]
    fn mismatched_tuple_arity_is_an_error() {
        let a = Type::Tuple(vec![Type::Int, Type::Int]);
        let b = Type::Tuple(vec![Type::Int, Type::Int, Type::Int]);
        assert!(unify(&a, &b).is_err());
    }

    #[test]
    fn mismatched_tuple_element_types_are_an_error() {
        let a = Type::Tuple(vec![Type::Int, Type::Bool]);
        let b = Type::Tuple(vec![Type::Int, Type::Str]);
        assert!(unify(&a, &b).is_err());
    }

    #[test]
    fn tuple_and_struct_do_not_unify() {
        assert!(unify(&Type::Tuple(vec![Type::Int]), &Type::Struct("Point".to_string())).is_err());
    }

    #[test]
    fn occurs_check_rejects_infinite_types_inside_tuples() {
        let err = unify(&Type::Var(0), &Type::Tuple(vec![Type::Var(0), Type::Int]))
            .expect_err("expected an infinite-type error");
        assert!(err.contains("infinite"), "expected an infinite-type error, got: {err}");
    }

    #[test]
    fn mismatched_function_arity_is_an_error() {
        let f1 = fn_ty(vec![Type::Int], Type::Bool);
        let f2 = fn_ty(vec![Type::Int, Type::Int], Type::Bool);
        assert!(unify(&f1, &f2).is_err());
    }

    #[test]
    fn unifying_a_polymorphic_function_type_pins_every_occurrence_of_the_same_variable() {
        // The example from this crate's design discussion: unifying
        // (T0 -> T0) with (Int -> T1) must conclude BOTH T0 = Int AND
        // T1 = Int — proving substitutions learned from the parameter
        // correctly carry forward into solving the return type, not
        // just be discarded. This is the whole reason `compose` has to
        // be threaded through, not just the last unification kept.
        let a = fn_ty(vec![Type::Var(0)], Type::Var(0));
        let b = fn_ty(vec![Type::Int], Type::Var(1));
        let s = unify(&a, &b).unwrap();
        assert_eq!(s.apply(&Type::Var(0)), Type::Int);
        assert_eq!(s.apply(&Type::Var(1)), Type::Int);
    }
}
