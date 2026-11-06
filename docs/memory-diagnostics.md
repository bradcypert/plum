# Sanitizer builds and memory measurements

Use a source-built compiler containing the sanitizer build mode. Native Linux
and macOS are supported with a Clang toolchain and its sanitizer runtimes;
Windows and cross-target sanitizer builds are explicitly rejected. On macOS,
use an upstream LLVM toolchain when the system Clang runtime lacks leak detection.
Put `llvm-symbolizer` on PATH, or set `ASAN_SYMBOLIZER_PATH` to its full path.
The runner verifies leak detection before running your project.

```sh
./bootstrap/sanitize-run --plum /path/to/plum /path/to/project -- application-arguments
./bootstrap/sanitize-run --plum /path/to/plum --release /path/to/project
```

`sanitize-run` builds with `--sanitize address`, then executes with leak detection,
symbolization and nonzero failure status enabled. It first compiles and runs a
known leak fixture with the same toolchain/settings. If that fixture does not
produce a failing LeakSanitizer report, the runner fails. Build failures,
sanitizer setup failures and permission restrictions are failures; it never
retries with leak detection disabled. Ambient `detect_leaks=0` or `exitcode=0`
settings cannot silently turn a finding into a passing run. Project arguments
follow the project; an optional first `--` is consumed by the runner.

For direct control, the same build policy is available to all compilation paths:

```sh
plum build project --sanitize address -o /tmp/project-asan
ASAN_OPTIONS=detect_leaks=1 /tmp/project-asan
plum run project --sanitize=address -- application-arguments
ASAN_OPTIONS=detect_leaks=1 plum test project --sanitize address
plum emit-llvm project --sanitize address > /tmp/project.ll
```

The direct commands obey sanitizer runtime settings. Use `sanitize-run` for the
verified ASan/LSan workflow. `--release` retains `-O2` while adding symbols in
sanitizer mode. Optimization and tail-call elimination remain enabled, so stacks
can omit inlined/tail-called frames. `--trace` remains available separately.
For `run`, compiler options precede `--`; tokens after it belong to the program.

## Instrumentation coverage

Generated source functions, runtime helpers, synthesized destructors and entry
functions carry LLVM `sanitize_address` and frame-pointer attributes. Merely
passing `-fsanitize=address` when linking LLVM IR does not enable address checks
on arbitrary generated function loads. Native compilation and linking receive
the sanitizer flags too, including embedded C/TLS sources and project/dependency
C sources. Prebuilt external libraries must be rebuilt separately for equivalent
instrumentation; this is not a whole-process instrumentation guarantee.

IR cache keys include the complete build policy; native object keys include
the sanitizer compiler flags and preprocessed C inputs. Ordinary and sanitized
artifacts remain distinct. Cached and uncached builds, test dispatchers, and
single-file/project IR emission use the same policy. macOS symbol extraction
runs while its object files still exist.

`bootstrap/sanitizer-check.py /path/to/plum` exercises generated Plum and native
C invalid accesses, intentional leaks, clean shared ownership, debug/release
source symbols, instrumentation cache transitions and runner preflight failures.
It is a diagnostic suite: its intentionally invalid programs must fail with the
specified sanitizer diagnostics. They are outside the ordinary execution corpus.

## The larger memory story

ASan detects invalid accesses and LSan detects unreachable allocations at exit;
a clean workload is evidence about that workload, not proof of all ownership
behavior. Live reachable memory may be intentional or excessive without being
an LSan leak. Sanitizers also change allocator behavior, timing and RSS, so do
not compare their footprint directly with ordinary-build performance baselines.

`PLUM_RT_STATS` reports cumulative allocation traffic, including freed cells.
It does not report live/peak retained bytes. The approved next stage is opt-in
Plum-cell lifetime tracking and coherent snapshots; see the
[delivery plan](memory-diagnostics-plan.md). Perceus ownership, last-owner release,
and in-place reuse remain the production memory model. This sanitizer stage
adds no per-cell accounting metadata to ordinary builds.

References: [Clang ASan](https://clang.llvm.org/docs/AddressSanitizer.html),
[Clang LSan](https://clang.llvm.org/docs/LeakSanitizer.html), and
[LLVM function attributes](https://llvm.org/docs/LangRef.html#function-attributes).
