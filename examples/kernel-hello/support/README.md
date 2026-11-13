# Example x86-64 kernel hooks

These are optional example kernel hooks for Plum's experimental
`x86_64-unknown-none` profile. They support a **bounded boot demonstration**,
not a sustained workload: the 1 MiB arena never reclaims storage. Allocations
will eventually exhaust it even when reference-counted values are released.
A reclaiming heap belongs in the consuming OS project, outside Plum.
These hardware and allocator policies are not a standard runtime implementation.
The compiler supplies only the hook ABI; it does not embed or select these files.

A boot adapter and bootable BIOS image are supplied by the
[kernel-hello example](../../../examples/kernel-hello/README.md) (#510).
See the [target/entry contract](../../../docs/freestanding-x86_64.md).

## Initialization and sources

The native boot adapter must establish the CPU/x87/SSE state, aligned System V
stack, mapped and zero-filled BSS, and disabled maskable interrupts first.
Use one CPU and call these functions in this order:

```c
#include "kernel.h"

_Noreturn void enter_plum_after_machine_setup(void) {
    plum_kernel_runtime_init();
    plum_kernel_start();
}
```

`plum_kernel_runtime_init` prepares COM1 and enables the arena before Plum's
global initializers allocate. It is idempotent: repeating it never resets the
allocation cursor or clears pending kernel serial output. Writing or panicking
before heap initialization can prepare the serial port alone; this does not
enable allocations. There is no heap reset or deinitialization API.

Compile all three support sources with the freestanding profile. For example,
a project with its own startup assembly and linker script can use:

```sh
plum build mykernel --target x86_64-unknown-none \
  --link-c examples/kernel-hello/support/kernel.c \
  --link-c examples/kernel-hello/support/memory.c \
  --link-c examples/kernel-hello/support/platform.c \
  --link-asm start.S --linker-script kernel.ld -o kernel.elf
```

Paths above are relative to the repository root. The compiler neither embeds
nor automatically links these files. `kernel.c` implements allocation and UART
policy; `platform.c` contains privileged port I/O and halt instructions;
`memory.c` supplies `memcpy`, `memmove`, `memset`, and `memcmp`. Use the profile's
`-ffreestanding -fno-builtin` flags when compiling them separately, including in
release mode, to prevent memory loops from turning into self-calls.

## Hook behavior

| Hook | Behavior |
|---|---|
| `plum_kernel_alloc` | Monotonic allocation, rounded to 16-byte alignment; returns NULL before initialization, for zero size, alignment overflow, or exhaustion. Failed requests do not advance the cursor. |
| `plum_kernel_free` | No-op, including for NULL. Compiler-generated reference-count decrements and child releases still run. |
| `plum_kernel_write` | Polled COM1 (`0x3f8`), 115200 baud at the PC UART clock, 8N1, FIFO enabled, UART interrupts disabled. Writes exactly the borrowed bytes; no LF/CR or NUL translation. NULL with length zero is accepted. |
| `plum_kernel_panic` | Writes `panic: `, the supplied bytes, and LF, then calls the halt hook. Does not allocate. |
| `plum_kernel_halt` | Repeats `cli; hlt` so an NMI wakeup cannot fall through. Never returns. |

The arena has no allocation-size metadata. The compiler's current freestanding
profile disables capacity-dependent reuse; do not combine these hooks with a
runtime that assumes `malloc_usable_size` exists. Empty Plum values still work:
zero-size raw allocation requests are distinct from valid cells with headers.

Serial output assumes the QEMU PC's 16550-compatible COM1 exists and eventually
accepts bytes. A missing/unresponsive UART can block output; there is no timeout,
interrupt-driven driver, concurrent access, or fault/NMI recovery at this stage.
The polling/register definitions follow the
[16550 definitions](https://git.ti.com/cgit/ti-u-boot/ti-u-boot/tree/include/ns16550.h?id=3ade5bc4dc24edf5e1f13f3c43a9e8b7f8c2d853)
and [transmit sequence](https://git.ti.com/cgit/ti-u-boot/ti-u-boot/tree/drivers/serial/ns16550.c?id=29caf9305b6fafe8f6d6b18fa1f825dff8686e61)
in TI's U-Boot source; the PC address/clock and bounded heap are Plum choices.

## Verification

Run `python3 bootstrap/kernel-hooks-check.py /path/to/source-built-plum` from
the repository root. It requires Linux x86-64, Clang/LLD, and binutils.

The harness compiles the actual support sources at `-O0` and `-O2`, audits their
undefined symbols, and inspects the privileged object's port-I/O/halt opcodes.
Native tests substitute only `platform.c` with a UART/halt model to exercise
alignment, exact exhaustion, overflow, no-op frees, initialization, byte output,
backpressure, panic, and memory overlap/byte boundaries. Debug/release Plum
fixtures execute with the real allocator/output/panic/memory code and check
release calls through a linker wrapper. A final static ELF uses the actual
privileged platform implementation and must have no unresolved symbols or
host loader/library dependencies. The ELF is inspected, not booted; this is not
proof of a working QEMU image.
