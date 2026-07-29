fn main() {
    // Extern-call tests (and any Plum program declaring `extern "C" { fn
    // sqrt(...) ... }`) resolve symbols against the CURRENT PROCESS's
    // own dynamic symbol table (see `resolve_extern_fn` in lib.rs) —
    // that only works for libm functions if libm is actually linked
    // into the process, which nothing else in this crate does on its
    // own (unlike libc, which Rust always links). Unix-only, matching
    // the rest of v1's Unix-only symbol-resolution scope.
    #[cfg(unix)]
    println!("cargo:rustc-link-lib=dylib=m");
}
