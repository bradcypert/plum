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

    // The TCP socket shim (`native_stdlib/net_shim.c`, shared with
    // `plumc`'s own `codegen_cli.rs`, which links the SAME source into
    // every `plum build` output) — needed here too because `plum run`'s
    // extern-call resolution works against the CURRENT PROCESS's own
    // symbol table (same reasoning as `-lm` just above), so `tcp_
    // connect`/`tcp_listen`/etc. have to actually be linked INTO this
    // interpreter binary, not just declared `extern "C"` by a Plum
    // program at runtime. Unlike `-lm` (a real shared library, needing
    // the `--no-as-needed` dance above), this compiles a plain `.o` and
    // passes it directly on the link line — an object file's own code
    // is always included in full by the linker regardless of whether
    // anything else statically references its symbols (that `--as-
    // needed`-drops-it problem is specific to a shared library's own
    // `NEEDED` ELF entry, not a directly-linked object file), so no
    // extra flag is needed here to keep it from being silently dropped.
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
        // Rust's default linker flags include `--gc-sections` (dead-code
        // stripping) — confirmed empirically (not assumed) that WITHOUT
        // this, the whole `net_shim.o` translation unit gets silently
        // discarded from the final binary: nothing in the STATICALLY
        // linked Rust code ever references `tcp_connect`/etc. (every
        // call goes through `dlsym`/`Library::this()` at runtime, same
        // "no real linked reference" shape `-lm`'s own `--no-as-needed`
        // note above already describes for a different reason). `-u
        // <symbol>` per function is the surgical fix — it forces the
        // linker to treat each name as a real GC root and keep it,
        // without disabling `--gc-sections` (and its real binary-size
        // benefit) for the rest of the binary the way a blanket
        // `--no-gc-sections` would.
        for symbol in ["tcp_connect", "tcp_listen", "tcp_accept", "tcp_send", "tcp_recv", "tcp_close"] {
            println!("cargo:rustc-link-arg=-Wl,-u,{symbol}");
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
        // resolution genuinely failed at runtime as a result).
        println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
        println!("cargo:rustc-link-arg={}", obj_path.display());
        println!("cargo:rerun-if-changed={}", shim_src.display());
    }
}
