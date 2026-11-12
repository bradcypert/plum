# x86-64 freestanding target and kernel ABI

Status: implementation contract for [#505](https://github.com/bradcypert/plum/issues/505).
This profile is not implemented or a supported platform tier yet. The
[tracking issue](https://github.com/bradcypert/plum/issues/514) records the work
needed to make it executable. Requirements below describe the intended behavior,
not capabilities of today's compiler. Normal hosted builds retain their behavior.

## Scope and terms

A **freestanding profile** compiles without an existing operating system's
startup code, libraries, or services. A **boot adapter** translates the selected
boot protocol into the environment the generated Plum entry wrapper requires.
**Kernel hooks** are native functions supplied by the kernel for the core runtime.
These are separate boundaries: the compiler does not implement the boot protocol,
and the core runtime does not implement device drivers.

The first milestone runs one CPU in QEMU, writes `PLUM_KERNEL_OK\n` from Plum
through a serial helper, and halts. Subsequent fixtures exercise a checked panic
and bounded heap operations. Scheduling, interrupts, paging ownership, and a
reclaiming heap follow later; they are not prerequisites for the first message.

## Target and toolchain

| Property | Initial choice |
|---|---|
| LLVM target | `x86_64-unknown-none`, explicitly selected; never inferred from the build host |
| Representation | Little endian, 64-bit pointers and Plum `Int`, existing Plum heap layouts |
| Machine | QEMU `pc`, TCG, `-cpu qemu64`, `-smp 1`, `-m 128M` |
| Firmware / boot | QEMU PC BIOS, Limine bootloader and Limine protocol base revision 3 |
| Compiler / assembler | Clang with its integrated assembler; Plum emits LLVM IR |
| Linker | LLD ELF (`ld.lld`), explicit linker script and object inputs |
| Image packaging | Limine BIOS-bootable ISO, produced with `xorriso` and Limine's installation tooling |
| Inspection / debug | `readelf`, `nm`, `objdump` (or LLVM equivalents), GDB and QEMU's GDB stub |

Use a matching, pinned Limine binary release and protocol header supporting base
revision 3. The boot-example change (#510) must record their exact versions,
source URLs and checksums; do not download a moving branch during builds. The
adapter verifies base-revision negotiation and required responses before use.
The first CI image must also record the exact Clang/LLD/QEMU versions it tests;
this contract does not claim an untested minimum tool version.

Limine is chosen to avoid a custom real-mode loader or a separate 32-to-64-bit
transition. UEFI, Multiboot, other machines/architectures, and 32-bit targets are
outside the initial contract. Bootloader-specific declarations stay in native
code; ordinary Plum structs are not protocol structures.

## ELF layout and code generation

Produce a statically linked ELF64 `ET_EXEC` for x86-64 with entry `_start`.
Use virtual image base `0xffffffff80000000`, static relocation and the kernel
code model. All image symbols, including the early heap and stack, must remain
in the upper 2 GiB address range, within code-model displacement limits. Disable
KASLR and PIE for this image. Physical placement belongs to the bootloader; do
not assume that virtual addresses equal physical addresses or subtract a guessed
offset to access memory.

The linker script must define page-aligned loadable regions for executable code,
read-only data, and writable data/BSS, with no writable/executable segment. Keep
Limine's request delimiters and request records even when section GC is enabled;
their response-pointer storage must be writable. Zero-initialized globals,
the private stack, and the early heap belong in mapped BSS. Retain debug sections
outside loadable regions. No ELF interpreter, dynamic dependencies, unresolved
symbols, or required dynamic relocations are allowed.

The build profile must enforce:

- Baseline x86-64/SSE2; no host-native tuning, AVX, or implicit newer CPU features.
- No red zone for **every** generated/helper function. Plum LLVM IR must carry
  `noredzone`; passing `-mno-red-zone` only when compiling C is insufficient.
- The kernel code model and static relocation on LLVM IR compilation and linking,
  not just the native C compilation path. Emit the target triple and a matching
  target data layout rather than relying on Clang's host defaults.
- Freestanding C compilation with no stack protector, PIE/PIC, TLS, exceptions,
  stack unwinder, sanitizer, profiling, or hosted runtime instrumentation.
- No host CRT, libc, libm, pthreads, bundled networking/TLS shims, or implicit
  compiler support libraries. Supply and audit any machine-code helper required
  by lowering; an unexpected helper is a build failure, not an ignored symbol.

Compiler-generated memory operations can require `memcpy`, `memmove`, `memset`,
or `memcmp`. The native support layer supplies correct freestanding versions as
needed, including overlap behavior for `memmove`. Build those implementations
without turning their loops back into recursive calls to the same routines.
Verify effective IR attributes and final objects: driver flags alone are not
proof that pre-existing LLVM IR received the required settings.

## Boot-to-Plum calling contract

`_start` is assembly and must not return to Limine. It disables maskable
interrupts, clears the direction flag, switches to a kernel-owned 64 KiB stack,
and establishes CPU state before calling compiled C or Plum. The stack top is
16-byte aligned **before** `call`; the callee sees the usual System V AMD64
entry alignment after the return address is pushed. Native helpers preserve
System V callee-saved registers. No boot argument is passed in an invented
register convention: the adapter reads Limine request responses directly.

Before compiled code, initialize x87/SSE: clear CR0.EM/TS, set CR0.MP and
CR4.OSFXSR/OSXMMEXCPT, initialize the x87 control state, and set MXCSR to `0x1f80`.
Code may use baseline SSE2, including for non-floating-point work. No AVX/XSAVE
state or context switching is required. This does not enable libm-dependent
operations. FP/SIMD is an internal kernel choice, not a boot-protocol call ABI.

Keep the bootloader's mappings and descriptor environment initially. Do not
reclaim bootloader memory, load a new CR3, enable interrupts, or start secondary
CPUs. Install kernel-owned exception/interrupt handling before those capabilities
are introduced (#513). Disabling interrupts does not prevent CPU exceptions or
NMIs; this early demo has no recovery guarantee for either.

The generated external symbol is `plum_kernel_start` with C type `void(void)`.
It initializes Plum globals once and invokes source `let main (): Unit` using
Plum's internal calling convention. A native caller must never call the mangled
Plum `main` directly: today `Unit` is represented as LLVM `i1`, not C `void`.
The wrapper calls the halt hook if `main` returns and never returns itself.
There is no `argc`/`argv`, locale initialization, exit handler, or clock/random
seeding. Dynamic initializers run only after hooks and the allocator are ready.

## Core runtime and hooks

Preserve checked arithmetic, bounds checks, existing heap layouts, atomic
reference counting, and generated retain/release behavior. Do not reinterpret
heap values as C structures or silently change arithmetic semantics. Atomic
operations must lower without an external atomic library on this target.

The following C ABI is private runtime-to-kernel glue, not a new Plum pointer API:

```c
void *plum_kernel_alloc(uint64_t size); /* 16-byte aligned; NULL on exhaustion */
void plum_kernel_free(void *allocation); /* NULL is accepted */
void plum_kernel_write(const uint8_t *data, uint64_t length); /* borrowed bytes */
_Noreturn void plum_kernel_panic(const uint8_t *data, uint64_t length);
_Noreturn void plum_kernel_halt(void);
```

Here `uint64_t` and `uint8_t` denote the C fixed-width types. The runtime checks
allocation failure before writing a refcount/header and calls panic on failure.
The allocator receives the complete allocation size, including headers. Borrowed
output/panic bytes must remain valid for the call; hooks neither retain nor free
them and must not allocate, call back into Plum, or require initialized globals.
Panic writes a diagnostic and halts. Halt is a `cli; hlt` loop, including if an
NMI wakes the CPU. Neither hook calls host `exit` or returns.

Start with a statically reserved, 16-byte-aligned 1 MiB BSS arena. Validate
alignment rounding, overflow, and exhaustion. No-op free is allowed only for
bounded boot fixtures: reference counts still run, but arena space is not
reclaimed. Disable size-based allocation reuse in this profile initially; do
not call `malloc_usable_size` or guess allocation capacity. A reclaiming allocator
and a capacity contract are separate follow-up work.

The console is a polled 16550-compatible COM1 UART at port `0x3f8`, configured by
native code for 115200 baud, 8 data bits, no parity, one stop bit, with serial
interrupts disabled. Output preserves bytes, including LF, without locale or
terminal translation. To print the first marker, Plum uses a native helper via
the existing scalar FFI (for example a borrowed `CStr` input); raw pointers in
the hook ABI stay internal to generated runtime/native code. Initial core output
must not require general printf formatting or float-to-string support.

Supported core features must be explicitly enumerated by #507. Files, processes,
environment variables, host clocks, entropy, networking/TLS, threads/channels,
host tracing/statistics, locale services, and libm-dependent operations are
unavailable initially. Reject unsupported reachable operations with diagnostics;
do not substitute fake successful OS operations. Merely parsing an unused
prelude definition must not force a hosted dependency into the kernel.

## Initialization and verification

The ordered startup sequence is:

1. Limine maps the ELF and zero-fills BSS, then enters `_start`.
2. Assembly establishes interrupts/direction flag, private stack, and FP/SIMD state.
3. Native adapter initializes COM1 and validates boot revision and required
   responses. Request HHDM, memory map and four-level paging; keep boot pointers
   borrowed and bootloader memory reserved. Read the HHDM offset from its response.
4. Initialize the static arena and runtime hooks; failures now use serial/halt.
5. Call `plum_kernel_start`, which initializes globals and invokes Plum `main`.
6. Plum writes `PLUM_KERNEL_OK\n`; returning from `main` reaches the halt hook.

The future example's reference invocation, after building `kernel.iso`, is:

```sh
qemu-system-x86_64 -machine pc -accel tcg -cpu qemu64 -smp 1 -m 128M \
  -cdrom kernel.iso -boot d -display none -serial stdio -monitor none \
  -no-reboot -no-shutdown
```

This is an ISO boot, not an assertion that QEMU's `-kernel` accepts this ELF.
For debugging add `-s -S`, load the unstripped `kernel.elf` in GDB and connect
with `target remote localhost:1234`. The smoke harness must bound execution:
normal halt deliberately keeps QEMU running. A marker alone does not prove halt;
#511 must separately observe the terminal halt path (for example through GDB)
and distinguish reset, triple fault, and timeout before success. Use separate
normal, checked-panic, and allocation fixtures with distinct expected markers.

Inspect final ELF headers, segments, symbols, and disassembly. Verify target,
entry, permissions, retained requests, absence of hosted dependencies, and
effective codegen constraints. Build the compiler from current source. First boot
does not establish an unrestricted stdlib or sustained allocator support.

## Implementation map and references

- #506: target validation, object/link pipeline, and cache profile separation in
  `bootstrap/self_host/main.plum`.
- #507: runtime selection/reachability in `codegen/codegen.plum` and
  `codegen/runtime.plum` under `bootstrap/self_host/`.
- #508: replace the hosted `cg_entry_named` path for this profile only.
- #509: core runtime hooks, failure routing, and bounded allocator.
- #510: pinned boot dependencies, native adapter, image and example.
- #511: object/ELF inspection, QEMU fixtures, GDB halt checks and CI.
- #512 and #513: direct low-level language APIs and later kernel facilities.

External specifications inform the boot/toolchain boundary; the profile choices
and narrower supported surface above are Plum requirements:

- [Limine protocol](https://github.com/limine-bootloader/limine-protocol/blob/trunk/PROTOCOL.md)
  (base revisions, requests, memory layout and machine state; pin the matching
  header/document revision with the example).
- [Clang command-line reference](https://clang.llvm.org/docs/ClangCommandLineReference.html).
- [LLVM language reference](https://llvm.org/docs/LangRef.html)
  (target layout, `noredzone`, and module code-model metadata).
- [QEMU invocation](https://www.qemu.org/docs/master/system/invocation.html)
  and [GDB usage](https://www.qemu.org/docs/master/system/gdb.html).
