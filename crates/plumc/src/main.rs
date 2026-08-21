use plum_interp::Value;
use plumc::{
    collect_project_files, discover_tests, resolve_project_diag, run_tests_interpreted, typecheck_and_run,
    typecheck_and_run_project_with_process_args_diag, ModuleSources, TestOutcome,
};
use std::path::Path;

// Raw libc FFI, not a new crate dependency (matching this codebase's
// established preference — see the FFI section of DESIGN.md — for
// declaring exactly the one function needed over pulling in a whole
// binding crate, especially somewhere a future self-hosted Plum
// compiler's own driver would need the identical raw call regardless
// of implementation language). `fflush(NULL)` flushes every open C
// stdio stream. Needed because a Plum program's own `println` (see
// `plumc::STDLIB_IO_SRC`) writes through libc's `puts`, which — unlike
// Rust's own `println!` — is fully block-buffered whenever stdout
// isn't a TTY (e.g. running under a test harness or piped output), so
// its writes only reach the OS at actual process exit unless flushed
// explicitly. Without this, the interpreter CLI's own final `println!`
// of the entry function's return value (Rust-buffered, flushes on its
// own newline) could reach the terminal/pipe BEFORE a Plum program's
// own EARLIER `println` calls — confirmed by hand: exactly this
// reordering happened before this fix was added. The native/`build`
// path doesn't need this: `emit_main`'s own hand-written `main()` and
// every `puts()` call it makes both run inside the SAME single
// process, and process exit itself flushes every open C stream in the
// correct, true program order — there's no separate Rust host process
// printing something else afterward.
unsafe extern "C" {
    fn fflush(stream: *mut std::ffi::c_void) -> i32;
}

/// `plum run <project-dir>` (or the bare `plum <project-dir>` shorthand
/// — kept working for backward compatibility, since it predates `run`
/// existing as an explicit subcommand at all) runs `<project-dir>`'s
/// `main` function (a unit-param entry point — `let main () = { ... }`,
/// the same convention `examples/overview.plum`'s own `example()`
/// already uses) through the full module-aware pipeline. With no
/// arguments, falls back to the original single-expression smoke-test
/// demo, kept as a zero-setup sanity check that the whole lex/parse/
/// type-check/lower/optimize/interpret pipeline still works end to end.
///
/// `plum build <project-dir> [-o <output>]` compiles+links the SAME
/// project through the LLVM codegen backend instead, producing a real
/// native executable — see `run_build`'s own doc comment.
///
/// `plum new <name>` scaffolds a minimal starter project — see
/// `run_new`'s own doc comment.
///
/// `plum test [--native] <project-dir>` discovers and runs every
/// `test_*` function — see `run_test_cmd`'s own doc comment.
///
/// `plum dump-ast <file>` parses a single `.plum` FILE (no module
/// resolution, no prelude — just this crate's real `Lexer`+`Parser` on
/// exactly the bytes in the file) and prints its canonical s-expression
/// form (`plum_syntax::render::render_program`) to stdout — see
/// `run_dump_ast`'s own doc comment.
///
/// `plum dump-tokens <file>` is `dump-ast`'s lexer-only counterpart —
/// see `run_dump_tokens`'s own doc comment.
fn main() {
    let mut cli_args = std::env::args().skip(1);
    match cli_args.next() {
        Some(first) if first == "run" => {
            let rest: Vec<String> = cli_args.collect();
            run_interpreter(&rest);
        }
        Some(first) if first == "new" => {
            let rest: Vec<String> = cli_args.collect();
            run_new(&rest);
        }
        Some(first) if first == "test" => {
            let rest: Vec<String> = cli_args.collect();
            run_test_cmd(&rest);
        }
        Some(first) if first == "dump-ast" => {
            let rest: Vec<String> = cli_args.collect();
            run_dump_ast(&rest);
        }
        Some(first) if first == "dump-tokens" => {
            let rest: Vec<String> = cli_args.collect();
            run_dump_tokens(&rest);
        }
        Some(first) if first == "lsp" => {
            plumc::lsp::run();
        }
        Some(project_dir) => run_interpreter(&[project_dir]),
        None => {
            let source = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
            match typecheck_and_run(source, "sum", vec![Value::Int(5), Value::Int(0)]) {
                Ok(value) => {
                    unsafe { fflush(std::ptr::null_mut()) };
                    println!("sum(5, 0) = {value:?}");
                }
                Err(e) => eprintln!("{e}"),
            }
        }
    }
}

/// The interpreter path itself, factored out of `main` so both `plum
/// run <project-dir>` and the bare `plum <project-dir>` shorthand
/// funnel through the exact same code.
fn run_interpreter(args: &[String]) {
    let RunArgs { project_dir, process_args } = match parse_run_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let root = Path::new(&project_dir);
    let sources = match module_sources(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    match typecheck_and_run_project_with_process_args_diag(root, "main", vec![Value::Unit], process_args) {
        Ok(value) => {
            unsafe { fflush(std::ptr::null_mut()) };
            println!("{value:?}");
        }
        Err(e) => {
            eprintln!("{}", sources.render(&e));
            std::process::exit(1);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    project_dir: String,
    process_args: Vec<String>,
}

/// `plum run <project-dir> [-- <arg>...]` — the cargo-style `--`
/// separator (`cargo run -- foo bar`, `npm run x -- foo bar`):
/// everything before a literal `--` token is this CLI's own argument
/// (just `<project-dir>`, today), everything AFTER it is passed
/// through VERBATIM as the Plum program's own `args()` — no further
/// flag parsing on that side at all, matching cargo/npm's own
/// semantics exactly (so e.g. a Plum program wanting its OWN `-o` flag
/// doesn't collide with this CLI's unrelated flags). The bare `plum
/// <project-dir>` shorthand (no `run`) reuses this same function (see
/// `main`), so it gets `--`-args for free too.
fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let separator = args.iter().position(|a| a == "--");
    let (own_args, process_args) = match separator {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let project_dir = own_args
        .first()
        .ok_or_else(|| "usage: plum run <project-dir> [-- <arg>...]".to_string())?
        .clone();
    if own_args.len() > 1 {
        return Err(format!("unexpected argument: {:?}", own_args[1]));
    }
    Ok(RunArgs { project_dir, process_args })
}

/// Builds a `ModuleSources` from `root`'s own `.plum` files — shared by
/// both CLI paths (the interpreter path in `main`, and `run_build`
/// below) so a `CompileError` returned from EITHER pipeline can be
/// rendered as a real `file:line:col` + source snippet (`ModuleSources
/// ::render`) rather than a bare message. Walks the project directory a
/// SECOND time, independently of whatever `typecheck_and_run_project_
/// diag`/`resolve_project_diag` do internally — a deliberate, accepted
/// inefficiency for a CLI invoked once per run, not a hot path.
fn module_sources(root: &Path) -> Result<ModuleSources, String> {
    let files = collect_project_files(root)?;
    let modules: Vec<(&str, &str)> = files.iter().map(|(path, src)| (path.as_str(), src.as_str())).collect();
    Ok(ModuleSources::new(&modules))
}











/// `plum new <name>`: scaffolds a minimal starter project — a new
/// directory `<name>/` (resolved against the current working
/// directory, matching `plum build`'s own `-o` default convention)
/// containing one `main.plum` with a hello-world `main`, so `plum run
/// <name>` (or `plum build <name>`) works immediately with zero setup.
/// Refuses to overwrite an existing path rather than silently
/// clobbering whatever's already there — the same "don't destroy
/// unexpected state" caution this whole toolchain otherwise applies to
/// real user files.
fn run_new(args: &[String]) {
    let Some(name) = args.first() else {
        eprintln!("usage: plum new <name>");
        std::process::exit(1);
    };
    if let Err(e) = new_project(Path::new(name)) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    println!("created {name:?}");
}

fn new_project(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        return Err(format!("{dir:?} already exists — refusing to overwrite it"));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create {dir:?}: {e}"))?;
    let main_path = dir.join("main.plum");
    std::fs::write(&main_path, "let main (): Unit = println(\"hello, plum\")\n")
        .map_err(|e| format!("failed to write {main_path:?}: {e}"))?;
    Ok(())
}

/// `plum dump-ast <file>` — parses exactly one `.plum` FILE (no module
/// resolution, no prelude injection, no `assoc_fns`/`nested_struct_
/// update` AST rewriting — just the real `Lexer`+`Parser` on precisely
/// what's in the file) and prints its canonical s-expression form to
/// stdout. Two real, deliberate uses: (1) ad-hoc debugging — "what did
/// the parser actually build for this snippet"; (2) `bootstrap/corpus/`
/// (DESIGN.md's "Self-hosting bootstrap corpus" section) — every
/// `.expected` golden file in that corpus was generated by piping a
/// `.plum` fixture through exactly this command, and the SAME command
/// (this Rust one today, a from-scratch Plum one once Stage 1 exists)
/// is what re-validates the corpus later. A parse error exits 1 with
/// the same `CompileError` message every other command already
/// produces, just without `ModuleSources`' file/line/col rendering
/// (there's no multi-file project here to attribute a span to).
fn run_dump_ast(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("usage: plum dump-ast <file>");
        std::process::exit(1);
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read {path:?}: {e}");
            std::process::exit(1);
        }
    };
    let tokens = plum_syntax::lexer::Lexer::new(&src).tokenize();
    let mut parser = plum_syntax::parser::Parser::new(tokens);
    match parser.parse_program() {
        Ok(program) => println!("{}", plum_syntax::render::render_program(&program)),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// `plum dump-tokens <file>` — `dump-ast`'s lexer-only counterpart:
/// tokenizes a single `.plum` FILE (this crate's real `Lexer`, nothing
/// else) and prints the canonical, space-separated, span-free token
/// list (`plum_syntax::render::render_tokens`) to stdout. Exists so
/// `bootstrap/corpus/`'s self-hosted Stage 1 lexer gets its OWN real
/// pass/fail signal, independent of the parser — without this, a
/// lexer bug could only ever be discovered indirectly, once the parser
/// was also far enough along to expose it, and easily misattributed to
/// the wrong stage. Never fails on malformed input the way `dump-ast`
/// can: this crate's `Lexer` has no error type at all (an unrecognized
/// character just becomes whatever token kind its own fallback
/// produces), so there's no error path to handle here.
fn run_dump_tokens(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("usage: plum dump-tokens <file>");
        std::process::exit(1);
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read {path:?}: {e}");
            std::process::exit(1);
        }
    };
    let tokens = plum_syntax::lexer::Lexer::new(&src).tokenize();
    println!("{}", plum_syntax::render::render_tokens(&tokens));
}

/// Parsed `plum test` arguments — a single positional project
/// directory plus an optional `--native` flag, same hand-parsed style
/// as `parse_build_args`.
#[derive(Debug, PartialEq, Eq)]
struct TestArgs {
    project_dir: String,
    native: bool,
}

fn parse_test_args(args: &[String]) -> Result<TestArgs, String> {
    let mut project_dir = None;
    let mut native = false;
    for arg in args {
        if arg == "--native" {
            native = true;
        } else if project_dir.is_none() {
            project_dir = Some(arg.clone());
        } else {
            return Err(format!("unexpected argument: {arg:?}"));
        }
    }
    let project_dir = project_dir.ok_or_else(|| "usage: plum test [--native] <project-dir>".to_string())?;
    Ok(TestArgs { project_dir, native })
}

/// `plum test [--native] <project-dir>`: resolves the project (same
/// `resolve_project_diag` `plum build` uses — a real syntax/type error
/// in a test file gets the SAME `file:line:col` + snippet treatment
/// every other command already has), discovers every top-level
/// `test_*` function (`plumc::discover_tests`), then runs them either
/// through one loaded interpreter (default — `run_tests_interpreted`,
/// cheap, single process) or as isolated native subprocesses (`--
/// native` — `run_tests_native`, needed because a native runtime
/// failure aborts the whole process with no way to catch and continue
/// — see that function's own doc comment). Exit code 0 only if every
/// test passed, matching `cargo test`'s own convention.
fn run_test_cmd(args: &[String]) {
    let parsed = match parse_test_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let root = Path::new(&parsed.project_dir);
    let sources = match module_sources(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let program = match resolve_project_diag(root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", sources.render(&e));
            std::process::exit(1);
        }
    };
    let names = discover_tests(&program);
    let outcomes = run_tests_interpreted(program, &names);
    let outcomes = match outcomes {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{}", sources.render(&e));
            std::process::exit(1);
        }
    };
    let failed = print_test_report(&outcomes);
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Prints a `cargo test`-style report and returns the failure count.
fn print_test_report(outcomes: &[TestOutcome]) -> usize {
    println!("running {} test{}", outcomes.len(), if outcomes.len() == 1 { "" } else { "s" });
    for outcome in outcomes {
        match &outcome.result {
            Ok(()) => println!("test {} ... ok", outcome.name),
            Err(_) => println!("test {} ... FAILED", outcome.name),
        }
    }
    let failures: Vec<&TestOutcome> = outcomes.iter().filter(|o| o.result.is_err()).collect();
    if !failures.is_empty() {
        println!("\nfailures:");
        for f in &failures {
            println!("\n---- {} ----\n{}", f.name, f.result.as_ref().unwrap_err());
        }
    }
    let passed = outcomes.len() - failures.len();
    let verdict = if failures.is_empty() { "ok" } else { "FAILED" };
    println!("\ntest result: {verdict}. {passed} passed; {} failed", failures.len());
    failures.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumc::typecheck_and_run_project;












    #[test]
    fn a_bare_project_directory_with_no_separator_has_no_process_args() {
        let args: Vec<String> = vec!["myproj".to_string()];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed, RunArgs { project_dir: "myproj".to_string(), process_args: vec![] });
    }

    #[test]
    fn everything_after_the_separator_becomes_process_args_verbatim() {
        let args: Vec<String> = vec!["myproj".to_string(), "--".to_string(), "foo".to_string(), "-o".to_string(), "bar".to_string()];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(
            parsed,
            RunArgs { project_dir: "myproj".to_string(), process_args: vec!["foo".to_string(), "-o".to_string(), "bar".to_string()] }
        );
    }

    #[test]
    fn a_trailing_separator_with_nothing_after_it_is_an_empty_process_args() {
        let args: Vec<String> = vec!["myproj".to_string(), "--".to_string()];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed, RunArgs { project_dir: "myproj".to_string(), process_args: vec![] });
    }

    #[test]
    fn missing_project_directory_is_a_clear_error_for_run_args() {
        let args: Vec<String> = vec![];
        let err = parse_run_args(&args).expect_err("expected a usage error");
        assert!(err.contains("usage"), "unexpected error: {err}");
    }

    #[test]
    fn a_second_positional_before_the_separator_is_a_clear_error() {
        let args: Vec<String> = vec!["myproj".to_string(), "extra".to_string()];
        let err = parse_run_args(&args).expect_err("expected an unexpected-argument error");
        assert!(err.contains("unexpected argument"), "unexpected error: {err}");
    }





    #[test]
    fn new_project_scaffolds_a_runnable_hello_world() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("plumc-new-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        new_project(&dir).unwrap();
        assert!(dir.join("main.plum").exists());

        // The scaffolded project actually runs, not just "some file got
        // written" — proves `new_project`'s hello-world source is real,
        // valid Plum, not just plausible-looking text.
        let result = typecheck_and_run_project(&dir, "main", vec![Value::Unit]);
        assert_eq!(result, Ok(Value::Unit));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_project_directory_parses_for_test_args() {
        let args: Vec<String> = vec!["myproj".to_string()];
        let parsed = parse_test_args(&args).unwrap();
        assert_eq!(parsed, TestArgs { project_dir: "myproj".to_string(), native: false });
    }



    #[test]
    fn print_test_report_returns_the_failure_count() {
        let outcomes = vec![
            TestOutcome { name: "test_a".to_string(), result: Ok(()) },
            TestOutcome { name: "test_b".to_string(), result: Err("assertion failed".to_string()) },
        ];
        assert_eq!(print_test_report(&outcomes), 1);
    }

    #[test]
    fn run_test_cmd_end_to_end_through_the_interpreter() {
        // Mirrors `build_end_to_end_compiles_and_runs_via_the_persisted_
        // binary`'s own real-project-on-disk shape — proves `plum test`
        // wires `resolve_project_diag` -> `discover_tests` -> `run_
        // tests_interpreted` -> the report together correctly, not just
        // that each piece works in isolation.
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("plumc-test-cmd-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("main.plum"),
            "let test_pass (): Unit = assert(true)\nlet test_fail (): Unit = assert(false)\n",
        )
        .unwrap();

        let root = &dir;
        let program = resolve_project_diag(root).unwrap();
        let names = discover_tests(&program);
        let outcomes = run_tests_interpreted(program, &names).unwrap();
        let failed = print_test_report(&outcomes);
        assert_eq!(failed, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_project_refuses_to_overwrite_an_existing_path() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("plumc-new-exists-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let err = new_project(&dir).expect_err("expected an already-exists error");
        assert!(err.contains("already exists"), "unexpected error: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
