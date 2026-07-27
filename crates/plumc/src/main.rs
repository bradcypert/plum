use plum_interp::{Interpreter, Value};
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;

fn main() {
    // First real .plum-shaped program run through the whole pipeline:
    // lex -> parse_program -> lower_program -> load into the
    // interpreter -> call a real, recursive, user-defined function.
    let source = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";

    let tokens = Lexer::new(source).tokenize();
    let mut parser = Parser::new(tokens);
    let program = match parser.parse_program() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse error: {e}");
            return;
        }
    };

    let ctx = LoweringContext::from_items(&program.items);
    let ir_program = match lower_program(&program, &ctx) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lowering error: {e}");
            return;
        }
    };

    let mut interp = Interpreter::new();
    interp.load_program(&ir_program);
    match interp.call("sum", vec![Value::Int(5), Value::Int(0)]) {
        Ok(value) => println!("sum(5, 0) = {value:?}"),
        Err(e) => eprintln!("eval error: {e}"),
    }
}
