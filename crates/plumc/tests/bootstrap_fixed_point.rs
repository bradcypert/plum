//! THE BOOTSTRAP FIXED POINT, as an automated test.
//!
//! This is the strongest invariant this project has, and until now
//! nothing guarded it — a miscompilation in the self-hosted backend
//! would only surface the next time someone ran `bootstrap/
//! bootstrap-check` by hand. It has already caught things the whole
//! `bootstrap/exec_corpus` missed twice: during the refcount layout
//! change two offset edits silently failed to apply and 16 of 17
//! fixtures still passed, and every one of the three ownership bugs in
//! the reference-counting work showed up here first.
//!
//! The chain:
//!
//!   stage 1  the self-hosted compiler, built by THIS (Rust) compiler
//!   stage 2  the self-hosted compiler, built by stage 1
//!   stage 3  the self-hosted compiler, built by stage 2
//!
//! Stages 1 and 2 are different binaries produced by different
//! compilers, so their agreeing proves little. Stages 2 and 3 are built
//! by compilers *themselves written in Plum*, so any construct the
//! self-hosted backend miscompiles — one it uses in its own source —
//! makes stage 3 diverge. They must be byte-identical.
//!
//! Deliberately does NOT go through `./sh`: that wrapper exists to cap
//! an interactive runaway's memory, and a test should fail by asserting
//! rather than by being killed.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
}

fn plum_bin() -> PathBuf {
    // The integration test's own executable lives next to the binaries
    // cargo built for this profile.
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("plum")
}

fn run(cmd: &mut Command, what: &str) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("could not run {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_self_hosted_compiler_compiles_itself_to_a_fixed_point() {
    let root = repo_root();
    let plum = plum_bin();
    if !plum.exists() {
        panic!("expected the `plum` binary at {plum:?} — run via `cargo test`, which builds it");
    }
    let work = std::env::temp_dir().join(format!("plum-bootstrap-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let self_host = root.join("bootstrap").join("self_host");
    let stage1 = work.join("stage1");
    run(
        Command::new(&plum).current_dir(&root).arg("build").arg(&self_host).arg("-o").arg(&stage1),
        "stage 1 build",
    );

    // Stage 1 emits the IR for stage 2, and stage 2 emits it for stage 3.
    let ir2 = run(
        Command::new(&stage1).current_dir(&root).arg("emit-llvm").arg("bootstrap/self_host"),
        "stage 1 emit",
    );
    assert!(
        ir2.starts_with("; --- Stage 5 runtime"),
        "stage 1 did not emit IR — it reported: {}",
        ir2.lines().next().unwrap_or("")
    );
    let ir2_path = work.join("stage2.ll");
    std::fs::write(&ir2_path, &ir2).unwrap();

    let stage2 = work.join("stage2");
    run(
        Command::new(&plum).current_dir(&root).arg("compile-ir").arg(&ir2_path).arg("-o").arg(&stage2),
        "stage 2 link",
    );

    let ir3 = run(
        Command::new(&stage2).current_dir(&root).arg("emit-llvm").arg("bootstrap/self_host"),
        "stage 2 emit",
    );

    assert_eq!(
        ir2.len(),
        ir3.len(),
        "stage 3 IR differs in length from stage 2 ({} vs {} bytes) — the self-hosted backend \
         miscompiles something it uses in its own source",
        ir2.len(),
        ir3.len()
    );
    assert!(
        ir2 == ir3,
        "stage 2 and stage 3 IR differ — the self-hosted backend miscompiles something it uses \
         in its own source. First differing line: {}",
        ir2.lines()
            .zip(ir3.lines())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map(|(i, (a, b))| format!("{}: {a:?} vs {b:?}", i + 1))
            .unwrap_or_else(|| "<none, lengths differ>".to_string())
    );

    // A fixed point that produces a broken compiler would still be a
    // fixed point, so stage 2 also has to be a working compiler.
    let ast = run(
        Command::new(&stage2)
            .current_dir(&root)
            .arg("ast")
            .arg("bootstrap/corpus/let_defs/associated_function.plum"),
        "stage 2 ast",
    );
    let golden = std::fs::read_to_string(root.join("bootstrap/corpus/let_defs/associated_function.expected")).unwrap();
    assert_eq!(ast.trim_end(), golden.trim_end(), "stage 2 produced the wrong AST");

    let checked = run(
        Command::new(&stage2).current_dir(&root).arg("check").arg("bootstrap/self_host"),
        "stage 2 check",
    );
    assert_eq!(checked.trim_end(), "ok", "stage 2 failed to type-check the compiler");

    let _ = std::fs::remove_dir_all(&work);
}
