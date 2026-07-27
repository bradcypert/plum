use plum_ir::ir::Expr;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Unit,
}

pub struct Interpreter;

impl Interpreter {
    pub fn new() -> Self {
        Interpreter
    }

    pub fn eval(&mut self, expr: &Expr) -> Result<Value, String> {
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Var(name) => Err(format!("unbound variable: {name}")),
            Expr::Let { value, body, .. } => {
                let _ = self.eval(value)?;
                self.eval(body)
            }
            Expr::RcAnnotated { rest, .. } => self.eval(rest),
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
