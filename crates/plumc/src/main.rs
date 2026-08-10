use plum_interp::Value;
use plumc::{
    collect_project_files, compile_ir_to_binary_with_native, compile_program_to_ir_diag, discover_tests, emit_main,
    reject_unprintable_return, resolve_project_diag, run_tests_interpreted, run_tests_native, typecheck_and_run,
    typecheck_and_run_project_with_process_args_diag, CgValue, ModuleSources, TestOutcome,
};
use std::path::{Path, PathBuf};

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
fn main() {
    let mut cli_args = std::env::args().skip(1);
    match cli_args.next() {
        Some(first) if first == "build" => {
            let rest: Vec<String> = cli_args.collect();
            run_build(&rest);
        }
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
    // Extra C source files to compile+link alongside the generated IR
    // — see `run_build`'s own doc comment for why (an FFI-bound C
    // library whose real ABI doesn't fit Plum's `extern "C"` type
    // surface needs a hand-written shim). ADDS to, doesn't replace,
    // whatever `native/*.c` auto-discovery already finds (see
    // `discover_native_c_files`) — the explicit flag and the directory
    // convention are two different ways to reach the SAME mechanism,
    // not competing ones: a project that doesn't want (or can't have)
    // a `native/` directory of its own can still link a shim from
    // anywhere via this flag alone.
    link_c: Vec<String>,
    // Extra libraries to link — each becomes a plain `-l<name>` clang
    // flag (e.g. `--link-lib raylib` → `-lraylib`). No `-L`/`-I`
    // support yet — not needed for any library installed at its
    // system-default paths (confirmed directly: `-lraylib` alone
    // links against this system's package-manager-installed raylib,
    // headers and library both already on clang's default search
    // paths, no `pkg-config` invocation required) — a real, narrower-
    // than-ideal v1 scope, not a silent gap, matching this codebase's
    // own established "honest narrow gap" style elsewhere.
    link_lib: Vec<String>,
}

fn parse_build_args(args: &[String]) -> Result<BuildArgs, String> {
    let mut project_dir = None;
    let mut output = None;
    let mut link_c = Vec::new();
    let mut link_lib = Vec::new();
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
        } else if arg == "--link-c" {
            let value = args.get(i + 1).ok_or_else(|| format!("{arg} requires a value"))?;
            link_c.push(value.clone());
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--link-c=") {
            link_c.push(value.to_string());
            i += 1;
        } else if arg == "--link-lib" {
            let value = args.get(i + 1).ok_or_else(|| format!("{arg} requires a value"))?;
            link_lib.push(value.clone());
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--link-lib=") {
            link_lib.push(value.to_string());
            i += 1;
        } else if project_dir.is_none() {
            project_dir = Some(arg.clone());
            i += 1;
        } else {
            return Err(format!("unexpected argument: {arg:?}"));
        }
    }
    let project_dir = project_dir
        .ok_or_else(|| "usage: plumc build <project-dir> [-o <output>] [--link-c <file>]... [--link-lib <name>]...".to_string())?;
    Ok(BuildArgs { project_dir, output, link_c, link_lib })
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

/// `plumc build <project-dir> [-o <output>] [--link-c <file>]... [--link-lib
/// <name>]...`: resolves the project's modules into one flat `ast::
/// Program` (`resolve_project` — the exact same pre-pass `plumc
/// <project-dir>`'s interpreter path already uses, just stopped short of
/// running anything), compiles it through the LLVM backend (`compile_
/// program_to_ir`), verifies `main` has the compiled entry-point shape
/// this CLI always produces (exactly one Unit parameter — see `plum_ir::
/// lower`'s `__unit_paramN` convention), rejects an unprintable return
/// type up front, appends the hand-written native entry point
/// (`emit_main`), and persists the linked binary (`compile_ir_to_
/// binary_with_native`). Any failure at any stage prints via `eprintln!`
/// and exits 1, mirroring the interpreter path's own error-handling
/// pattern (see `main` above) exactly.
///
/// **Native C linking**: an `extern "C"` function whose real C ABI
/// doesn't fit Plum's own extern type surface (`Int`/`Float`/`Bool`/
/// `CStr`/qualifying-struct only — no raw pointers, no `float`/`u8`-
/// sized fields) — raylib being the concrete motivating case, whose
/// `Vector2`/`Color` use 32-bit `float`/`unsigned char` fields Plum has
/// no representation for — needs a hand-written C shim exposing an
/// ABI-safe wrapper API instead (see `plum_codegen::codegen`'s module
/// doc comment, or just: this is the same reason every OTHER language's
/// FFI story eventually reaches for a shim too). TWO ways to supply
/// one, usable together or alone: drop `.c` files in the project's own
/// `native/` directory (auto-discovered, sorted for a deterministic
/// build), or pass `--link-c <file>` explicitly for a project that
/// doesn't want (or can't have) its own `native/` directory. `--link-
/// lib <name>` links an external library (`-l<name>`) the shim itself
/// calls into (e.g. `--link-lib raylib`).
fn run_build(args: &[String]) {
    let parsed = match parse_build_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let out_path = parsed.output.map(PathBuf::from).unwrap_or_else(|| default_output_path(&parsed.project_dir));
    let root = Path::new(&parsed.project_dir);
    let sources = match module_sources(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let mut link_c: Vec<PathBuf> = match discover_native_c_files(root) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    link_c.extend(parsed.link_c.iter().map(PathBuf::from));

    if let Err(e) = build(root, &out_path, &link_c, &parsed.link_lib) {
        eprintln!("{}", sources.render(&e));
        std::process::exit(1);
    }
    println!("built {:?} -> {:?}", parsed.project_dir, out_path);
}

/// `<project-dir>/native/*.c`, sorted by filename — the auto-discovery
/// half of `run_build`'s own native-linking support (see its doc
/// comment). Sorted for the SAME "reproducible build order" reason
/// `project::collect_plum_files` sorts its own directory walk; `clang`
/// itself doesn't care about source-file order, but a stable build is
/// still worth having. Missing `native/` entirely is NOT an error —
/// the overwhelming common case (most Plum projects have no native
/// code at all) — only a real filesystem error while reading an
/// EXISTING `native/` directory is.
fn discover_native_c_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let native_dir = root.join("native");
    if !native_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&native_dir)
        .map_err(|e| format!("failed to read {native_dir:?}: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to read a directory entry in {native_dir:?}: {e}"))?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("c"))
        .collect();
    files.sort();
    Ok(files)
}

fn build(root: &Path, out_path: &Path, link_c: &[PathBuf], link_lib: &[String]) -> Result<(), plum_syntax::error::CompileError> {
    let program = resolve_project_diag(root)?;
    let (body_ir, signatures, resolved_entry, has_globals) = compile_program_to_ir_diag(&program, "main")?;
    let sig = signatures
        .get(&resolved_entry)
        .ok_or_else(|| {
            plum_syntax::error::CompileError::spanless(
                "codegen: no such function \"main\" — every buildable project needs a `main` entry point",
            )
        })?
        .clone();
    // Every compiled entry point this CLI produces takes exactly one
    // synthetic Unit parameter (`let main (): T = ...` lowers to a
    // single `i1` param — see `plum_ir::lower`'s `__unit_paramN`
    // convention) — a `main` with any other arity is a clear error
    // here rather than a confusing argument-count mismatch downstream.
    if sig.params.len() != 1 {
        return Err(plum_syntax::error::CompileError::spanless(format!(
            "codegen: \"main\" must take exactly one Unit parameter (`let main (): T = ...`), found {} parameter(s)",
            sig.params.len()
        )));
    }
    reject_unprintable_return("main", sig.ret.clone()).map_err(plum_syntax::error::CompileError::spanless)?;
    let main_ir = emit_main(&resolved_entry, sig.ret, &[CgValue::Unit], has_globals);
    let full_ir = format!("{body_ir}\n{main_ir}");
    compile_ir_to_binary_with_native(&full_ir, out_path, link_c, link_lib).map_err(plum_syntax::error::CompileError::spanless)
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
    let outcomes = if parsed.native {
        run_tests_native(&program, &names)
    } else {
        run_tests_interpreted(program, &names)
    };
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
    fn a_bare_project_directory_with_no_flags_parses() {
        let args: Vec<String> = vec!["myproj".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: None, link_c: vec![], link_lib: vec![] });
    }

    #[test]
    fn a_dash_o_flag_with_a_separate_value_is_parsed() {
        let args: Vec<String> = vec!["myproj".to_string(), "-o".to_string(), "myapp".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: Some("myapp".to_string()), link_c: vec![], link_lib: vec![] });
    }

    #[test]
    fn a_long_output_flag_before_the_positional_arg_is_parsed() {
        let args: Vec<String> = vec!["--output".to_string(), "myapp".to_string(), "myproj".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: Some("myapp".to_string()), link_c: vec![], link_lib: vec![] });
    }

    #[test]
    fn an_equals_form_flag_is_parsed() {
        let args: Vec<String> = vec!["myproj".to_string(), "--output=myapp".to_string()];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(parsed, BuildArgs { project_dir: "myproj".to_string(), output: Some("myapp".to_string()), link_c: vec![], link_lib: vec![] });
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
    fn link_c_and_link_lib_flags_are_repeatable_in_both_forms() {
        let args: Vec<String> = vec![
            "myproj".to_string(),
            "--link-c".to_string(),
            "shim.c".to_string(),
            "--link-c=other.c".to_string(),
            "--link-lib".to_string(),
            "raylib".to_string(),
            "--link-lib=m".to_string(),
        ];
        let parsed = parse_build_args(&args).unwrap();
        assert_eq!(
            parsed,
            BuildArgs {
                project_dir: "myproj".to_string(),
                output: None,
                link_c: vec!["shim.c".to_string(), "other.c".to_string()],
                link_lib: vec!["raylib".to_string(), "m".to_string()],
            }
        );
    }

    #[test]
    fn a_link_c_flag_missing_its_value_is_a_clear_error() {
        let args: Vec<String> = vec!["myproj".to_string(), "--link-c".to_string()];
        let err = parse_build_args(&args).expect_err("expected a missing-value error");
        assert!(err.contains("requires a value"), "unexpected error: {err}");
    }

    #[test]
    fn discover_native_c_files_is_empty_when_there_is_no_native_directory() {
        let dir = std::env::temp_dir().join(format!("plumc-native-discover-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(discover_native_c_files(&dir).unwrap(), Vec::<PathBuf>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_native_c_files_finds_only_dot_c_files_sorted() {
        let dir = std::env::temp_dir().join(format!("plumc-native-discover-some-{}", std::process::id()));
        let native = dir.join("native");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join("b.c"), "").unwrap();
        std::fs::write(native.join("a.c"), "").unwrap();
        std::fs::write(native.join("notes.txt"), "").unwrap();
        let found = discover_native_c_files(&dir).unwrap();
        assert_eq!(found, vec![native.join("a.c"), native.join("b.c")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

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

        let result = build(&dir, &out_bin, &[], &[]);
        assert!(result.is_ok(), "build failed: {result:?}");
        let output = std::process::Command::new(&out_bin).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), "42");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_c_linking_works_via_both_the_native_directory_and_the_explicit_flag() {
        // The end-to-end proof for `run_build`'s own native-linking
        // support (see its doc comment) — a real project directory
        // with a `native/` C file (auto-discovered) AND a SEPARATE C
        // file passed via the explicit `--link-c`-equivalent `link_c`
        // parameter, both actually linked into the SAME binary and
        // both actually called from Plum via `extern "C"`. No raylib
        // dependency needed to prove the MECHANISM works — a trivial
        // hand-written C function is enough; raylib itself needs
        // nothing more than what this test already exercises (`plum
        // build`'s own clang invocation doesn't care what the C code
        // actually does).
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("plumc-native-link-test-{}-{n}", std::process::id()));
        let native = dir.join("native");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(
            dir.join("main.plum"),
            "extern \"C\" { \
                fn native_test_double(x: Int) -> Int; \
                fn native_test_triple(x: Int) -> Int; \
             } \
             let main (): Int = unsafe { native_test_double(5) + native_test_triple(5) }",
        )
        .unwrap();
        // Auto-discovered via `native/`.
        std::fs::write(native.join("double.c"), "long long native_test_double(long long x) { return x * 2; }").unwrap();
        // Passed explicitly, living OUTSIDE the project directory
        // entirely — proving a project doesn't need its own `native/`
        // directory to use this at all.
        let explicit_c = dir.join("../plumc-native-link-test-explicit.c");
        std::fs::write(&explicit_c, "long long native_test_triple(long long x) { return x * 3; }").unwrap();
        let out_bin = dir.join("built-with-native-c");

        // `build()` itself takes exactly the link inputs it's given —
        // `discover_native_c_files` is `run_build`'s own job (see its
        // doc comment), so this test replicates that merge step
        // exactly the way `run_build` does, rather than calling
        // `build()` with only the explicit file and silently missing
        // half of what this test means to prove.
        let mut link_c = discover_native_c_files(&dir).unwrap();
        link_c.push(explicit_c.clone());
        let result = build(&dir, &out_bin, &link_c, &[]);
        assert!(result.is_ok(), "build failed: {result:?}");
        let output = std::process::Command::new(&out_bin).output().unwrap();
        assert!(output.status.success());
        // double(5) + triple(5) = 10 + 15 = 25
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), "25");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&explicit_c);
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
    fn the_native_flag_is_parsed_regardless_of_position() {
        let args: Vec<String> = vec!["--native".to_string(), "myproj".to_string()];
        let parsed = parse_test_args(&args).unwrap();
        assert_eq!(parsed, TestArgs { project_dir: "myproj".to_string(), native: true });

        let args: Vec<String> = vec!["myproj".to_string(), "--native".to_string()];
        let parsed = parse_test_args(&args).unwrap();
        assert_eq!(parsed, TestArgs { project_dir: "myproj".to_string(), native: true });
    }

    #[test]
    fn missing_project_directory_is_a_clear_error_for_test_args() {
        let args: Vec<String> = vec!["--native".to_string()];
        let err = parse_test_args(&args).expect_err("expected a usage error");
        assert!(err.contains("usage"), "unexpected error: {err}");
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
