#ifndef PLUM_FREESTANDING_X86_64_PLATFORM_H
#define PLUM_FREESTANDING_X86_64_PLATFORM_H

#include <stdint.h>

/* Internal hardware boundary. Native tests supply a deterministic UART model
 * instead of executing privileged instructions. Kernel builds use platform.c. */
uint8_t plum_kernel_port_read8(uint16_t port);
void plum_kernel_port_write8(uint16_t port, uint8_t value);

#endif
