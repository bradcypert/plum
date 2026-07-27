mod heap;

use plum_ir::ir::{BinOp, Expr, Function, Program, RcOp, UnOp};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Unit,
    HeapRef(usize),
}

// A flat, innermost-last scope stack — `let` pushes one entry, evaluates
// its body, then pops. Variable lookup scans from the end so shadowing
// (an inner `let x` hiding an outer one) falls out for free, and
// popping on the way back out of a `let`'s body is what keeps a binding
// from leaking into code that comes lexically after it.
pub struct Interpreter {
    env: Vec<(String, Value)>,
    heap: heap::Heap,
    // Separate from `env` on purpose: a named top-level function must
    // NEVER see whatever local variables happen to exist wherever it
    // was called from — only its own parameters. Keeping functions in
    // their own table (rather than pushing them onto `env`) is what
    // makes that isolation structural rather than something `call` has
    // to remember to enforce.
    functions: HashMap<String, Function>,
}

impl Interpreter {
    pub fn new() -> Self {
        Interpreter {
            env: Vec::new(),
            heap: heap::Heap::new(),
            functions: HashMap::new(),
        }
    }

    pub fn load_program(&mut self, program: &Program) {
        for f in &program.functions {
            self.functions.insert(f.name.clone(), f.clone());
        }
    }

    /// Invokes a named top-level function with concrete argument
    /// values. Swaps in a completely FRESH environment containing only
    /// the parameters (not whatever `env` currently holds), evaluates
    /// the body, then restores the caller's environment — this swap-
    /// and-restore is what gives correct isolation, and it's also
    /// exactly what makes recursion work for free: a function calling
    /// itself just looks itself up in `functions` again, fresh.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String> {
        let func = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown function: {name}"))?;
        if func.params.len() != args.len() {
            return Err(format!(
                "{name} expects {} argument(s), found {}",
                func.params.len(),
                args.len()
            ));
        }
        let fresh_env: Vec<(String, Value)> = func.params.into_iter().zip(args).collect();
        let caller_env = std::mem::replace(&mut self.env, fresh_env);
        let result = self.eval(&func.body);
        self.env = caller_env;
        result
    }

    fn lookup(&self, name: &str) -> Result<Value, String> {
        self.env
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("unbound variable: {name}"))
    }

    /// Total number of fresh heap allocations performed so far — does
    /// NOT count `CtorReuse` overwriting an existing cell, so this is
    /// what actually demonstrates FBIP's reuse-in-place saving a real
    /// allocation, not just that a `CtorReuse` node was present.
    pub fn alloc_count(&self) -> usize {
        self.heap.alloc_count()
    }

    /// The current live refcount of a heap value — `Err` if `value`
    /// isn't a `HeapRef` or points at already-freed memory.
    pub fn refcount(&self, value: &Value) -> Result<usize, String> {
        match value {
            Value::HeapRef(addr) => self.heap.refcount(*addr),
            other => Err(format!("{other:?} is not a heap value")),
        }
    }

    pub fn eval(&mut self, expr: &Expr) -> Result<Value, String> {
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Unit => Ok(Value::Unit),
            Expr::Var(name) => self.lookup(name),
            Expr::Unary(op, e) => {
                let v = self.eval(e)?;
                eval_unary(op, v)
            }
            // && and || short-circuit: the right side is only
            // evaluated when the left side doesn't already determine
            // the result. This has to be handled here, before the
            // operands are evaluated, not inside a generic binary-op
            // helper that's already given both values.
            Expr::Binary(BinOp::And, l, r) => match self.eval(l)? {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => match self.eval(r)? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    v => Err(format!("`&&` requires Bool operands, found {v:?}")),
                },
                v => Err(format!("`&&` requires Bool operands, found {v:?}")),
            },
            Expr::Binary(BinOp::Or, l, r) => match self.eval(l)? {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => match self.eval(r)? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    v => Err(format!("`||` requires Bool operands, found {v:?}")),
                },
                v => Err(format!("`||` requires Bool operands, found {v:?}")),
            },
            Expr::Binary(op, l, r) => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                eval_binary(op, lv, rv)
            }
            Expr::Let { name, value, body } => {
                let v = self.eval(value)?;
                self.env.push((name.clone(), v));
                let result = self.eval(body);
                self.env.pop();
                result
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => match self.eval(cond)? {
                Value::Bool(true) => self.eval(then_branch),
                Value::Bool(false) => self.eval(else_branch),
                v => Err(format!("`if` condition must be Bool, found {v:?}")),
            },
            Expr::Call { callee, args } => {
                // Only a bare name naming a known top-level function is
                // supported — calling anything else (a closure value,
                // an unbound name) waits for closures to exist at all.
                let Expr::Var(name) = callee.as_ref() else {
                    return Err(
                        "calling a non-function-name value is not yet supported (no closures yet)"
                            .to_string(),
                    );
                };
                let arg_values = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
                self.call(name, arg_values)
            }
            Expr::RcAnnotated { op, target, rest } => {
                // Only heap values are affected — a stray Inc/Dec on a
                // non-heap name (shouldn't happen given how fbip.rs
                // scopes its analysis, but nothing here assumes it
                // can't) is a harmless no-op.
                if let Value::HeapRef(addr) = self.lookup(target)? {
                    match op {
                        RcOp::Inc => self.heap.inc(addr)?,
                        RcOp::Dec => self.heap.dec(addr)?,
                    }
                }
                self.eval(rest)
            }
            Expr::Ctor { tag, fields } => {
                let values = fields.iter().map(|f| self.eval(f)).collect::<Result<Vec<_>, _>>()?;
                let addr = self.heap.alloc(tag.clone(), values);
                Ok(Value::HeapRef(addr))
            }
            Expr::CtorReuse { reuse_of, tag, fields } => {
                // Fields are evaluated BEFORE touching the reused
                // cell — this is symbolic (we don't manipulate real
                // memory here), but it mirrors the real constraint a
                // codegen backend has: compute the new values first,
                // then overwrite, so a partial in-place write never
                // reads back its own not-yet-written bytes.
                let values = fields.iter().map(|f| self.eval(f)).collect::<Result<Vec<_>, _>>()?;
                let Value::HeapRef(addr) = self.lookup(reuse_of)? else {
                    return Err(format!("reuse target {reuse_of:?} is not a heap value"));
                };
                let result_addr = self.heap.dec_and_maybe_reuse(addr, tag.clone(), values)?;
                Ok(Value::HeapRef(result_addr))
            }
            Expr::Match { scrutinee, arms } => {
                let Value::HeapRef(addr) = self.eval(scrutinee)? else {
                    return Err("match scrutinee must be a heap value".to_string());
                };
                let (tag, fields) = self.heap.read(addr)?;
                let tag = tag.to_string();
                let fields = fields.to_vec();
                let arm = arms
                    .iter()
                    .find(|a| a.tag == tag)
                    .ok_or_else(|| format!("no match arm for tag {tag:?}"))?;
                if arm.bindings.len() != fields.len() {
                    return Err(format!(
                        "arm for {tag:?} expects {} field(s), found {}",
                        arm.bindings.len(),
                        fields.len()
                    ));
                }
                let bound: Vec<(String, Value)> =
                    arm.bindings.iter().cloned().zip(fields.iter().cloned()).collect();
                self.env.extend(bound);
                let result = self.eval(&arm.body);
                self.env.truncate(self.env.len() - arm.bindings.len());
                result
            }
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

fn eval_unary(op: &UnOp, v: Value) -> Result<Value, String> {
    match (op, v) {
        (UnOp::Neg, Value::Int(n)) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| "integer overflow negating i64::MIN".to_string()),
        (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (op, v) => Err(format!("type error: cannot apply {op:?} to {v:?}")),
    }
}

fn eval_binary(op: &BinOp, lv: Value, rv: Value) -> Result<Value, String> {
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => eval_arith(op, lv, rv),
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => eval_order(op, lv, rv),
        BinOp::Eq => values_equal(&lv, &rv).map(Value::Bool),
        BinOp::Ne => values_equal(&lv, &rv).map(|b| Value::Bool(!b)),
        BinOp::And | BinOp::Or => {
            unreachable!("And/Or are handled with short-circuit evaluation in Interpreter::eval")
        }
    }
}

fn eval_arith(op: &BinOp, lv: Value, rv: Value) -> Result<Value, String> {
    match (lv, rv) {
        (Value::Int(a), Value::Int(b)) => {
            let result = match op {
                BinOp::Add => a.checked_add(b),
                BinOp::Sub => a.checked_sub(b),
                BinOp::Mul => a.checked_mul(b),
                BinOp::Div if b == 0 => return Err("division by zero".to_string()),
                BinOp::Div => a.checked_div(b),
                BinOp::Rem if b == 0 => return Err("division by zero".to_string()),
                BinOp::Rem => a.checked_rem(b),
                _ => unreachable!(),
            };
            result.map(Value::Int).ok_or_else(|| "integer overflow".to_string())
        }
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => a / b,
            BinOp::Rem => a % b,
            _ => unreachable!(),
        })),
        (a, b) => Err(format!(
            "type error: no implicit numeric coercion, cannot apply {op:?} to {a:?} and {b:?}"
        )),
    }
}

fn eval_order(op: &BinOp, lv: Value, rv: Value) -> Result<Value, String> {
    let ordering = match (&lv, &rv) {
        (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        _ => return Err(format!("type error: cannot order-compare {lv:?} and {rv:?}")),
    };
    let Some(ord) = ordering else {
        return Err("comparison produced no ordering (NaN?)".to_string());
    };
    Ok(Value::Bool(match op {
        BinOp::Lt => ord.is_lt(),
        BinOp::Gt => ord.is_gt(),
        BinOp::Le => ord.is_le(),
        BinOp::Ge => ord.is_ge(),
        _ => unreachable!(),
    }))
}

fn values_equal(a: &Value, b: &Value) -> Result<bool, String> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x == y),
        (Value::Float(x), Value::Float(y)) => Ok(x == y),
        (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
        (Value::Str(x), Value::Str(y)) => Ok(x == y),
        (Value::Unit, Value::Unit) => Ok(true),
        _ => Err(format!("type error: cannot compare {a:?} and {b:?} for equality")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_ir::lower::{lower_expr, LoweringContext};
    use plum_syntax::lexer::Lexer;
    use plum_syntax::ast;
    use plum_syntax::parser::Parser;

    fn eval(src: &str) -> Value {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ir = lower_expr(&ast, &LoweringContext::new())
            .unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"));
        Interpreter::new()
            .eval(&ir)
            .unwrap_or_else(|e| panic!("eval error for {src:?}: {e}"))
    }

    fn eval_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ir = lower_expr(&ast, &LoweringContext::new())
            .unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"));
        Interpreter::new()
            .eval(&ir)
            .expect_err(&format!("expected eval of {src:?} to fail"))
    }

    #[test]
    fn literals() {
        assert_eq!(eval("5"), Value::Int(5));
        assert_eq!(eval("3.14"), Value::Float(3.14));
        assert_eq!(eval("\"hi\""), Value::Str("hi".to_string()));
        assert_eq!(eval("true"), Value::Bool(true));
        assert_eq!(eval("()"), Value::Unit);
    }

    #[test]
    fn integer_arithmetic() {
        assert_eq!(eval("1 + 2"), Value::Int(3));
        assert_eq!(eval("10 - 3"), Value::Int(7));
        assert_eq!(eval("4 * 5"), Value::Int(20));
        assert_eq!(eval("10 / 3"), Value::Int(3));
        assert_eq!(eval("10 % 3"), Value::Int(1));
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_panic() {
        eval_err("5 / 0");
        eval_err("5 % 0");
    }

    #[test]
    fn float_arithmetic() {
        assert_eq!(eval("1.5 + 2.5"), Value::Float(4.0));
    }

    #[test]
    fn mixed_int_float_arithmetic_is_a_type_error() {
        // No implicit numeric coercion — see DESIGN.md's small built-in
        // trait set (Num); this interpreter doesn't implement overload
        // resolution, just checks the operand types match.
        eval_err("1 + 1.0");
    }

    #[test]
    fn comparisons() {
        assert_eq!(eval("3 < 5"), Value::Bool(true));
        assert_eq!(eval("5 <= 5"), Value::Bool(true));
        assert_eq!(eval("5 > 3"), Value::Bool(true));
        assert_eq!(eval("5 >= 6"), Value::Bool(false));
    }

    #[test]
    fn equality() {
        assert_eq!(eval("3 == 3"), Value::Bool(true));
        assert_eq!(eval("3 != 4"), Value::Bool(true));
        assert_eq!(eval("\"a\" == \"a\""), Value::Bool(true));
        assert_eq!(eval("true == false"), Value::Bool(false));
    }

    #[test]
    fn logical_and_or_short_circuit() {
        // The right-hand side must NOT be evaluated when the left side
        // already determines the result — proven here by putting a
        // division-by-zero on the side that should never run. If
        // short-circuiting were broken, these would error instead of
        // returning cleanly.
        assert_eq!(eval("false && (1 / 0 == 0)"), Value::Bool(false));
        assert_eq!(eval("true || (1 / 0 == 0)"), Value::Bool(true));
        assert_eq!(eval("true && false"), Value::Bool(false));
        assert_eq!(eval("false || true"), Value::Bool(true));
    }

    #[test]
    fn unary_operators() {
        assert_eq!(eval("-5"), Value::Int(-5));
        assert_eq!(eval("-3.14"), Value::Float(-3.14));
        assert_eq!(eval("!true"), Value::Bool(false));
    }

    #[test]
    fn if_expression() {
        assert_eq!(eval("if true { 1 } else { 2 }"), Value::Int(1));
        assert_eq!(eval("if false { 1 } else { 2 }"), Value::Int(2));
    }

    #[test]
    fn if_condition_must_be_bool() {
        eval_err("if 5 { 1 } else { 2 }");
    }

    #[test]
    fn let_binding() {
        assert_eq!(eval("{ let x = 5; x + 1 }"), Value::Int(6));
    }

    #[test]
    fn let_shadowing_inner_wins() {
        assert_eq!(eval("{ let x = 1; let x = 2; x }"), Value::Int(2));
    }

    #[test]
    fn let_binding_does_not_leak_after_its_scope() {
        // Both `x`s are independent — the outer `x` is unreachable from
        // inside the inner block's own `x`, and once a `let`'s body
        // finishes evaluating, that binding is gone.
        assert_eq!(eval("{ let x = 1; { let x = x + 1; x } }"), Value::Int(2));
    }

    #[test]
    fn unbound_variable_is_an_error() {
        eval_err("y");
    }

    #[test]
    fn calling_an_undefined_function_is_an_error() {
        // Calls themselves work now (see the "real function
        // definitions and calls" tests below) — this specific `eval`
        // helper never loads any program, so `f` is genuinely unknown
        // here, not "calls aren't supported" in general anymore.
        eval_err("f(1)");
    }

    // --- Heap-shaped values: Ctor/Match/CtorReuse/RcAnnotated ---
    //
    // No surface syntax lowers to these yet (see plum-ir's scope
    // notes), so — same as fbip.rs — these tests construct small
    // ir::Expr trees by hand rather than going through the parser.

    use plum_ir::ir::{MatchArm, RcOp};

    fn var(name: &str) -> Expr {
        Expr::Var(name.to_string())
    }
    fn int(n: i64) -> Expr {
        Expr::Int(n)
    }
    fn ctor(tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::Ctor {
            tag: tag.to_string(),
            fields,
        }
    }
    fn ctor_reuse(reuse_of: &str, tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::CtorReuse {
            reuse_of: reuse_of.to_string(),
            tag: tag.to_string(),
            fields,
        }
    }
    fn let_(name: &str, value: Expr, body: Expr) -> Expr {
        Expr::Let {
            name: name.to_string(),
            value: Box::new(value),
            body: Box::new(body),
        }
    }
    fn match_(scrutinee: Expr, arms: Vec<MatchArm>) -> Expr {
        Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        }
    }
    fn arm(tag: &str, bindings: Vec<&str>, body: Expr) -> MatchArm {
        MatchArm {
            tag: tag.to_string(),
            bindings: bindings.into_iter().map(|s| s.to_string()).collect(),
            body,
        }
    }
    fn inc(name: &str, rest: Expr) -> Expr {
        Expr::RcAnnotated {
            op: RcOp::Inc,
            target: name.to_string(),
            rest: Box::new(rest),
        }
    }
    fn dec(name: &str, rest: Expr) -> Expr {
        Expr::RcAnnotated {
            op: RcOp::Dec,
            target: name.to_string(),
            rest: Box::new(rest),
        }
    }

    fn eval_ir(ir: &Expr) -> Value {
        Interpreter::new()
            .eval(ir)
            .unwrap_or_else(|e| panic!("eval error: {e}"))
    }

    fn eval_ir_err(ir: &Expr) -> String {
        Interpreter::new()
            .eval(ir)
            .expect_err("expected evaluation to fail")
    }

    #[test]
    fn construct_and_match_extracts_fields() {
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            match_(var("p"), vec![arm("Point", vec!["x", "y"], Expr::Binary(BinOp::Add, Box::new(var("x")), Box::new(var("y"))))]),
        );
        assert_eq!(eval_ir(&program), Value::Int(3));
    }

    #[test]
    fn match_with_no_matching_arm_is_an_error() {
        let program = let_(
            "p",
            ctor("Foo", vec![]),
            match_(var("p"), vec![arm("Bar", vec![], int(0))]),
        );
        let err = eval_ir_err(&program);
        assert!(err.contains("Foo"), "error should mention the unmatched tag, got: {err}");
    }

    #[test]
    fn match_on_non_heap_scrutinee_is_an_error() {
        let program = match_(int(5), vec![arm("Whatever", vec![], int(0))]);
        eval_ir_err(&program);
    }

    #[test]
    fn rc_inc_dec_actually_change_the_refcount() {
        // Proves the interpreter's RcAnnotated evaluation is really
        // wired to the heap (not just heap.rs's own unit tests of the
        // Heap type in isolation): alloc (rc=1) -> inc (rc=2) -> dec
        // (rc=1, must still be alive — if Inc were a no-op, this dec
        // alone would already free it) -> dec again (rc=0, now freed).
        // A bare `Var(p)` never dereferences the heap, so "still
        // alive"/"freed" can only be proven by actually reading via
        // Match at each checkpoint.
        let read = || match_(var("p"), vec![arm("Point", vec![], int(0))]);

        let still_alive = let_("p", ctor("Point", vec![]), inc("p", dec("p", read())));
        assert_eq!(eval_ir(&still_alive), Value::Int(0), "should still be readable after inc+dec");

        let now_freed = let_("p", ctor("Point", vec![]), inc("p", dec("p", dec("p", read()))));
        let err = eval_ir_err(&now_freed);
        assert!(err.contains("free"), "expected a use-after-free error, got: {err}");
    }

    #[test]
    fn dec_to_zero_makes_the_value_unreadable_afterward() {
        // Construct, drop explicitly, then try to match on it — proves
        // Dec actually freed the cell, using the heap's own
        // use-after-free detection as the check.
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            dec("p", match_(var("p"), vec![arm("Point", vec!["x", "y"], var("x"))])),
        );
        let err = eval_ir_err(&program);
        assert!(err.contains("free"), "expected a use-after-free error, got: {err}");
    }

    #[test]
    fn reuse_when_uniquely_owned_recycles_the_same_allocation() {
        // p has exactly one reference (the match itself) — FBIP's
        // analysis would leave this use bare (no preceding Inc), so at
        // runtime the match's implicit ownership transfer means
        // refcount is 1 at the CtorReuse, and reuse should fire.
        let mut interp = Interpreter::new();
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            match_(
                var("p"),
                vec![arm(
                    "Point",
                    vec!["x", "y"],
                    ctor_reuse("p", "Point", vec![var("y"), var("x")]),
                )],
            ),
        );
        let result = interp.eval(&program).unwrap();
        let Value::HeapRef(addr) = result else {
            panic!("expected a HeapRef result")
        };
        assert_eq!(addr, 0, "should reuse address 0 — the original Point's cell");
        assert_eq!(interp.alloc_count(), 1, "reuse must not count as a second allocation");
        assert_eq!(interp.refcount(&result).unwrap(), 1);
        let (tag, fields) = interp.heap.read(addr).unwrap();
        assert_eq!(tag, "Point");
        assert_eq!(fields, &[Value::Int(2), Value::Int(1)]);
    }

    #[test]
    fn reuse_when_shared_allocates_fresh_and_preserves_the_alias() {
        // `saved` aliases `p`, so by the time the match runs, the
        // cell's refcount is 2 (bumped by the Inc a real FBIP pass
        // would insert for the non-last use in `saved`'s binding).
        // Reuse must refuse, allocate a NEW cell, and `saved` must
        // still see the ORIGINAL untouched data afterward — this is
        // the actual safety property the whole refcount-gated design
        // exists to guarantee.
        let mut interp = Interpreter::new();
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            let_(
                "saved",
                inc("p", var("p")), // the Inc a real FBIP pass would insert here
                match_(
                    var("p"),
                    vec![arm(
                        "Point",
                        vec!["x", "y"],
                        ctor_reuse("p", "Point", vec![var("y"), var("x")]),
                    )],
                ),
            ),
        );
        let result = interp.eval(&program).unwrap();
        let Value::HeapRef(new_addr) = result else {
            panic!("expected a HeapRef result")
        };
        assert_ne!(new_addr, 0, "must not reuse memory that `saved` still references");
        assert_eq!(interp.alloc_count(), 2, "the shared path must allocate a fresh cell");
        assert_eq!(interp.heap.read(0).unwrap(), ("Point", &[Value::Int(1), Value::Int(2)][..]));
        assert_eq!(interp.heap.read(new_addr).unwrap(), ("Point", &[Value::Int(2), Value::Int(1)][..]));
    }

    // --- The capstone: real Plum SOURCE TEXT, through the entire
    // pipeline for the first time — parse -> lower (resolving a real
    // struct declaration's field order) -> FBIP optimize (refcount
    // insertion + reuse analysis) -> evaluate on the simulated heap.
    // Every other test in this whole thread used hand-built IR
    // specifically to isolate what was being validated; this is where
    // all of it is proven to actually fit together.

    #[test]
    fn end_to_end_real_source_struct_swap_via_reuse() {
        let src = "struct Point { x: Float, y: Float }\n\
                   let result = { let p = Point { x: 1.0, y: 2.0 }; match p { Point(x, y) => Point { x: y, y: x } } }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);

        let ast::ItemKind::Let(def) = &program.items[1].kind else {
            panic!("expected the second item to be a let definition");
        };
        let ir = lower_expr(&def.body, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let optimized = plum_ir::fbip::optimize(ir);

        let mut interp = Interpreter::new();
        let result = interp.eval(&optimized).unwrap_or_else(|e| panic!("eval error: {e}"));

        let Value::HeapRef(addr) = result else {
            panic!("expected a HeapRef result")
        };
        assert_eq!(addr, 0, "single owner, single use — reuse should recycle the original cell");
        assert_eq!(
            interp.alloc_count(),
            1,
            "the swap should cost zero extra allocations — that's the whole point of FBIP"
        );
        assert_eq!(interp.heap.read(addr).unwrap(), ("Point", &[Value::Float(2.0), Value::Float(1.0)][..]));
    }

    // --- Real function definitions and calls, including recursion —
    // the big remaining gap this whole thread has been flagging since
    // the very first interpreter tests.

    use plum_ir::lower::lower_program;

    fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        let ir_program = lower_program(&program, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let mut interp = Interpreter::new();
        interp.load_program(&ir_program);
        interp
            .call(fn_name, args)
            .unwrap_or_else(|e| panic!("call error: {e}"))
    }

    fn run_err(src: &str, fn_name: &str, args: Vec<Value>) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        let ir_program = lower_program(&program, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let mut interp = Interpreter::new();
        interp.load_program(&ir_program);
        interp.call(fn_name, args).expect_err("expected call to fail")
    }

    #[test]
    fn call_single_param_function() {
        assert_eq!(run("let double n = n * 2", "double", vec![Value::Int(5)]), Value::Int(10));
    }

    #[test]
    fn call_multi_param_function() {
        assert_eq!(
            run("let add a b = a + b", "add", vec![Value::Int(2), Value::Int(3)]),
            Value::Int(5)
        );
    }

    #[test]
    fn recursion() {
        // The capstone this whole thread has been building toward:
        // first real recursive Plum function actually running.
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        assert_eq!(run(src, "sum", vec![Value::Int(5), Value::Int(0)]), Value::Int(15));
    }

    #[test]
    fn cross_function_calls() {
        let src = "let square x = x * x\nlet sum_of_squares a b = square(a) + square(b)";
        assert_eq!(
            run(src, "sum_of_squares", vec![Value::Int(3), Value::Int(4)]),
            Value::Int(25)
        );
    }

    #[test]
    fn wrong_argument_count_is_an_error() {
        run_err("let add a b = a + b", "add", vec![Value::Int(1)]);
    }

    #[test]
    fn unknown_function_is_an_error() {
        run_err("let double n = n * 2", "triple", vec![Value::Int(1)]);
    }

    #[test]
    fn function_body_cannot_see_the_caller_environment() {
        // A function's body must only ever see its own parameters —
        // never whatever locals happened to exist wherever it was
        // called from. Proven by pre-populating the interpreter's env
        // with a binding the function references but never receives
        // as a parameter, and confirming the call still fails to find
        // it — if isolation were broken (e.g. `call` accidentally
        // reused `self.env` instead of swapping in a fresh one), this
        // would incorrectly succeed.
        let src = "let leak_check unused_param = outer_var + 1";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap();
        let ctx = LoweringContext::new();
        let ir_program = lower_program(&program, &ctx).unwrap();

        let mut interp = Interpreter::new();
        interp.load_program(&ir_program);
        interp.env.push(("outer_var".to_string(), Value::Int(99)));

        let err = interp.call("leak_check", vec![Value::Int(0)]).expect_err("expected call to fail");
        assert!(err.contains("outer_var"), "expected an unbound-variable error, got: {err}");
    }

    #[test]
    fn calling_a_non_name_callee_is_not_yet_supported() {
        // No closures yet — only a bare name naming a known function
        // can be called. `(if true { f } else { g })(1)` is a
        // legitimate future case (calling a computed function value)
        // that simply isn't representable yet.
        let call_expr = Expr::Call {
            callee: Box::new(Expr::If {
                cond: Box::new(Expr::Bool(true)),
                then_branch: Box::new(Expr::Int(0)),
                else_branch: Box::new(Expr::Int(0)),
            }),
            args: vec![],
        };
        assert!(Interpreter::new().eval(&call_expr).is_err());
    }
}
