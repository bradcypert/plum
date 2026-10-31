// Exercise the actual Windows loader adapter independently of Plum/cache keys.
#include <stdio.h>
#if defined(_WIN64)
#include <windows.h>
#include <stdlib.h>
// Advance a virtual clock without slowing CI: an image hash may consume
// more than the execution budget while the child is stopped. A wait timeout
// must still exhaust that same budget and clean up the child.
static HANDLE observed_child;
static BOOL WINAPI probe_create(LPCWSTR app, LPWSTR command, LPSECURITY_ATTRIBUTES process_security,
    LPSECURITY_ATTRIBUTES thread_security, BOOL inherit, DWORD flags, LPVOID env,
    LPCWSTR cwd, LPSTARTUPINFOW startup, LPPROCESS_INFORMATION process) {
    BOOL ok=CreateProcessW(app,command,process_security,thread_security,inherit,flags,env,cwd,startup,process);
    if (ok && !DuplicateHandle(GetCurrentProcess(),process->hProcess,GetCurrentProcess(),
                               &observed_child,SYNCHRONIZE,FALSE,0)) abort();
    return ok;
}
static int probe_child_reaped(void) {
    if (!observed_child) return 1;
    int done=WaitForSingleObject(observed_child,2000)==WAIT_OBJECT_0;
    CloseHandle(observed_child); observed_child=NULL;
    if (!done) fprintf(stderr,"loader probe left a running child\n");
    return done;
}
static ULONGLONG clock_offset;
static int slow_hash, wait_timeout;
static ULONGLONG WINAPI probe_tick(void) { return GetTickCount64()+clock_offset; }
static BOOL WINAPI probe_read(HANDLE file, LPVOID data, DWORD size, LPDWORD got, LPOVERLAPPED overlap) {
    BOOL ok=ReadFile(file,data,size,got,overlap);
    if (slow_hash && size==32768) { clock_offset+=11000; slow_hash=0; }
    return ok;
}
static BOOL WINAPI probe_wait(LPDEBUG_EVENT event, DWORD timeout) {
    if (wait_timeout) {
        wait_timeout=0; clock_offset+=11000;
        SetLastError(ERROR_SEM_TIMEOUT); return FALSE;
    }
    return WaitForDebugEvent(event,timeout);
}
#define CreateProcessW probe_create
#define GetTickCount64 probe_tick
#define ReadFile probe_read
#define WaitForDebugEvent probe_wait
#endif
#define PLUM_WIN_IMAGE_TRACE(path, digest) fprintf(stderr,"loader image: %ls %s\n",path,digest)
#include "../native_stdlib/os_shim.c"
#if defined(_WIN64)
#undef CreateProcessW
#undef GetTickCount64
#undef ReadFile
#undef WaitForDebugEvent
#endif

#if defined(_WIN64)
#define PROBE_TEMP_THREADS 8
#define PROBE_TEMP_COUNT 64
typedef struct {
    const char *root;
    char (*paths)[PLUM_PATH_MAX];
    int failed;
} probe_temp_job;

static DWORD WINAPI probe_temp_worker(void *arg) {
    probe_temp_job *job=arg;
    for (unsigned i=0;i<PROBE_TEMP_COUNT;i++) {
        const char *path=i%2 ? os_temp_dir() : plum_cache_temp(job->root);
        snprintf(job->paths[i],PLUM_PATH_MAX,"%s",path);
        DWORD attrs=*path ? GetFileAttributesA(path) : INVALID_FILE_ATTRIBUTES;
        if (attrs==INVALID_FILE_ATTRIBUTES || !(attrs & FILE_ATTRIBUTE_DIRECTORY)) job->failed=1;
    }
    return 0;
}

static int probe_temp_race(const char *root) {
    size_t count=PROBE_TEMP_THREADS*PROBE_TEMP_COUNT;
    char (*paths)[PLUM_PATH_MAX]=calloc(count,sizeof(*paths));
    if (!paths) return 2;
    HANDLE threads[PROBE_TEMP_THREADS]; probe_temp_job jobs[PROBE_TEMP_THREADS];
    for (unsigned i=0;i<PROBE_TEMP_THREADS;i++) {
        jobs[i]=(probe_temp_job){root,paths+i*PROBE_TEMP_COUNT,0};
        threads[i]=CreateThread(NULL,0,probe_temp_worker,&jobs[i],0,NULL);
        if (!threads[i]) ExitProcess(2);
    }
    int failed=WaitForMultipleObjects(PROBE_TEMP_THREADS,threads,TRUE,60000)!=WAIT_OBJECT_0;
    if (failed) ExitProcess(2);
    for (unsigned i=0;i<PROBE_TEMP_THREADS;i++) {
        CloseHandle(threads[i]); failed |= jobs[i].failed;
    }
    for (size_t i=0;i<count;i++) {
        for (size_t j=0;j<i;j++) if (*paths[i] && strcmp(paths[i],paths[j])==0) failed=1;
        if (*paths[i] && os_remove_tree(paths[i])!=0) failed=1;
    }
    free(paths);
    if (failed) fprintf(stderr,"concurrent temporary directories were not uniquely reserved\n");
    return failed ? 2 : 0;
}
#endif

int main(int argc, char **argv) {
#if defined(_WIN64)
    if (argc==3 && strcmp(argv[1],"--temp-race")==0) return probe_temp_race(argv[2]);
    if (argc<2) return 2;
    HANDLE file=CreateFileA(argv[1],GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_DELETE,NULL,OPEN_EXISTING,0,NULL);
    if (file!=INVALID_HANDLE_VALUE) {
        plum_win_sha provider; plum_win_sha_open(&provider);
        char fast[65], portable[65];
        int equal=plum_win_hash(file,&provider,fast) && plum_win_hash(file,NULL,portable)
            && strcmp(fast,portable)==0;
        CloseHandle(file); plum_win_sha_close(&provider);
        if (!equal) { fprintf(stderr,"Windows SHA providers disagree\n"); return 2; }
    }
    int repeat=0, inject_hash=0, inject_wait=0;
    for (int i=2;i<argc;i++) {
        if (!strcmp(argv[i],"--repeat")) repeat=1;
        else if (!strcmp(argv[i],"--slow-hash")) inject_hash=1;
        else if (!strcmp(argv[i],"--wait-timeout")) inject_wait=1;
        else return 2;
    }
    slow_hash=inject_hash; wait_timeout=inject_wait;
    char first[160];
    snprintf(first,sizeof(first),"%s",plum_cache_windows_fingerprint(argv[1]));
    if (!probe_child_reaped()) return 2;
    if (slow_hash || wait_timeout) {
        fprintf(stderr,"injected clock path was not reached\n"); return 2;
    }
    if (repeat) {
        // Warm once before counting: Windows initializes process-wide
        // debugger/crypto resources on the first probe. Repeated successful
        // and timed-out probes must not keep growing the handle count.
        DWORD before=0, after=0;
        if (!GetProcessHandleCount(GetCurrentProcess(),&before)) return 2;
        for (int i=0;i<5;i++) {
            slow_hash=inject_hash; wait_timeout=inject_wait;
            const char *next=plum_cache_windows_fingerprint(argv[1]);
            if (!probe_child_reaped() || slow_hash || wait_timeout) return 2;
            if (strcmp(first,next)!=0) {
                fprintf(stderr,"loader fingerprint was not stable: %s -> %s\n",first,next); return 2;
            }
        }
        if (!GetProcessHandleCount(GetCurrentProcess(),&after) || after>before+2) {
            fprintf(stderr,"loader probe leaked handles: %lu -> %lu\n",before,after); return 2;
        }
    }
    puts(first);
    return first[0]=='!' ? 1 : 0;
#else
    (void)argc; (void)argv;
    puts("Windows loader probe: not applicable");
    return 0;
#endif
}
