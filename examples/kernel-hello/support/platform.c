#include "kernel.h"
#include "platform.h"

uint8_t plum_kernel_port_read8(uint16_t port) {
    uint8_t value;
    __asm__ volatile ("inb %1, %0" : "=a"(value) : "Nd"(port) : "memory");
    return value;
}

void plum_kernel_port_write8(uint16_t port, uint8_t value) {
    __asm__ volatile ("outb %0, %1" : : "a"(value), "Nd"(port) : "memory");
}

_Noreturn void plum_kernel_halt(void) {
    /* An NMI can wake HLT even with maskable interrupts disabled. */
    for (;;) __asm__ volatile ("cli; hlt" : : : "memory");
}
