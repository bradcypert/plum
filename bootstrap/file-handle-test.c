// Include the adapter to exercise otherwise unreachable identity exhaustion.
#include "../native_stdlib/file_shim.c"
#include <assert.h>

static const char *test_path;

static void assert_stale(long long stale) {
    assert(file_close_checked(stale) == 0);
    file_close(stale);
    assert(file_read_n(stale, 1) == -1);
    assert(file_write(stale, "bad", 3) == -1);
    assert(file_seek(stale, 0, 0) == -1);
    assert(file_flush(stale) == -1);
}

static void reuse_and_capacity(void) {
    assert(file_open(test_path, 3) == -1);
    assert(file_open("", 0) == -1);
    long long old = file_open(test_path, 1);
    assert(old > 0);
    assert(file_close_checked(old) == 0);
    long long next = file_open(test_path, 1);
    assert(next > 0 && next != old);
    assert((next - 1) % PLUM_MAX_FILES == (old - 1) % PLUM_MAX_FILES);
    assert_stale(old);
    assert(file_write(next, "abc", 3) == 3);
    assert(file_flush(next) == 0);
    assert(file_close_checked(next) == 0);
    for (int i = 0; i < 1024; i++) {
        long long h = file_open(test_path, 0);
        assert(h > 0 && h != next && h != old);
        assert_stale(old);
        assert(file_read_n(h, 3) == 3);
        assert(memcmp(file_read_data(), "abc", 3) == 0);
        assert(file_close_checked(h) == 0);
    }
    long long live[PLUM_MAX_FILES];
    for (int i = 0; i < PLUM_MAX_FILES; i++) {
        live[i] = file_open(test_path, 0);
        assert(live[i] > 0);
    }
    assert(file_open(test_path, 0) == -1);
    for (int i = 0; i < PLUM_MAX_FILES; i++) assert(file_close_checked(live[i]) == 0);
    assert_stale(0);
    assert_stale(-1);
    assert_stale(LLONG_MAX);
}

// Concurrent first use tests once-only initialization and allocation.
// Every worker uses an independent stream, but shares the registry.
static void *worker(void *unused) {
    (void)unused;
    for (int i = 0; i < 500; i++) {
        long long h = file_open(test_path, 0);
        assert(h > 0);
        assert(file_read_n(h, 1) == 1);
        assert(file_seek(h, 0, 0) == 0);
        assert(file_flush(h) == 0);
        assert(file_close_checked(h) == 0);
        assert_stale(h);
    }
    free(read_buf); // Test teardown: production TLS buffer lifetime is unchanged.
    read_buf = NULL;
    read_cap = 0;
    return NULL;
}

typedef struct { long long h; int writing; } Race;

static void *race_io(void *raw) {
    Race *race = raw;
    long long h = race->h;
    for (int i = 0; i < 1000; i++) {
        long long n;
        if (race->writing) {
            n = file_write(h, "x", 1);
            assert(n == 1 || n == -1);
        } else {
            n = file_read_n(h, 1);
            assert(n == 1 || n == 0 || n == -1);
        }
        n = file_seek(h, 0, 0);
        assert(n == 0 || n == -1);
        n = file_flush(h);
        assert(n == 0 || n == -1);
    }
    assert(file_close_checked(h) == 0);
    free(read_buf);
    read_buf = NULL;
    read_cap = 0;
    return NULL;
}

static void close_during_io(int writing) {
    // tmpfile is read/write, unlike the public single-direction open modes.
    PlumFileSlot *slot = &slots[0];
    slot->identity += PLUM_MAX_FILES;
    slot->reserved = 1;
    slot->stream = tmpfile();
    assert(slot->stream);
    long long h = slot->identity;
    Race race = {h, writing};
    pthread_t threads[4];
    for (int i = 0; i < 4; i++) assert(pthread_create(&threads[i], NULL, race_io, &race) == 0);
    assert(file_close_checked(h) == 0);
    // Reuse immediately while operations on the old identity are racing.
    long long replacement = file_open(test_path, 0);
    assert(replacement > 0 && replacement != h);
    for (int i = 0; i < 4; i++) assert(pthread_join(threads[i], NULL) == 0);
    assert(file_read_n(replacement, 3) == 3);
    assert(memcmp(file_read_data(), "abc", 3) == 0);
    assert(file_close_checked(replacement) == 0);
}

static void independent_allocation(void) {
    long long a = file_open(test_path, 0);
    assert(a > 0);
    PlumFileSlot *busy = slot_lock(a);
    assert(busy);
    // Model blocked I/O: allocating another stream must not wait on it.
    long long b = file_open(test_path, 0);
    assert(b > 0 && b != a);
    pthread_mutex_unlock(&busy->lock);
    assert(file_close_checked(a) == 0);
    assert(file_close_checked(b) == 0);
}

static void exhaustion(void) {
    // Highest encodable generation for slot 0: never wrap to handle 1.
    long long last = LLONG_MAX - ((LLONG_MAX - 1) % PLUM_MAX_FILES);
    slots[0].identity = last - PLUM_MAX_FILES;
    long long final = file_open(test_path, 0);
    assert(final == last);
    assert(file_close_checked(final) == 0);
    long long h = file_open(test_path, 0);
    assert(h > 0 && (h - 1) % PLUM_MAX_FILES != 0);
    assert_stale(1);
    assert(file_read_n(h, 1) == 1);
    assert(file_close_checked(h) == 0);
    // Retiring all slots fails cleanly rather than opening an untracked FILE.
    for (int i = 0; i < PLUM_MAX_FILES; i++) slots[i].identity = LLONG_MAX;
    assert(file_open(test_path, 0) == -1);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    test_path = argv[1];
    FILE *seed = fopen(test_path, "wb");
    assert(seed && fwrite("abc", 1, 3, seed) == 3 && fclose(seed) == 0);
    pthread_t threads[8];
    for (int i = 0; i < 8; i++) assert(pthread_create(&threads[i], NULL, worker, NULL) == 0);
    for (int i = 0; i < 8; i++) assert(pthread_join(threads[i], NULL) == 0);
    reuse_and_capacity();
    close_during_io(0);
    close_during_io(1);
    independent_allocation();
    exhaustion();
    free(read_buf);
    assert(remove(test_path) == 0);
    printf("native file handles: ok (slot size %zu, table %zu bytes)\n", sizeof(PlumFileSlot), sizeof(slots));
    return 0;
}
