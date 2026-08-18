//! THE COMPILER'S OUTPUT MUST BE REPRODUCIBLE.
//!
//! Compiling the same source twice must produce byte-identical IR.
//! Nothing guarded this, and it was NOT holding: `monomorphize::plan`
//! seeded its worklist by iterating `HashMap`s, and `codegen::
//! merge_envs` allocated SSA registers while iterating one. Both
//! iterate in a per-PROCESS random order, so the compiler emitted
//! differently-numbered `closure$f$N` symbols and permuted register
//! numbers on every run — 261,046 differing lines on the self-hosted
//! compiler's own source.
//!
//! The generated programs were still CORRECT (two differently-numbered
//! builds of the self-hosted compiler emitted byte-identical output),
//! so this never showed up as a miscompilation. What it cost was the
//! ability to diff IR to prove a change altered nothing — a check
//! worth having, which was silently unavailable — plus reproducible
//! builds, caching, and bisectable codegen regressions.
//!
//! MUST use two separate PROCESSES. Rust's `HashMap` picks its random
//! seed once per process, so two compiles inside one process iterate
//! identically and would pass no matter how order-dependent the
//! compiler is. Running the real binary twice is the only form of this
//! test that can actually fail.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
}

fn plum_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("plum")
}

fn emit(project: &Path) -> String {
    let out = Command::new(plum_bin())
        .current_dir(repo_root())
        .arg("emit-llvm")
        .arg(project)
        .output()
        .unwrap_or_else(|e| panic!("could not run plum emit-llvm: {e}"));
    assert!(
        out.status.success(),
        "emit-llvm failed for {project:?}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Exercises the two constructs whose ordering was actually unstable:
/// closures (numbered from one global counter, in function-emission
/// order) and branch merges (`merge_envs` allocating a register per
/// divergent binding while iterating an `Env`).
///
/// THREE runs, not two: the failure is a random permutation, so two
/// runs can coincide by luck and report success on a genuinely
/// nondeterministic compiler. Verified to have teeth — with either fix
/// reverted, the fixture below emits six different hashes in six runs.
#[test]
fn emitting_the_same_project_twice_produces_identical_ir() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("determinism");
    let runs: Vec<String> = (0..3).map(|_| emit(&project)).collect();
    for (i, other) in runs.iter().enumerate().skip(1) {
        if runs[0] == *other {
            continue;
        }
        let at = runs[0]
            .lines()
            .zip(other.lines())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map(|(n, (a, b))| format!("first difference at line {}:\n  run 1: {a}\n  run {}: {b}", n + 1, i + 1))
            .unwrap_or_else(|| "differs only in length".to_string());
        panic!(
            "the compiler is nondeterministic: two emit-llvm runs over the same source \
             produced different IR ({} vs {} bytes).\n{at}",
            runs[0].len(),
            other.len()
        );
    }
}
