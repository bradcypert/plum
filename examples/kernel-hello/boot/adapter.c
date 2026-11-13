#include <stdint.h>
#include "limine.h"
#include "../../../runtime/freestanding/x86_64/kernel.h"

__attribute__((used, section(".limine_requests_start")))
static volatile LIMINE_REQUESTS_START_MARKER
__attribute__((used, section(".limine_requests")))
static volatile LIMINE_BASE_REVISION(3)
__attribute__((used, section(".limine_requests")))
static volatile struct limine_hhdm_request hhdm_request = {
    .id = LIMINE_HHDM_REQUEST, .revision = 0
};
__attribute__((used, section(".limine_requests")))
static volatile struct limine_memmap_request memmap_request = {
    .id = LIMINE_MEMMAP_REQUEST, .revision = 0
};
__attribute__((used, section(".limine_requests")))
static volatile struct limine_paging_mode_request paging_request = {
    .id = LIMINE_PAGING_MODE_REQUEST, .revision = 1,
    .mode = LIMINE_PAGING_MODE_X86_64_4LVL,
    .max_mode = LIMINE_PAGING_MODE_X86_64_4LVL,
    .min_mode = LIMINE_PAGING_MODE_X86_64_4LVL
};
__attribute__((used, section(".limine_requests_end")))
static volatile LIMINE_REQUESTS_END_MARKER

/* Borrowed boot data, reserved for later kernel facilities. Do not reclaim or
 * reinterpret these addresses as physical pointers. The demo owns no pages. */
volatile uint64_t plum_boot_hhdm_offset;
const struct limine_memmap_response *volatile plum_boot_memory_map;

#define BOOT_FAIL(message) plum_kernel_panic((const uint8_t *)(message), sizeof(message) - 1)

_Noreturn void plum_boot_enter(void) {
    if (!LIMINE_BASE_REVISION_SUPPORTED || !LIMINE_LOADED_BASE_REV_VALID ||
        LIMINE_LOADED_BASE_REVISION != 3) {
        BOOT_FAIL("unsupported Limine base revision");
    }
    struct limine_hhdm_response *hhdm = hhdm_request.response;
    struct limine_memmap_response *map = memmap_request.response;
    struct limine_paging_mode_response *paging = paging_request.response;
    if (!hhdm || !map || !paging) BOOT_FAIL("missing required Limine response");
    if (paging->mode != LIMINE_PAGING_MODE_X86_64_4LVL) BOOT_FAIL("expected four-level paging");
    if (!map->entry_count || !map->entries) BOOT_FAIL("empty Limine memory map");
    for (uint64_t i = 0; i < map->entry_count; ++i) {
        const struct limine_memmap_entry *entry = map->entries[i];
        if (!entry || entry->length > UINT64_MAX - entry->base) BOOT_FAIL("invalid Limine memory region");
    }
    plum_boot_hhdm_offset = hhdm->offset;
    plum_boot_memory_map = map;
    /* Any early failure uses the same serial initializer; never reset COM1. */
    plum_kernel_runtime_init();
    plum_kernel_start();
}
