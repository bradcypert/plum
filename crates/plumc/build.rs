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

    // Every `native_stdlib/*.c` shim — see `plum-interp/build.rs`'s own,
    // more detailed doc comment for the full "why" (needed so `plum
    // run`'s extern-call resolution against the CURRENT PROCESS's
    // symbol table finds `tcp_connect`/`dir_open`/`process_run`/etc.).
    // Duplicated here rather than inherited from `plum-interp`'s own
    // build script deliberately, mirroring the existing `-lm`/`--no-as-
    // needed` precedent immediately above: a dependency crate's `build.
    // rs` link-args don't propagate to a SEPARATE downstream binary
    // target (`plumc`'s own `plum` bin) — that's the exact reason this
    // file already re-emits `-lm` itself rather than trusting `plum-
    // interp`'s copy to cover it. `native_shims()`'s own list must be
    // kept in sync with `plum-interp/build.rs`'s identical copy (and
    // `codegen_cli.rs`'s `ALL_NATIVE_SHIMS`) by hand — see that
    // function's own doc comment for why that's an accepted
    // duplication, not an oversight.
    #[cfg(unix)]
    {
        let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        for (file_name, symbols) in native_shims() {
            let shim_src = manifest_dir.join("../../native_stdlib").join(file_name);
            let obj_path = out_dir.join(file_name).with_extension("o");
            let status = std::process::Command::new("cc")
                .arg("-c")
                .arg(&shim_src)
                .arg("-o")
                .arg(&obj_path)
                .status()
                .unwrap_or_else(|e| panic!("failed to run `cc` to compile native_stdlib/{file_name} (required for its stdlib module's extern-call resolution): {e}"));
            assert!(status.success(), "cc failed to compile native_stdlib/{file_name}");
            // `-u <symbol>` per function — see `plum-interp/build.rs`'s
            // own copy of this same loop for why: `--gc-sections` (on
            // by default) silently drops the whole translation unit
            // otherwise, confirmed empirically, since nothing in the
            // statically-linked Rust code ever references these
            // symbols directly (every call goes through runtime symbol
            // resolution).
            for symbol in symbols {
                println!("cargo:rustc-link-arg=-Wl,-u,{symbol}");
            }
            println!("cargo:rustc-link-arg={}", obj_path.display());
            println!("cargo:rerun-if-changed={}", shim_src.display());
        }
        // `--export-dynamic` — see `plum-interp/build.rs`'s own copy of
        // this comment for the full "why" (confirmed empirically
        // required, separately from `-u` above: `dlsym(RTLD_DEFAULT,
        // ..)` only searches `.dynsym`, and a normal PIE executable's
        // own local symbols aren't exported there by default). One flag
        // covers every shim, not one per file.
        println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
    }
}

/// See `plum-interp/build.rs`'s identical copy of this function for
/// the full "why this list, why duplicated" doc comment.
#[cfg(unix)]
fn native_shims() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("net_shim.c", &["tcp_connect", "tcp_listen", "tcp_accept", "tcp_send", "tcp_recv", "tcp_close"]),
        ("dir_shim.c", &["dir_open", "dir_read_next", "dir_close", "path_is_dir"]),
        (
            "process_shim.c",
            &["process_run", "process_exit_code", "process_stdout_data", "process_stderr_data", "process_free"],
        ),
    ]
}
