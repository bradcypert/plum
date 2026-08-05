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
}
