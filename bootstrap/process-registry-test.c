// Real concurrent launches must keep separate results and reclaim every slot.
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#include <windows.h>
#else
#include <time.h>
#endif
#include "../native_stdlib/process_shim.c"

#define WORKERS 12
static const char *program;
static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static int arrived, released;
static long long handles[WORKERS];
static void pause_ms(int ms) {
#if defined(_WIN32)
    Sleep(ms);
#else
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
#endif
}
static void *launch(void *arg) {
    long id = (long)(intptr_t)arg;
    char args[64];
    snprintf(args, sizeof(args), "child\t%ld", id);
    pthread_mutex_lock(&gate);
    arrived++;
    pthread_cond_broadcast(&ready);
    while (!released) pthread_cond_wait(&ready, &gate);
    pthread_mutex_unlock(&gate);
    handles[id] = id % 3 == 0 ? process_run(program, args, 2) :
        id % 3 == 1 ? process_run_ex(program, args, 2, "", 0, "", 0, 0) :
                      process_spawn(program, args, 2, "", 0, "", 0, 0);
    return NULL;
}
static void *wait_child(void *arg) {
    long long h = *(long long *)arg;
    long long result = process_child_wait(h, -1);
    assert(result == 1 || result == -1); // free may win the race
    return NULL;
}
int main(int argc, char **argv) {
    if (argc > 1) {
        pause_ms(strcmp(argv[1], "slow") == 0 ? 30000 : 100);
        if (argc > 2) { fputs(argv[2], stdout); fputs(argv[2], stderr); }
        return 7;
    }
#if defined(_WIN32)
    // Test cleanup in-process too: leaked inherited capture handles can keep
    // files alive after their owner exits, even when every slot is released.
    wchar_t base[PLUM_PROC_PATH_MAX], capture_root[PLUM_PROC_PATH_MAX];
    assert(GetTempPathW(PLUM_PROC_PATH_MAX, base));
    assert(GetTempFileNameW(base, L"reg", 0, capture_root));
    assert(DeleteFileW(capture_root) && CreateDirectoryW(capture_root, NULL));
    assert(SetEnvironmentVariableW(L"TMP", capture_root));
    assert(SetEnvironmentVariableW(L"TEMP", capture_root));
#endif
    program = argv[0];
    for (int round = 0; round < 8; round++) {
        pthread_t threads[WORKERS];
        arrived = released = 0;
        for (long i = 0; i < WORKERS; i++) assert(!pthread_create(&threads[i], NULL, launch, (void *)(intptr_t)i));
        pthread_mutex_lock(&gate);
        while (arrived < WORKERS) pthread_cond_wait(&ready, &gate);
        released = 1;
        pthread_cond_broadcast(&ready);
        pthread_mutex_unlock(&gate);
        for (int i = 0; i < WORKERS; i++) pthread_join(threads[i], NULL);
        for (int i = 0; i < WORKERS; i++) {
            assert(handles[i] >= 0);
            for (int j = 0; j < i; j++) assert(handles[i] != handles[j]);
            if (i % 3 == 2) assert(process_child_wait(handles[i], -1) == 1);
            char expected[16];
            snprintf(expected, sizeof(expected), "%d", i);
            assert(process_exit_code(handles[i]) == 7);
            assert(strcmp(process_stdout_data(handles[i]), expected) == 0);
            assert(strcmp(process_stderr_data(handles[i]), expected) == 0);
            process_free(handles[i]);
        }
    }
    // A waiting task must not block another task killing/dropping that child,
    // or launching an unrelated process. Concurrent waiters collect once.
    for (int drop = 0; drop < 2; drop++) {
        long long h = process_spawn(program, "slow", 1, "", 0, "", 0, 0);
        assert(h >= 0);
        pthread_t a, b;
        assert(!pthread_create(&a, NULL, wait_child, &h));
        assert(!pthread_create(&b, NULL, wait_child, &h));
        pause_ms(30);
        long long other = process_run_ex(program, "child\tok", 2, "", 0, "", 0, 0);
        assert(other >= 0 && strcmp(process_stdout_data(other), "ok") == 0);
        process_free(other);
        if (drop) process_child_free(h);
        else assert(process_terminate(h, 1) == 0);
        pthread_join(a, NULL); pthread_join(b, NULL);
        process_free(h);
    }
    // Failed launches cannot consume capacity. Reservations also keep an
    // in-flight launch private, and a full table fails before launching.
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS + 2; i++) {
        assert(process_spawn("/plum194-no-such-program", "", 0, "", 0, "", 0, 0) == -1);
    }
    long long reserved[PLUM_PROC_MAX_SLOTS];
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        reserved[i] = plum_process_reserve();
        assert(reserved[i] >= 0);
        pthread_mutex_unlock(&plum_process_slots[reserved[i]].lock);
    }
    assert(process_run(program, "child", 1) == -1);
    assert(process_run_ex(program, "child", 1, "", 0, "", 0, 0) == -1);
    assert(process_spawn(program, "child", 1, "", 0, "", 0, 0) == -1);
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        plum_process_available(&plum_process_slots[reserved[i]]);
    }
    // Stale identities cannot read or free a replacement, and storage stays bounded.
    long long old = process_run_ex(program, "child\told", 2, "", 0, "", 0, 0);
    process_free(old);
    long long next = process_run_ex(program, "child\tnew", 2, "", 0, "", 0, 0);
    assert(next >= 0 && next != old);
    process_free(old); process_child_free(old);
    assert(process_exit_code(old) == -1 && process_poll(old) == -1);
    assert(strcmp(process_stdout_data(next), "new") == 0);
    process_free(next);
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        PlumProcessSlot *slot = &plum_process_slots[i];
        assert(!slot->in_use && !slot->reserved);
        assert(slot->child == NULL && slot->out_data == NULL && slot->err_data == NULL);
    }
    // Generation exhaustion retires a slot instead of wrapping.
    plum_process_slots[0].identity = LLONG_MAX;
    long long index = plum_process_reserve();
    assert(index > 0 && plum_process_slots[0].reserved);
    plum_process_available(&plum_process_slots[index]);
    pthread_mutex_unlock(&plum_process_slots[index].lock);
#if defined(_WIN32)
    assert(RemoveDirectoryW(capture_root));
#endif
    puts("process registry: concurrent launches, waits, terminate/drop and stale identities: ok");
}
