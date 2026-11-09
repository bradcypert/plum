/* Allocation-free observation; the input stays live across both snapshots. */
#include <stdint.h>
#include <stdio.h>
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t, int64_t);
static int64_t before[6];
void parse_before(void) {
    int64_t token = plum_heap_snapshot();
    for (int i = 0; i < 6; ++i) before[i] = plum_heap_snapshot_field(token, i);
}
void parse_after(void) {
    int64_t token = plum_heap_snapshot(), after[6];
    for (int i = 0; i < 6; ++i) after[i] = plum_heap_snapshot_field(token, i);
    printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"live_bytes\":%lld,"
           "\"peak_bytes\":%lld,\"previous_peak_bytes\":%lld,\"previous_live_bytes\":%lld}\n",
           (long long)(after[0] - before[0]), (long long)(after[1] - before[1]),
           (long long)(after[3] - before[3]), (long long)after[5], (long long)before[5], (long long)before[3]);
}
