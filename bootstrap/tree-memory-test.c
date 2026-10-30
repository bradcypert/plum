// Account for allocations owned by the native tree implementation separately
// from Plum's RC counters. libc directory/stdio internals and stack buffers are
// not intercepted; an external peak-RSS measurement complements these totals.
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef union { struct { size_t size; } allocation; max_align_t alignment; } MemoryHeader;
static size_t live_bytes, peak_bytes, live_blocks, fail_after;
static void *tracked_malloc(size_t size) {
    if (fail_after && !--fail_after) return NULL;
    if (size > SIZE_MAX - sizeof(MemoryHeader)) return NULL;
    MemoryHeader *header = malloc(sizeof(*header) + size);
    if (!header) return NULL;
    header->allocation.size = size;
    live_bytes += size; live_blocks++;
    if (live_bytes > peak_bytes) peak_bytes = live_bytes;
    return header + 1;
}
static void tracked_free(void *pointer) {
    if (!pointer) return;
    MemoryHeader *header = (MemoryHeader *)pointer - 1;
    live_bytes -= header->allocation.size; live_blocks--;
    free(header);
}
static void *tracked_calloc(size_t count, size_t size) {
    if (count && size > SIZE_MAX / count) return NULL;
    void *pointer = tracked_malloc(count * size);
    if (pointer) memset(pointer, 0, count * size);
    return pointer;
}
static void *tracked_realloc(void *pointer, size_t size) {
    if (!pointer) return tracked_malloc(size);
    void *next = tracked_malloc(size);
    if (!next) return NULL;
    MemoryHeader *header = (MemoryHeader *)pointer - 1;
    memcpy(next, pointer, size < header->allocation.size ? size : header->allocation.size);
    tracked_free(pointer);
    return next;
}
static char *tracked_strdup(const char *text) {
    size_t size = strlen(text) + 1;
    char *copy = tracked_malloc(size);
    if (copy) memcpy(copy, text, size);
    return copy;
}
#define strdup tracked_strdup
#define malloc tracked_malloc
#define calloc tracked_calloc
#define realloc tracked_realloc
#define free tracked_free
#include "../native_stdlib/os_shim.c"
#undef strdup
#undef malloc
#undef calloc
#undef realloc
#undef free

#if !defined(_WIN32)
#include <sys/resource.h>
#endif
int main(int argc, char **argv) {
    assert(argc == 3); // paths supplied by the temporary-directory harness
    const char *source = argv[1], *destination = argv[2];
    char path[4096]; assert(snprintf(path, sizeof(path), "%s/data", source) > 0);
    for (long megabytes = 8; megabytes <= 128; megabytes *= 16) {
        FILE *file = fopen(path, "wb"); assert(file);
        assert(!fseek(file, megabytes * 1024 * 1024 - 1, SEEK_SET));
        assert(fputc(0, file) == 0 && !fclose(file));
        peak_bytes = 0;
        assert(!os_copy_tree(source, destination));
        size_t copy_peak = peak_bytes;
        assert(!live_bytes && !live_blocks && copy_peak <= 128 * 1024);
        long long root = plum_tree_root_open(destination); assert(root);
        long long entry = plum_tree_entry_open(root, "data", 0); assert(entry);
        peak_bytes = 0;
        assert(strlen(plum_tree_file_hash(entry, "data", 2)) == 64);
        assert(peak_bytes == 0); // the bounded hash transfer buffer is on stack
        plum_tree_entry_close(entry); plum_tree_entry_close(root);
        assert(!live_bytes && !live_blocks);
        printf("tree-memory: %ld MiB file, native copy peak %zu bytes, hash heap 0 bytes\n", megabytes, copy_peak);
    }
    for (int count = 1024; count <= 4096; count *= 2) {
        peak_bytes = 0;
        long long seen = plum_tree_seen_open(); assert(seen);
        for (int i = 0; i < count; i++) {
            char identity[32], logical[64];
            snprintf(identity, sizeof(identity), "id:%08d", i);
            snprintf(logical, sizeof(logical), "module/source-%08d.plum", i);
            assert(!plum_tree_seen_add(seen, identity, logical));
        }
        printf("tree-memory: %d identities, native retained %zu bytes, peak %zu bytes\n", count, live_bytes, peak_bytes);
        plum_tree_seen_close(seen);
        assert(!live_bytes && !live_blocks);
    }
    // Index growth and string-allocation failures leave a usable index and
    // unwind all allocations. Exercise both initial and resized tables.
    for (int initial = 0; initial <= 16; initial += 16) {
        for (size_t failure = 1; failure <= 4; failure++) {
            long long seen = plum_tree_seen_open(); assert(seen);
            for (int i = 0; i < initial; i++) {
                char key[32]; snprintf(key, sizeof(key), "existing:%d", i);
                assert(!plum_tree_seen_add(seen, key, key));
            }
            fail_after = failure;
            long long rc = plum_tree_seen_add(seen, "new", "logical");
            fail_after = 0;
            assert(rc == 0 || rc == -1);
            if (rc == -1) assert(!plum_tree_seen_add(seen, "new", "logical"));
            assert(plum_tree_seen_find(seen, "absent") == 0);
            assert(plum_tree_seen_find(seen, "new") == 1);
            assert(!strcmp(plum_tree_seen_previous(seen), "logical"));
            assert(plum_tree_seen_add(seen, "absent", "fresh") == 0);
            assert(plum_tree_seen_add(seen, "new", "alias") == 1);
            plum_tree_seen_close(seen);
            assert(!live_bytes && !live_blocks);
        }
    }
#if !defined(_WIN32)
    struct rusage usage; assert(!getrusage(RUSAGE_SELF, &usage));
#if defined(__APPLE__)
    printf("tree-memory: process peak RSS %ld KiB (includes libc and stack)\n", usage.ru_maxrss / 1024);
#else
    printf("tree-memory: process peak RSS %ld KiB (includes libc and stack)\n", usage.ru_maxrss);
#endif
#endif
    return 0;
}
