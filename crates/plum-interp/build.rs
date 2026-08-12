fn main() {
    // Extern-call tests (and any Plum program declaring `extern "C" { fn
    // sqrt(...) ... }`) resolve symbols against the CURRENT PROCESS's
    // own dynamic symbol table (see `resolve_extern_fn` in lib.rs) —
    // that only works for libm functions if libm is actually linked
    // into the process, which nothing else in this crate does on its
    // own (unlike libc, which Rust always links). Unix-only, matching
    // the rest of v1's Unix-only symbol-resolution scope.
    //
    // `--no-as-needed` is required alongside `-lm`, not optional: the
    // default Linux linker behavior drops a shared library's `NEEDED`
    // ELF entry entirely if nothing in the STATICALLY-linked object
    // files references any of its symbols — true here, since every
    // libm call goes through `dlsym`/`Library::this()` at runtime, not
    // a real linked reference. Without `--no-as-needed`, `-lm` compiles
    // fine but the resulting binary silently has no `libm.so` dependency
    // at all, and any `extern "C"` call to a real libm-only function
    // (confirmed with `floor`/`ceil`/`round`/`pow` — added once the
    // stdlib started wrapping them — fails at runtime with "undefined
    // symbol", even though `sqrt`/`cbrt` happened to keep working, since
    // those are ALSO present directly in `libc.so` on modern glibc,
    // masking the gap until a libm-only function was actually used).
    //
    // Both flags are emitted as raw `-C link-arg`s (not `cargo:rustc-
    // link-lib`), and in this exact order, deliberately: `cargo:rustc-
    // link-lib` only guarantees libm is referenced somewhere in the
    // link line for THIS package's own targets — for a DOWNSTREAM
    // binary (`plumc`'s own `plum` bin target, a separate rustc
    // invocation that only sees this crate as an already-built rlib),
    // rustc re-derives its own `-lm` from the rlib's embedded metadata
    // and inserts it BEFORE any `-C link-arg`s from a dependency reach
    // the final linker command — so a `--no-as-needed` added via a
    // separate `cargo:rustc-link-lib` call has already been overtaken
    // by the default `--as-needed` by the time that auto-derived `-lm`
    // is processed, silently dropping it again downstream (confirmed:
    // the two-directive version fixed it for `plum-interp`'s/`plumc`'s
    // OWN lib+test targets but not for the separate `plum` bin target).
    // Emitting `--no-as-needed` and `-lm` back-to-back as raw link args
    // instead keeps them adjacent at the END of the link line — always
    // after any earlier, already-stripped auto-derived reference — so
    // this SECOND, explicit `-lm` is the one that actually lands under
    // `--no-as-needed`, regardless of which crate does the final link.
    #[cfg(unix)]
    println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
    #[cfg(unix)]
    println!("cargo:rustc-link-arg=-lm");

    // Every `native_stdlib/*.c` shim (shared with `plumc`'s own
    // `codegen_cli.rs`, which links the SAME sources into every `plum
    // build` output) — needed here too because `plum run`'s extern-call
    // resolution works against the CURRENT PROCESS's own symbol table
    // (same reasoning as `-lm` just above), so e.g. `tcp_connect`/`dir_
    // open`/`process_run` have to actually be linked INTO this
    // interpreter binary, not just declared `extern "C"` by a Plum
    // program at runtime.
    //
    // Looping over a list, rather than one hand-written block per shim,
    // is a deliberate refactor (done when the SECOND and THIRD shims —
    // `dir_shim.c`/`process_shim.c` — were added alongside the
    // original `net_shim.c`): the compile-and-link recipe is identical
    // per file, just the filename and the exported-symbol list differ,
    // and hand-duplicating it a 2nd/3rd time was already starting to
    // drift-risk the way this exact file's own `-lm` block warns
    // AGAINST for a different reason above.
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
            // Rust's default linker flags include `--gc-sections` (dead-
            // code stripping) — confirmed empirically that WITHOUT
            // this, the whole shim's translation unit gets silently
            // discarded: nothing in the STATICALLY linked Rust code
            // ever references these symbols (every call goes through
            // `dlsym`/`Library::this()` at runtime, same "no real
            // linked reference" shape `-lm`'s own `--no-as-needed` note
            // above already describes for a different reason). `-u
            // <symbol>` per function is the surgical fix — forces the
            // linker to treat each name as a real GC root, without
            // disabling `--gc-sections` (and its real binary-size
            // benefit) for the rest of the binary the way a blanket
            // `--no-gc-sections` would.
            for symbol in symbols {
                println!("cargo:rustc-link-arg=-Wl,-u,{symbol}");
            }
            println!("cargo:rustc-link-arg={}", obj_path.display());
            println!("cargo:rerun-if-changed={}", shim_src.display());
        }
        // `--export-dynamic` — also confirmed empirically REQUIRED,
        // separately from `-u` above: `-u` only stops `--gc-sections`
        // from discarding the symbols, it doesn't make them visible to
        // `dlsym`. `Library::this()` resolves via `dlsym(RTLD_DEFAULT,
        // ..)`, which only searches a symbol's DYNAMIC table (`.dynsym`)
        // — a normal PIE executable's own locally-defined symbols
        // (unlike `sqrt`, which lives in `libm.so`'s own already-
        // dynamic table, a SEPARATE shared object dlsym also searches)
        // are NOT exported to `.dynsym` by default, only `.symtab`
        // (verified directly: `nm -D` showed `tcp_listen` present in
        // `.symtab` but absent from `.dynsym` before this flag, and
        // resolution genuinely failed at runtime as a result). One
        // flag covers every shim, not one per file.
        println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
    }
}

/// The single source of truth for "which `native_stdlib/*.c` shims
/// exist and which symbols each exports" — shared, in spirit if not in
/// literal code (Cargo build scripts can't easily share a module across
/// crates), with `plumc/build.rs`'s own identical copy and `codegen_
/// cli.rs`'s `ALL_NATIVE_SHIMS`. All three lists must stay in sync by
/// hand when a shim is added — a real, accepted duplication (four call
/// sites, matching `run_via_clang_with_c_helper`'s own precedent of
/// needing the fix applied everywhere a `clang` invocation happens),
/// not something a `build.rs`/library-crate boundary makes easy to
/// factor away for real.
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
