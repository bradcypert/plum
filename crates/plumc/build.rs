fn main() {
    // See plum-interp's own build.rs for why: extern-call resolution
    // works against the CURRENT PROCESS's symbol table, which only
    // contains libm functions (`sqrt`, etc.) if libm is actually linked
    // into this binary. `--no-as-needed` is required alongside `-lm` —
    // see plum-interp's build.rs for the full explanation of why the
    // linker otherwise silently drops it, and why both flags are raw
    // `-C link-arg`s rather than `cargo:rustc-link-lib`, in this exact
    // order.
    #[cfg(unix)]
    println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
    #[cfg(unix)]
    println!("cargo:rustc-link-arg=-lm");
}
