// Replace the process shim with this translation unit in fault-injection
// builds. The real shared reader still does all I/O; only its failures are
// injected here. No test controls enter the production ABI.
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#include <io.h>
#include <fcntl.h>
#endif

extern char *plum_read_whole_file(const char *, long long *);
static int read_calls, fail_read, live_buffers;
static int malloc_calls, fail_malloc;
static void *capture_malloc(size_t n) {
    return ++malloc_calls == fail_malloc ? NULL : malloc(n);
}
static char *buffers[2];
static char paths[2][4096];
static char *capture_read(const char *path, long long *length) {
    int index = read_calls++;
    assert(index < 2);
    snprintf(paths[index], sizeof(paths[index]), "%s", path);
    if (read_calls == fail_read) { *length = 0; return NULL; }
    char *p = plum_read_whole_file(path, length);
    if (p != NULL) { buffers[index] = p; live_buffers++; }
    return p;
}
static void capture_free(void *p) {
    if (p != NULL) for (int i = 0; i < 2; i++) {
        if (p == buffers[i]) { buffers[i] = NULL; live_buffers--; }
    }
    free(p);
}
#define plum_read_whole_file capture_read
#define malloc capture_malloc
#define free capture_free
#include "../native_stdlib/process_shim.c"
#undef free
#undef malloc
#undef plum_read_whole_file

void capture_arm(long long which) {
    assert(live_buffers == 0);
    read_calls = malloc_calls = fail_malloc = 0;
    fail_read = (int)which;
    memset(paths, 0, sizeof(paths));
}
void capture_clean(void) {
    assert(live_buffers == 0);
    for (int i = 0; i < 2; i++) if (paths[i][0]) {
        FILE *f = fopen(paths[i], "rb");
        if (f != NULL) { fclose(f); assert(!"capture file leaked"); }
    }
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        assert(!plum_process_slots[i].in_use);
    }
}

#ifdef PROCESS_CAPTURE_NATIVE_TEST
int main(int argc, char **argv) {
    if (argc > 1) {
#if defined(_WIN32)
        _setmode(1, _O_BINARY);
        _setmode(2, _O_BINARY);
#endif
        fwrite("a\0b\r\n", 1, 5, stdout);
        fwrite("e\0r", 1, 3, stderr);
        return 7;
    }
    for (int iteration = 0; iteration < 20; iteration++) {
        for (int mode = 0; mode < 3; mode++) for (int fail = 0; fail <= 2; fail++) {
            capture_arm(fail);
            long long h = mode == 0 ? process_run(argv[0], "child", 1) :
                mode == 1 ? process_run_ex(argv[0], "child", 1, "", 0, "", 0, 0) :
                            process_spawn(argv[0], "child", 1, "", 0, "", 0, 0);
            assert(h >= 0);
            if (mode == 2) {
                assert(process_child_wait(h, -1) == 1);
                assert(process_poll(h) == 1 && process_child_wait(h, 1) == 1);
            }
            assert(process_exit_code(h) == 7);
            assert(process_capture_status(h) == (fail ? -1 : 0));
            if (fail) {
                assert(live_buffers == 0);
                assert(process_stdout_length(h) == 0 && process_stderr_length(h) == 0);
            } else {
                assert(process_stdout_length(h) == 5 && process_stderr_length(h) == 3);
                assert(memcmp(process_stdout_data(h), "a\0b\r\n", 5) == 0);
                assert(memcmp(process_stderr_data(h), "e\0r", 3) == 0);
            }
            assert(read_calls == (fail == 1 ? 1 : 2));
            process_free(h);
            capture_clean();
        }
        capture_arm(0);
        long long h = process_spawn(argv[0], "child", 1, "", 0, "", 0, 0);
        assert(h >= 0);
        // Drop must close/delete captures without allocating output buffers.
        char *out = strdup(plum_process_slots[h].child->out_path);
        char *err = strdup(plum_process_slots[h].child->err_path);
        process_child_free(h);
        assert(read_calls == 0);
        assert(fopen(out, "rb") == NULL && fopen(err, "rb") == NULL);
        free(out); free(err);
        capture_clean();
        // The final two shim allocations copy the capture paths. Either
        // failure must reap the launched child and delete both files.
        int allocations = malloc_calls;
        for (int fail = allocations - 1; fail <= allocations; fail++) {
            capture_arm(0);
            fail_malloc = fail;
            assert(process_spawn(argv[0], "child", 1, "", 0, "", 0, 0) == -1);
            assert(read_calls == 0);
            capture_clean();
        }
    }
    puts("process captures: exact lengths, sticky failures, no leaked buffers/files/slots, no reads on drop");
}
#endif
