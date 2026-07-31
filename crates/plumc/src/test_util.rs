//! Shared test-only fixtures — currently just `TempProject`, used by
//! both `project.rs`'s own directory-walking tests and `codegen_cli.rs`'s
//! new `plumc build` tests (which need a real on-disk multi-file project
//! to drive `resolve_project`/`compile_program_to_ir` end to end, the
//! same way `project.rs`'s tests already do for the interpreter path).
//! Lives in its own module rather than being duplicated so both test
//! suites stay in lockstep.

use std::fs;

/// A minimal, dependency-free temp-directory helper — creates a UNIQUE
/// directory under `std::env::temp_dir()` (process id + an incrementing
/// counter, so parallel test threads never collide) and removes it on
/// drop, so a panicking assertion still cleans up rather than leaking a
/// directory per failed test run.
pub struct TempProject {
    pub path: std::path::PathBuf,
}

impl TempProject {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("plumc-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("failed to create temp project directory");
        TempProject { path }
    }

    pub fn write(&self, rel_path: &str, contents: &str) {
        let full = self.path.join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("failed to create parent directory");
        }
        fs::write(&full, contents).expect("failed to write test file");
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
