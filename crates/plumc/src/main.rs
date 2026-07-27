use plum_interp::Value;
use plumc::typecheck_and_run;

fn main() {
    // First real .plum-shaped program run through the whole pipeline:
    // lex -> parse_program -> type-check (gate) -> lower -> FBIP
    // optimize -> load into the interpreter -> call a real, recursive,
    // user-defined function.
    let source = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";

    match typecheck_and_run(source, "sum", vec![Value::Int(5), Value::Int(0)]) {
        Ok(value) => println!("sum(5, 0) = {value:?}"),
        Err(e) => eprintln!("{e}"),
    }
}
