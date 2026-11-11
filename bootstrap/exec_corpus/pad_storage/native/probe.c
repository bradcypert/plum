#include <stdint.h>
#include <stdlib.h>
extern int64_t plum_n_alloc, plum_b_alloc;
extern size_t malloc_usable_size(void *);
static int64_t allocations, bytes, input_length;
int64_t padding_capacity(const char *s) {
    int64_t *cell = (int64_t *)(s - 16);
    if (cell[0] < 0) return 0;
    size_t size = malloc_usable_size(cell);
    return size >= 17 && size <= INT64_MAX ? (int64_t)(size - 17) : 0;
}
void padding_begin(int64_t length) { allocations = plum_n_alloc; bytes = plum_b_alloc; input_length = length; }
void padding_end(const char *s, int64_t n, int64_t mode, int64_t expected) {
    if (n != input_length + 1 || s[n] || plum_n_alloc - allocations != expected || plum_b_alloc - bytes != expected * (n + 17)) abort();
    for (int64_t i = 0; i < n; i++) if (s[i] != ((mode == 0 ? i == 0 : i == n - 1) ? '.' : 'x')) abort();
}
const char *padding_raw(int64_t which) {
    static const unsigned char text[] = {0x80, 0x81, 'a', 0xbf, 0};
    static const unsigned char fill[] = {0x80, 0x81, 0x82, 0x83, 0x84, 0};
    return (const char *)(which ? fill : text);
}
