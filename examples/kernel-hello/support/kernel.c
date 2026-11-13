#include "kernel.h"
#include "platform.h"

#define COM1 0x3f8u

_Static_assert(sizeof(uintptr_t) == 8, "x86-64 pointers are required");
_Static_assert(PLUM_KERNEL_HEAP_BYTES % 16 == 0, "arena must contain whole aligned blocks");

_Alignas(16) static uint8_t arena[PLUM_KERNEL_HEAP_BYTES];
static uint64_t used;
static int heap_ready;
static int serial_ready;

static void serial_init(void) {
    if (serial_ready) return;
    plum_kernel_port_write8(COM1 + 1, 0x00); /* Disable UART interrupts. */
    plum_kernel_port_write8(COM1 + 3, 0x80); /* DLAB: divisor register access. */
    plum_kernel_port_write8(COM1 + 0, 0x01); /* 115200 baud on the PC UART. */
    plum_kernel_port_write8(COM1 + 1, 0x00);
    plum_kernel_port_write8(COM1 + 3, 0x03); /* 8 data bits, no parity, 1 stop. */
    plum_kernel_port_write8(COM1 + 2, 0xc7); /* Enable and clear both FIFOs. */
    plum_kernel_port_write8(COM1 + 4, 0x03); /* DTR/RTS, UART interrupt gate off. */
    serial_ready = 1;
}

void plum_kernel_runtime_init(void) {
    serial_init();
    /* BSS starts at zero. Never rewind used, including on repeated init. */
    heap_ready = 1;
}

void *plum_kernel_alloc(uint64_t size) {
    if (!heap_ready || size == 0 || size > UINT64_MAX - 15) return (void *)0;
    uint64_t aligned = (size + 15) & ~UINT64_C(15);
    if (aligned > sizeof(arena) - used) return (void *)0;
    void *allocation = arena + used;
    used += aligned;
    return allocation;
}

void plum_kernel_free(void *allocation) {
    /* Reference-count release still runs; this bounded arena never reclaims. */
    (void)allocation;
}

void plum_kernel_write(const uint8_t *data, uint64_t length) {
    /* Early panic output must not depend on successful heap initialization. */
    serial_init();
    for (uint64_t i = 0; i < length; ++i) {
        while ((plum_kernel_port_read8(COM1 + 5) & 0x20) == 0) { }
        plum_kernel_port_write8(COM1, data[i]);
    }
}

_Noreturn void plum_kernel_panic(const uint8_t *data, uint64_t length) {
    static const uint8_t prefix[] = "panic: ";
    static const uint8_t newline[] = "\n";
    plum_kernel_write(prefix, sizeof(prefix) - 1);
    plum_kernel_write(data, length);
    plum_kernel_write(newline, 1);
    plum_kernel_halt();
}
