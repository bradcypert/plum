# Plum kernel hello

A bounded, one-CPU boot demonstration for the experimental
`x86_64-unknown-none` profile. Limine loads the kernel, a small C/assembly adapter
establishes the environment, and Plum writes `PLUM_KERNEL_OK\n` to COM1 before
halting. It uses the existing scalar extern ABI; no new pointer types or inline
assembly in Plum are required.

## Project boundary

Plum enables external kernels; it does not ship an operating system. This
example is a small compiler/runtime integration fixture, not the starting point
for an OS maintained inside the language repository.

Plum owns freestanding code generation, object/ELF output, the generated entry
wrapper, the core runtime hook ABI, and reusable language/FFI capabilities.
Everything in this directory is optional example code: the Limine adapter,
UART driver, arena allocator, memory primitives, hardware helpers and image
packaging are explicitly linked here, never selected or embedded by the compiler.
External kernels supply their own implementations of the documented hooks and
choose their own bootloader, hardware drivers and memory policies.

Exception/interrupt tables, timers, physical/virtual memory management, reclaiming
heaps, scheduling, filesystems and other OS facilities belong in a separate
kernel project. This fixture stays bounded to boot, serial output, halt and
small runtime conformance checks; it is not a platform for expanding OS features.

## Build and run

Run these commands from the repository root on Linux x86-64. Required tools:
Python 3, Clang with x86-64 support and its integrated assembler, LLD (`ld.lld`),
binutils (`nm`, `readelf`), `curl` for the first download, `xorriso`, and
`qemu-system-x86_64` with PC BIOS firmware (normally packaged with QEMU).
For Debian/Ubuntu, install `clang lld binutils curl xorriso qemu-system-x86`.

Use a compiler built from current source, as CI does:

```sh
./bootstrap/from-seed -o /tmp/plum-kernel-seed
/tmp/plum-kernel-seed build bootstrap/self_host -o /tmp/plum-kernel-compiler
python3 examples/kernel-hello/build.py --plum /tmp/plum-kernel-compiler
python3 examples/kernel-hello/smoke.py
```

The build writes `kernel.elf`, `kernel.iso`, native objects, and `toolchain.json`
under `examples/kernel-hello/build/`. Add `--release` to `build.py` for an
optimized image. Both scripts accept `--output /absolute/output/directory`;
use the same directory for build and smoke. `build.py --plum` defaults to
`./sh.real`; it does not rebuild the compiler automatically.

To run interactively instead of the bounded smoke check:

```sh
qemu-system-x86_64 -machine pc -accel tcg -cpu qemu64 -smp 1 -m 128M \
  -cdrom examples/kernel-hello/build/kernel.iso -boot d \
  -display none -serial stdio -monitor none -no-reboot -no-shutdown
```

The expected serial bytes are exactly `PLUM_KERNEL_OK\n`. Normal halt keeps
QEMU running; stop the interactive run with Ctrl-C. This boots an ISO with
BIOS, rather than relying on QEMU's `-kernel` ELF handling. UEFI is not included.

The smoke check has a 20-second deadline (override with `--timeout SECONDS`). It
uses QMP over pipes to require both the exact serial bytes and `HLT=1` with RIP
inside `plum_kernel_halt`, then terminates QEMU. A timeout or a marker without
the expected halt fails. It saves `serial.log` and `halt-registers.txt` and adds
the actual QEMU version to `toolchain.json`. Broader panic/allocation fixtures,
GDB checks, and code-generation audits belong to #511.

## Pinned boot inputs

The build fetches only the immutable commit archive below, verifies SHA-256
before extraction/compilation, and verifies the matching header separately.
It never follows a moving branch. The archive is cached in the output directory;
a bad cached checksum fails rather than silently fetching a replacement.
For offline use, pass `--limine-archive /path/to/archive.tar.gz`; checksum
verification still applies. Limine's license accompanies the extracted files
and is copied into the ISO at `/boot/limine/LICENSE`.

| Input | Pin |
| --- | --- |
| Limine binary distribution | `v8.7.0-binary`, commit `aad3edd370955449717a334f0289dee10e2c5f01` |
| Source URL | <https://codeload.github.com/Limine-Bootloader/Limine/tar.gz/aad3edd370955449717a334f0289dee10e2c5f01> |
| Archive SHA-256 | `9c23ca261778cbe015041c02220ce26f45e05f38fe3e0c9913be71961f9023e5` |
| Protocol header | `limine.h` from that same archive; base revision 3, header API revision 0 |
| Header SHA-256 | `f8b35287579dbe50e9d84cca7b558e39b2b64e499f9b37ef5ebe969c05104c33` |

Boot configuration, protocol and bootloader inputs are pinned. ISO timestamps
and local toolchain/compiler versions are not normalized for byte-identical
images. `toolchain.json` records the versions actually used; CI prints it too.
Locally tested: Clang/LLD 23.1.4, xorriso 1.5.8.pl02, QEMU 11.1.3, debug and
release. These are tested versions, not minimum version claims.

## Native boundary and machine setup

| File | Responsibility |
| --- | --- |
| `boot/start.S` | Disable maskable interrupts, clear DF, select a private 64 KiB aligned BSS stack, initialize x87/SSE and MXCSR, call the C adapter with System V alignment |
| `boot/adapter.c` | Negotiate Limine base revision 3; require HHDM, a nonempty memory map and four-level paging; retain borrowed boot data; initialize hooks before the generated entry |
| `boot/kernel.ld` | Higher-half ELF64 ET_EXEC at `0xffffffff80000000`, `_start` entry, page-aligned RX/R/RW regions, writable retained request records, mapped BSS and debug sections |
| `boot/hardware.c` | C ABI serial write, interrupt disable/status and non-returning halt helpers |
| `main.plum` | Check interrupts remain disabled, emit the fixed serial marker, halt |

The adapter retains the HHDM offset returned by Limine and the borrowed memory
map. It does not guess address offsets, reclaim boot memory, change CR3/GDT,
start secondary CPUs, or enable interrupts. Limine zero-fills BSS. Failures use
`plum_kernel_panic` before heap initialization, sharing the reference UART
initializer so later initialization cannot clear pending adapter output.

The helper signatures match the current emitter:

| Plum | C |
| --- | --- |
| `CStr` | borrowed `const char *` data pointer |
| `Int` | `int64_t` |
| extern `Bool` | `int` (LLVM `i32`), **not** `_Bool` |
| extern `Unit` | `void` |

`plum_x86_serial_write` takes an explicit nonnegative byte length; callers must
supply a valid borrowed span for the entire call. The hello marker is 15 ASCII
bytes including LF; its CStr terminator is not transmitted. No `strlen` is used.
Only disable/status interrupt helpers are exposed: enabling interrupts requires
a kernel-owned IDT and exception handling, which this demo does not supply.

The [example support](support/README.md) uses a
1 MiB bump arena with no-op frees. This is a bounded boot demo, not a sustained
allocator or a general OS runtime. See the
[target/entry contract](../../docs/freestanding-x86_64.md).

Protocol/configuration references for the selected release:
[Limine protocol](https://github.com/Limine-Bootloader/Limine/blob/v8.7.0/PROTOCOL.md),
[Limine configuration](https://github.com/Limine-Bootloader/Limine/blob/v8.7.0/CONFIG.md).
