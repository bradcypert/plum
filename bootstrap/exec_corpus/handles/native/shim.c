// A fake resource, so a fixture can OBSERVE cleanup rather than infer
// it. Real resources (an fd, a thread) are cleaned up invisibly; this
// one counts, and can be asked to narrate.
#include <stdio.h>

static long long next_id = 1;
static long long live = 0;
static long long closes = 0;
static int verbose = 0;

// Hands out a distinct number each time, the way `open()` does.
long long fake_open(void) {
    live++;
    return next_id++;
}

// Narration is opt-in: the deep-recursion case closes fifty thousand of
// these, and a fixture compares exact output.
void fake_close(long long h) {
    closes++;
    live--;
    if (verbose) {
        printf("  closed %lld\n", h);
        fflush(stdout);
    }
}

void fake_verbose(long long on) { verbose = (int)on; }
long long fake_live(void) { return live; }
long long fake_closes(void) { return closes; }
