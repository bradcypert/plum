use plum_interp::Interpreter;
use plum_ir::lower::{lower_expr, LoweringContext};
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;

fn main() {
    // Item-level lowering (functions) doesn't exist yet — see
    // plum-ir's scope notes — so this runs one expression, not a real
    // .plum file. First real end-to-end run of the whole pipeline:
    // lex -> parse -> lower -> evaluate.
    let source = "{ let n = 5; if n == 5 { n * 2 } else { 0 } }";
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize();
    let mut parser = Parser::new(tokens);
    let ctx = LoweringContext::new();

    match parser.parse_expr() {
        Ok(ast) => match lower_expr(&ast, &ctx) {
            Ok(ir) => {
                let mut interp = Interpreter::new();
                match interp.eval(&ir) {
                    Ok(value) => println!("{value:?}"),
                    Err(e) => eprintln!("eval error: {e}"),
                }
            }
            Err(e) => eprintln!("lowering error: {e}"),
        },
        Err(e) => eprintln!("parse error: {e}"),
    }
}
