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

    // The TCP socket shim — see `plum-interp/build.rs`'s own, more
    // detailed doc comment for the full "why" (needed so `plum run`'s
    // extern-call resolution against the CURRENT PROCESS's symbol table
    // finds `tcp_connect`/etc.). Duplicated here rather than inherited
    // from `plum-interp`'s own build script deliberately, mirroring the
    // existing `-lm`/`--no-as-needed` precedent immediately above: a
    // dependency crate's `build.rs` link-args don't propagate to a
    // SEPARATE downstream binary target (`plumc`'s own `plum` bin) —
    // that's the exact reason this file already re-emits `-lm` itself
    // rather than trusting `plum-interp`'s copy to cover it.
    #[cfg(unix)]
    {
        let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let shim_src = manifest_dir.join("../../native_stdlib/net_shim.c");
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let obj_path = out_dir.join("net_shim.o");
        let status = std::process::Command::new("cc")
            .arg("-c")
            .arg(&shim_src)
            .arg("-o")
            .arg(&obj_path)
            .status()
            .expect("failed to run `cc` to compile native_stdlib/net_shim.c (required for the Net stdlib module's extern-call resolution)");
        assert!(status.success(), "cc failed to compile native_stdlib/net_shim.c");
        // `-u <symbol>` per function — see `plum-interp/build.rs`'s own
        // copy of this same loop for why: `--gc-sections` (on by
        // default) silently drops the whole `net_shim.o` translation
        // unit otherwise, confirmed empirically, since nothing in the
        // statically-linked Rust code ever references these symbols
        // directly (every call goes through runtime symbol resolution).
        for symbol in ["tcp_connect", "tcp_listen", "tcp_accept", "tcp_send", "tcp_recv", "tcp_close"] {
            println!("cargo:rustc-link-arg=-Wl,-u,{symbol}");
        }
        // `--export-dynamic` — see `plum-interp/build.rs`'s own copy of
        // this comment for the full "why" (confirmed empirically
        // required, separately from `-u` above: `dlsym(RTLD_DEFAULT,
        // ..)` only searches `.dynsym`, and a normal PIE executable's
        // own local symbols aren't exported there by default).
        println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
        println!("cargo:rustc-link-arg={}", obj_path.display());
        println!("cargo:rerun-if-changed={}", shim_src.display());
    }
}
