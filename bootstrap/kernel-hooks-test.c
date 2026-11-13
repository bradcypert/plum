/* Hardware model for the real support files. No privileged instruction runs
 * here: platform.c is inspected separately as an x86-64 freestanding object. */
#include "kernel.h"
#include "platform.h"
#include <assert.h>
#include <setjmp.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint16_t config_ports[7];
static uint8_t config_values[7];
static size_t config_count;
static int dlab;
static uint8_t transmitted[65536];
static size_t transmitted_count;
static unsigned busy_reads, status_reads, halt_calls, free_calls, register_writes;
static jmp_buf halted;

uint8_t plum_kernel_port_read8(uint16_t port) {
    assert(port == 0x3fd);
    ++status_reads;
    if (busy_reads) { --busy_reads; return 0; }
    return 0x20;
}

void plum_kernel_port_write8(uint16_t port, uint8_t value) {
    assert(port >= 0x3f8 && port <= 0x3fc);
    if (port != 0x3f8 || dlab) ++register_writes;
    if (config_count < 7) {
        config_ports[config_count] = port;
        config_values[config_count++] = value;
    }
    if (port == 0x3fb) dlab = (value & 0x80) != 0;
    if (port == 0x3f8 && !dlab) {
        assert(transmitted_count < sizeof(transmitted));
        transmitted[transmitted_count++] = value;
    }
}

_Noreturn void plum_kernel_halt(void) {
    ++halt_calls;
    longjmp(halted, 1);
}

void __real_plum_kernel_free(void *allocation);
void __wrap_plum_kernel_free(void *allocation) {
    ++free_calls;
    __real_plum_kernel_free(allocation);
}

static void check_uart_config(void) {
    const uint16_t ports[] = {0x3f9, 0x3fb, 0x3f8, 0x3f9, 0x3fb, 0x3fa, 0x3fc};
    const uint8_t values[] = {0, 0x80, 1, 0, 3, 0xc7, 3};
    assert(config_count == 7 && register_writes == 7);
    for (size_t i = 0; i < 7; ++i) {
        assert(config_ports[i] == ports[i]);
        assert(config_values[i] == values[i]);
    }
}

#ifdef PLUM_KERNEL_UNIT_TEST
static void check_memory(void) {
    uint8_t actual[128], expected[128], original[128];
    for (size_t length = 0; length <= 32; ++length) {
        for (size_t source = 0; source <= 32; ++source) {
            for (size_t destination = 0; destination <= 32; ++destination) {
                for (size_t i = 0; i < 128; ++i) actual[i] = expected[i] = original[i] = (uint8_t)(i * 17);
                for (size_t i = 0; i < length; ++i) expected[destination + i] = original[source + i];
                assert(memmove(actual + destination, actual + source, length) == actual + destination);
                for (size_t i = 0; i < 128; ++i) assert(actual[i] == expected[i]);
            }
        }
    }
    for (size_t length = 0; length <= 64; ++length) {
        for (size_t i = 0; i < 128; ++i) { original[i] = (uint8_t)i; actual[i] = 0xa5; }
        assert(memcpy(actual + 11, original, length) == actual + 11);
        for (size_t i = 0; i < 128; ++i) {
            assert(actual[i] == (i >= 11 && i < 11 + length ? original[i - 11] : 0xa5));
        }
        assert(memcmp(actual, actual, length) == 0);
        assert(memset(actual + 11, 0x1ff, length) == actual + 11);
        for (size_t i = 0; i < 128; ++i) {
            assert(actual[i] == (i >= 11 && i < 11 + length ? 0xff : 0xa5));
            expected[i] = actual[i];
        }
        if (length) {
            expected[length - 1] ^= 0x80;
            int sign = actual[length - 1] < expected[length - 1] ? -1 : 1;
            assert((memcmp(actual, expected, length) < 0 ? -1 : 1) == sign);
            assert((memcmp(expected, actual, length) < 0 ? -1 : 1) == -sign);
            assert(memcmp(actual, expected, length - 1) == 0);
        }
    }
    actual[0] = 0xff; original[0] = 0;
    assert(memcmp(actual, original, 1) > 0);
    assert(memcmp(original, actual, 1) < 0);
}

int main(int argc, char **argv) {
    assert(plum_kernel_alloc(1) == NULL);
    if (argc == 2 && strcmp(argv[1], "early-panic") == 0) {
        if (setjmp(halted) == 0) plum_kernel_panic((const uint8_t *)"early", 5);
        check_uart_config();
        assert(halt_calls == 1 && transmitted_count == 13);
        assert(memcmp(transmitted, "panic: early\n", 13) == 0);
        assert(plum_kernel_alloc(1) == NULL); /* Output did not enable the heap. */
        return 0;
    }
    plum_kernel_runtime_init();
    check_uart_config();
    assert(transmitted_count == 0);
    uint8_t *a = plum_kernel_alloc(1);
    uint8_t *b = plum_kernel_alloc(15);
    assert(a && b && (uintptr_t)a % 16 == 0 && b == a + 16);
    plum_kernel_free(a); plum_kernel_free(NULL);
    uint8_t *c = plum_kernel_alloc(16);
    assert(c == b + 16); /* No-op free never rewinds. */
    plum_kernel_runtime_init();
    uint8_t *d = plum_kernel_alloc(17);
    assert(d == c + 16); /* Reinitialization never invalidates live values. */
    assert(plum_kernel_alloc(0) == NULL);
    assert(plum_kernel_alloc(UINT64_MAX) == NULL);
    assert(plum_kernel_alloc(UINT64_MAX - 14) == NULL);
    assert(plum_kernel_alloc(UINT64_MAX - 15) == NULL);
    assert(plum_kernel_alloc(PLUM_KERNEL_HEAP_BYTES + 1) == NULL);
    uint8_t *tail = plum_kernel_alloc(PLUM_KERNEL_HEAP_BYTES - 80);
    assert(tail == d + 32);
    tail[0] = 1; tail[PLUM_KERNEL_HEAP_BYTES - 81] = 2;
    assert(plum_kernel_alloc(1) == NULL);
    plum_kernel_free(tail); plum_kernel_runtime_init();
    assert(plum_kernel_alloc(16) == NULL);

    const uint8_t bytes[] = {'A', 0, '\n', 0xff};
    busy_reads = 3;
    plum_kernel_write(bytes, sizeof(bytes));
    assert(status_reads == 7 && transmitted_count == sizeof(bytes));
    assert(memcmp(transmitted, bytes, sizeof(bytes)) == 0);
    plum_kernel_write(NULL, 0);
    assert(status_reads == 7 && transmitted_count == sizeof(bytes));
    if (setjmp(halted) == 0) plum_kernel_panic((const uint8_t *)"out of memory", 13);
    assert(halt_calls == 1);
    assert(transmitted_count == sizeof(bytes) + 21);
    assert(memcmp(transmitted + sizeof(bytes), "panic: out of memory\n", 21) == 0);
    check_memory();
    assert(free_calls >= 3);
    puts("kernel-hooks-unit: allocator, UART, panic and memory primitives passed");
    return 0;
}
#else
int main(void) {
    if (setjmp(halted) == 0) {
        plum_kernel_runtime_init();
        check_uart_config();
        if (getenv("PLUM_TEST_OOM")) assert(plum_kernel_alloc(PLUM_KERNEL_HEAP_BYTES));
        plum_kernel_start();
    }
    assert(halt_calls == 1);
    int panicked = transmitted_count >= 7 && memcmp(transmitted, "panic: ", 7) == 0;
    if (!panicked && getenv("PLUM_TEST_EXPECT_FREE")) assert(free_calls > 0);
    fwrite(transmitted, 1, transmitted_count, stdout);
    return panicked ? 97 : 0;
}
#endif
