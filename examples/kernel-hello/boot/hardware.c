#include "../support/kernel.h"

/* Plum Int is i64; CStr is a borrowed char pointer; extern Bool is C int,
 * not C _Bool. Unit-returning extern functions use C void. */
void plum_x86_serial_write(const char *bytes, int64_t length) {
    if (length < 0) {
        static const uint8_t message[] = "negative serial length";
        plum_kernel_panic(message, sizeof(message) - 1);
    }
    plum_kernel_write((const uint8_t *)bytes, (uint64_t)length);
}

void plum_x86_interrupts_disable(void) {
    __asm__ volatile ("cli" : : : "memory");
}

int plum_x86_interrupts_enabled(void) {
    uint64_t flags;
    __asm__ volatile ("pushfq; popq %0" : "=r"(flags) : : "memory");
    return (flags & (UINT64_C(1) << 9)) != 0;
}

_Noreturn void plum_x86_halt(void) {
    plum_kernel_halt();
}
