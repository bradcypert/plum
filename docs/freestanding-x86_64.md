# x86-64 freestanding target and kernel ABI

Status: experimental compiler support for external kernels. This is not a
supported platform tier or an OS implementation. The
[tracking issue](https://github.com/bradcypert/plum/issues/514) records compiler
work; [Plum-OS](https://github.com/bradcypert/plum-os) owns the bootable OS consumer.
See [build commands](../RUNNING.md#freestanding-objects-and-elf-linking).

## Ownership boundary

Plum owns freestanding code generation, object/ELF output and explicit linking,
the generated entry wrapper, core runtime dependency selection, the hook ABI
and reusable language/FFI capabilities. Kernel authors own boot integration,
hardware drivers, memory policies and OS subsystems.

There is no bundled bootloader, UART driver, allocator, image layout or QEMU
image dependency in the compiler. The earlier sample hook implementations and
bootable kernel have moved to [Plum-OS](https://github.com/bradcypert/plum-os).
That project chooses Limine, COM1, its stack/heap sizes and image packaging.
Those are consumer policies, not requirements imposed on every Plum kernel.

Exception/interrupt tables, timers, page/frame management, reclaiming heaps,
scheduling, filesystems and userspace belong in the OS repository. Missing
reusable compiler/language capabilities discovered there should become focused
Plum issues.

## Scope and terms

A **freestanding profile** compiles without an operating system's startup code,
libraries or services. A **boot adapter** establishes the environment the
compiler-generated entry requires. **Kernel hooks** are native implementations
supplied by the kernel for the core runtime. These are separate boundaries:
neither the compiler nor its core runtime implements a boot protocol or driver.

| Property | Compiler profile |
| --- | --- |
| LLVM target | `x86_64-unknown-none`, explicitly selected |
| Representation | Little endian, 64-bit pointers and Plum `Int`, existing heap layouts |
| Baseline CPU | x86-64/SSE2; no host-native tuning or implicit AVX/newer features |
| Relocation/code model | Static, kernel code model; image symbols must remain within its upper 2 GiB address range |
| Native toolchain | Clang integrated assembler and LLD ELF; explicit object inputs/linker script |
| Output | Relocatable object or statically linked ELF with no host CRT/libraries |

## Code generation and linking

The freestanding profile enforces the target triple/data layout, static/kernel
code model and no red zone on generated LLVM IR and native helper compilation.
Passing `-mno-red-zone` only to C would not protect existing LLVM IR; generated
functions carry `noredzone` too.

Native compilation disables hosted builtins, stack protection, PIE/PIC and
unwind tables. No host CRT, libc, libm, pthreads, networking/TLS shims or implicit
compiler support libraries are linked. An unexpected lowering helper must be
supplied and audited by the consumer or rejected by final linking.

The kernel owns the linker script, executable image entry, segment layout and
boot requests. Use page-aligned executable/read-only/writable load regions and
avoid writable/executable segments. Map and zero BSS before entry. Preserve
whatever boot records the chosen protocol requires; Plum does not inspect them.
No interpreter, dynamic dependencies, unresolved symbols or required dynamic
relocations are allowed in the final freestanding image.

Compiler-generated operations can require `memcpy`, `memmove`, `memset` and
`memcmp`; the consumer supplies correct freestanding implementations, including
overlap handling for `memmove`. Compile such implementations without converting
memory loops into recursive calls to the same routines.

## Boot-to-Plum calling contract

The external kernel's native entry establishes machine state before calling
compiled C or Plum. Maskable interrupts and the direction flag must be cleared.
Use a mapped kernel-owned stack with System V AMD64 alignment: RSP is 16-byte
aligned before `call`, and the pushed return address gives the normal callee
entry alignment. Native helpers preserve System V callee-saved registers.
Stack size and the boot protocol are kernel choices.

Before compiled code, initialize x87/SSE: clear CR0.EM/TS, set CR0.MP and
CR4.OSFXSR/OSXMMEXCPT, initialize x87 control state and set MXCSR to `0x1f80`.
Baseline SSE2 may be used even for non-floating-point operations. AVX/XSAVE,
context switching and interrupt/exception handling are outside this profile's
initial entry contract. Disabling maskable interrupts does not prevent CPU
exceptions or NMIs; the kernel owns their handling.

The generated external entry is:

```c
_Noreturn void plum_kernel_start(void);
```

It initializes Plum globals once and invokes source `let main (): Unit` with
Plum's internal calling convention. Exactly one Unit parameter and a Unit
result are required; generic entries and other signatures are rejected. A
native caller must not call mangled Plum `main` directly: internal Unit is LLVM
`i1`, not C `void`. No C `main`, argc/argv or invented boot-register convention
is supplied.

The adapter initializes its hooks/allocator before `plum_kernel_start`; even
an empty program's globals may allocate. The wrapper does not initialize the
machine or allocator. Its BSS entry guard is set before globals initialize;
a repeated or recursive entry panics without rerunning them. Initially use one
CPU with interrupts disabled; the guard is not an SMP initialization primitive.

If source `main` returns, the wrapper calls `plum_kernel_halt`. Runtime failures
call `plum_kernel_panic`. Both hooks must never return, including to the adapter.
There is no host exit handler, locale setup, clock or random seeding.

## Core runtime and hooks

The runtime preserves checked arithmetic, bounds checks, existing heap layouts,
atomic reference counting and generated retain/release behavior. Kernel code
must not reinterpret heap values as ordinary C structures or change arithmetic
semantics. Atomic operations must lower without a host atomic library.

This private runtime-to-kernel ABI does not introduce a Plum pointer API:

```c
void *plum_kernel_alloc(uint64_t size); /* 16-byte aligned; NULL on failure */
void plum_kernel_free(void *allocation); /* NULL accepted */
void plum_kernel_write(const uint8_t *data, uint64_t length); /* borrowed bytes */
_Noreturn void plum_kernel_panic(const uint8_t *data, uint64_t length);
_Noreturn void plum_kernel_halt(void);
```

`uint64_t` and `uint8_t` are C fixed-width types. Allocation requests include
headers; the runtime checks NULL before writing a header/cell and panics on
failure. The kernel chooses its allocator. A no-op free is suitable only for
bounded fixtures; it does not reclaim memory even when reference counts and
child releases run. Sustained workloads need an appropriate reclaiming allocator.
Capacity-based reuse is disabled in this profile: preferred allocations request
only required storage, without `malloc_usable_size` or guessed capacities.

Output/panic pointers remain valid for the entire call. Hooks must not retain
or free borrowed bytes, allocate while writing/panicking, call back into Plum
or depend on initialized Plum globals. Preserve explicit byte lengths, including
embedded NUL bytes; panic reports a failure and halts. The output device and halt
implementation belong to the kernel. Do not call host `exit`.

The core supports checked integers, strings/bytes, arrays, records, variants,
closures, references, their reference counting and pure operations whose
transitive dependencies need only permitted memory primitives/hooks. Integer
formatting uses stack scratch. Floating-point formatting, libm operations and
CStr-to-String via hosted `strlen` are unavailable initially.

Runtime selection follows direct calls, globals and function-pointer references.
Unused hosted definitions/declarations are omitted; parsing one does not force
a hosted dependency. Reaching an unsupported hosted service produces a
diagnostic with its symbol path. Files, processes, environment, host clocks,
entropy, networking/TLS, threads/channels, host tracing/statistics and locale
services are initially unavailable. Do not substitute fake successful OS calls.
Explicit user/dependency externs remain trusted native adapter boundaries.

## Verification and external consumer

Plum's `bootstrap/freestanding-check.py` verifies object/link constraints and
cache isolation. `bootstrap/freestanding-runtime-check.py` uses small host-side
mock hooks to verify core behavior, lifetime handling, checked failures, OOM,
entry initialization/halt/re-entry and hosted-service diagnostics. Run them with
a compiler built from current source. They require no bootloader or OS driver.

[Plum-OS](https://github.com/bradcypert/plum-os) owns hardware-hook tests, its
QEMU image/smoke checks, boot configuration and OS development. Its tests verify
the external integration; Plum's tests verify compiler/runtime contracts. Pin a
compatible compiler revision in the OS repository and link compiler regressions
back to focused Plum issues.

## Implementation map

- #505: initial target/ABI contract, now separated from consumer boot policies.
- #506: target validation, object/link pipeline and cache profile separation.
- #507: core runtime dependency selection and hosted-service rejection.
- #508: stable entry wrapper, signature validation and one-time initialization.
- #509: hook support; concrete hardware/allocator implementations now live in Plum-OS.
- #511: compiler-owned object/ELF and runtime ABI conformance verification.
- #512: reusable low-level language/FFI design for external consumers.

OS integration and OS-facility issues are tracked in
[Plum-OS](https://github.com/bradcypert/plum-os/issues).
