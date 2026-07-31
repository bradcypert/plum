use plum_interp::Value;
use plumc::{
    compile_ir_to_binary, compile_program_to_ir, emit_main, reject_unprintable_return, resolve_project, typecheck_and_run,
    typecheck_and_run_project, CgValue,
};
use std::path::{Path, PathBuf};

/// `plumc <project-dir>` runs `<project-dir>`'s `main` function (a
/// unit-param entry point — `let main () = { ... }`, the same
/// convention `examples/overview.plum`'s own `example()` already
/// uses) through the full module-aware pipeline. With no arguments,
/// falls back to the original single-expression smoke-test demo, kept
/// as a zero-setup sanity check that the whole lex/parse/type-check/
/// lower/optimize/interpret pipeline still works end to end.
///
/// `plumc build <project-dir> [-o <output>]` compiles+links the SAME
/// project through the LLVM codegen backend instead, producing a real
/// native executable — see `run_build`'s own doc comment.
fn main() {
    let mut cli_args = std::env::args().skip(1);
    match cli_args.next() {
        Some(first) if first == "build" => {
            let rest: Vec<String> = cli_args.collect();
            run_build(&rest);
        }
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

/// Parsed `plumc build` arguments — a single positional project
/// directory plus an optional `-o`/`--output` flag, hand-parsed in the
/// same raw `std::env::args()` style `main` already uses (no new crate
/// dependency needed for one positional + one optional flag). Split
/// into its own directly-testable function since there's no existing
/// precedent in this codebase for testing `main()` itself.
#[derive(Debug, PartialEq, Eq)]
struct BuildArgs {
    project_dir: String,
    output: Option<String>,
}

fn parse_build_args(args: &[String]) -> Result<BuildArgs, String> {
    let mut project_dir = None;
    let mut output = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-o" || arg == "--output" {
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} requires a value"))?;
            output = Some(value.clone());
            i += 2;
        } else if let Some(value) = arg.strip_prefix("-o=").or_else(|| arg.strip_prefix("--output=")) {
            output = Some(value.to_string());
            i += 1;
        } else if project_dir.is_none() {
            project_dir = Some(arg.clone());
            i += 1;
        } else {
            return Err(format!("unexpected argument: {arg:?}"));
        }
    }
    let project_dir = project_dir.ok_or_else(|| "usage: plumc build <project-dir> [-o <output>]".to_string())?;
    Ok(BuildArgs { project_dir, output })
}

/// The default output path when `-o`/`--output` is omitted: the
/// project directory's own basename (matching `go build`/`cargo build
/// --bin`'s own convention), resolved against the CURRENT WORKING
/// DIRECTORY — not written inside the project source tree — falling
/// back to `"a.out"` if the directory path has no filename component
/// (e.g. `plumc build .` or `plumc build /`).
fn default_output_path(project_dir: &str) -> PathBuf {
    let name = Path::new(project_dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "a.out".to_string());
    PathBuf::from(name)
}

/// `plumc build <project-dir> [-o <output>]`: resolves the project's
/// modules into one flat `ast::Program` (`resolve_project` — the exact
/// same pre-pass `plumc <project-dir>`'s interpreter path already uses,
/// just stopped short of running anything), compiles it through the
/// LLVM backend (`compile_program_to_ir`), verifies `main` has the
/// compiled entry-point shape this CLI always produces (exactly one
/// Unit parameter — see `plum_ir::lower`'s `__unit_paramN` convention),
/// rejects an unprintable return type up front, appends the hand-
/// written native entry point (`emit_main`), and persists the linked
/// binary (`compile_ir_to_binary`). Any failure at any stage prints via
/// `eprintln!` and exits 1, mirroring the interpreter path's own
/// error-handling pattern (see `main` above) exactly.
fn run_build(args: &[String]) {
    let parsed = match parse_build_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let out_path = parsed.output.map(PathBuf::from).unwrap_or_else(|| default_output_path(&parsed.project_dir));

    if let Err(e) = build(&parsed.project_dir, &out_path) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    println!("built {:?} -> {:?}", parsed.project_dir, out_path);
}

fn build(project_dir: &str, out_path: &Path) -> Result<(), String> {
    let root = Path::new(project_dir);
    let program = resolve_project(root)?;
    let (body_ir, signatures, resolved_entry) = compile_program_to_ir(&program, "main")?;
    let sig = signatures
        .get(&resolved_entry)
        .ok_or_else(|| "codegen: no such function \"main\" — every buildable project needs a `main` entry point".to_string())?
        .clone();
    // Every compiled entry point this CLI produces takes exactly one
    // synthetic Unit parameter (`let main (): T = ...` lowers to a
    // single `i1` param — see `plum_ir::lower`'s `__unit_paramN`
    // convention) — a `main` with any other arity is a clear error
    // here rather than a confusing argument-count mismatch downstream.
    if sig.params.len() != 1 {
        return Err(format!(
            "codegen: \"main\" must take exactly one Unit parameter (`let main (): T = ...`), found {} parameter(s)",
            sig.params.len()
        ));
    }
    reject_unprintable_return("main", sig.ret.clone())?;
    let main_ir = emit_main(&resolved_entry, sig.ret, &[CgValue::Unit]);
    let full_ir = format!("{body_ir}\n{main_ir}");
    compile_ir_to_binary(&full_ir, out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_project_directory_with_no_flags_parses() {
        let args: Vec<String> = vec!["myproj".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: None });
    }

    #[test]
    fn a_dash_o_flag_with_a_separate_value_is_parsed() {
        let args: Vec<String> = vec!["myproj".to_string(), "-o".to_string(), "myapp".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: Some("myapp".to_string()) });
    }

    #[test]
    fn a_long_output_flag_before_the_positional_arg_is_parsed() {
        let args: Vec<String> = vec!["--output".to_string(), "myapp".to_string(), "myproj".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: Some("myapp".to_string()) });
    }

    #[test]
    fn an_equals_form_flag_is_parsed() {
        let args: Vec<String> = vec!["myproj".to_string(), "--output=myapp".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: Some("myapp".to_string()) });
    }

    #[test]
    fn missing_project_directory_is_a_clear_error() {
        let args: Vec<String> = vec!["-o".to_string(), "myapp".to_string()];
        let err = parse_build_args(&args).expect_err("expected a usage error");
        assert!(err.contains("usage"), "unexpected error: {err}");
    }

    #[test]
    fn a_dash_o_flag_missing_its_value_is_a_clear_error() {
        let args: Vec<String> = vec!["myproj".to_string(), "-o".to_string()];
        let err = parse_build_args(&args).expect_err("expected a missing-value error");
        assert!(err.contains("requires a value"), "unexpected error: {err}");
    }

    #[test]
    fn a_second_unexpected_positional_argument_is_a_clear_error() {
        let args: Vec<String> = vec!["myproj".to_string(), "extra".to_string()];
        let err = parse_build_args(&args).expect_err("expected an unexpected-argument error");
        assert!(err.contains("unexpected argument"), "unexpected error: {err}");
    }

    #[test]
    fn the_default_output_path_is_the_project_directory_s_own_basename() {
        assert_eq!(default_output_path("some/nested/myproj"), PathBuf::from("myproj"));
        assert_eq!(default_output_path("myproj"), PathBuf::from("myproj"));
        assert_eq!(default_output_path("myproj/"), PathBuf::from("myproj"));
    }

    #[test]
    fn a_directory_path_with_no_filename_component_falls_back_to_a_dot_out() {
        assert_eq!(default_output_path("/"), PathBuf::from("a.out"));
        assert_eq!(default_output_path("."), PathBuf::from("a.out"));
    }

    #[test]
    fn build_end_to_end_compiles_and_runs_via_the_persisted_binary() {
        // The one non-arg-parsing integration test living in main.rs
        // rather than codegen_cli.rs's own build tests — proves `build`
        // (the function `run_build`/`main` actually calls) wires
        // `resolve_project` -> `compile_program_to_ir` -> `emit_main`
        // -> `compile_ir_to_binary` together correctly end to end, not
        // just that each piece works in isolation. A minimal inline
        // temp-directory setup (not `plumc`'s own `TempProject` — that's
        // a `#[cfg(test)]`-only fixture private to the LIB crate's own
        // test binary, not reachable from this separate BIN crate).
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("plumc-mainrs-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.plum"), "let main (): Int = 6 * 7").unwrap();
        let out_bin = dir.join("built-from-main-rs");

        let result = build(dir.to_str().unwrap(), &out_bin);
        assert!(result.is_ok(), "build failed: {result:?}");
        let output = std::process::Command::new(&out_bin).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), "42");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
