fn main() {
    // See plum-interp's own build.rs for why: extern-call resolution
    // works against the CURRENT PROCESS's symbol table, which only
    // contains libm functions (`sqrt`, etc.) if libm is actually linked
    // into this binary.
    #[cfg(unix)]
    println!("cargo:rustc-link-lib=dylib=m");
}
