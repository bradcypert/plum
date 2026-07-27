use plum_interp::Interpreter;
use plum_ir::lower::lower_expr;
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;

fn main() {
    let source = "0";
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize();
    let mut parser = Parser::new(tokens);

    match parser.parse_expr() {
        Ok(ast) => match lower_expr(&ast) {
            Ok(ir) => {
                let mut interp = Interpreter::new();
                match interp.eval(&ir) {
                    Ok(value) => println!("{value:?}"),
                    Err(e) => eprintln!("eval error: {e}"),
                }
            }
            Err(e) => eprintln!("lowering error: {e}"),
        },
        Err(e) => eprintln!("plumc: {e} (parser is a stub — this is expected for now)"),
    }
}
