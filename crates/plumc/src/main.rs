use plum_interp::Value;
use plumc::{typecheck_and_run, typecheck_and_run_project};
use std::path::Path;

/// `plumc <project-dir>` runs `<project-dir>`'s `main` function (a
/// unit-param entry point — `let main () = { ... }`, the same
/// convention `examples/overview.plum`'s own `example()` already
/// uses) through the full module-aware pipeline. With no arguments,
/// falls back to the original single-expression smoke-test demo, kept
/// as a zero-setup sanity check that the whole lex/parse/type-check/
/// lower/optimize/interpret pipeline still works end to end.
fn main() {
    let mut cli_args = std::env::args().skip(1);
    match cli_args.next() {
        Some(project_dir) => {
            let root = Path::new(&project_dir);
            match typecheck_and_run_project(root, "main", vec![Value::Unit]) {
                Ok(value) => println!("{value:?}"),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            let source = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
            match typecheck_and_run(source, "sum", vec![Value::Int(5), Value::Int(0)]) {
                Ok(value) => println!("sum(5, 0) = {value:?}"),
                Err(e) => eprintln!("{e}"),
            }
        }
    }
}
