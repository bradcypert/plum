// A thin ABI-adapter shim for running a child process and capturing
// its exit code + stdout + stderr — needed for `plum build` to shell
// out to `clang` (already how THIS compiler's own native codegen
// works, see `codegen_cli.rs`'s `clang_compile`) from a Plum program
// itself, the other hard blocker (alongside `dir_shim.c`) toward ever
// self-hosting. Same `net_shim.c`/`dir_shim.c` shim pattern (see their
// own doc comments) — Plum's extern surface has no raw pointers, no
// arrays, and no multi-value return, so a "run a process, get back
// THREE things (exit code/stdout/stderr)" operation needs the same
// two adaptations those files already established:
//
// **A handle-based, multi-call return** (mirrors `net_shim.c`'s socket
// fds, `dir_shim.c`'s directory handles): `process_run` blocks until
// the child exits, captures everything, and returns an opaque `Int`
// handle; `process_exit_code`/`process_stdout`/`process_stderr` read
// the ALREADY-CAPTURED result back out, any number of times, until
// `process_free` releases it. No result is ever recomputed or re-read
// from the child (which has already exited by the time `process_run`
// returns) — these are just accessors into a small in-process table.
//
// **Arguments packed into a single delimited `CStr`** (extern has no
// `Array[String]` at all): each argument separated by a TAB character.
// The natural choice here would be a rarer control byte (e.g. ASCII
// Unit Separator, `0x1F`) — genuinely safer against a real argument
// containing it — but Plum's own string-literal lexer only supports
// `\n`/`\t`/`\r`/`\\`/`\"` escapes, no `\xNN` hex byte escape, so the
// PLUM-side wrapper that joins an `Array[String]` into this shim's
// input literally cannot produce anything else. A real, honest trade
// forced by that limitation, not the ideal choice — a tab character
// appearing inside a real file path or flag is still rare, just not
// AS rare as a genuinely obscure control byte would have been. An
// argument that DOES contain a literal tab will be mis-split — not
// handled, not worth more complexity for something this unlikely.
//
// **Captures stdout/stderr via TEMP FILES, not pipes — deliberately.**
// A naive `pipe()` + `fork()` + `waitpid()` implementation has a
// classic, well-known deadlock: if the child writes more than the
// pipe's kernel buffer (~64KB on Linux) to a pipe NOBODY is draining
// yet (because the parent is blocked in `waitpid` first), the child
// blocks on `write()`, the parent blocks on `waitpid()` waiting for a
// child that's now blocked on the parent — a real hang, not a
// hypothetical (draining pipes concurrently, e.g. via `select()`/
// `poll()`, avoids it but adds real complexity this shim doesn't need
// to take on). Redirecting the child's stdout/stderr to real temp
// files via `dup2` sidesteps the whole class of bug: a file has no
// fixed-size buffer to fill, so there is no writer/reader ordering
// dependency at all — the parent just reads both files back, in full,
// AFTER `waitpid` confirms the child is done.
//
// Allocation reserves a slot before launching. Per-slot locks protect publication,
// collection and cleanup; unrelated children never share an operation lock.
// Positive generation-tagged handles prevent stale values from accessing reused
// storage. The table and synchronization metadata stay bounded across launches.
// Data accessors return borrowed immutable capture buffers: native callers must
// keep the owning handle alive until they finish copying. Plum's wrappers retain
// that owner while reading; freeing a raw handle ends its borrowed pointers.
//
// **Windows support is written but UNVERIFIED.** Everything above
// describes the POSIX path, which is tested by every harness in this
// repo. The Windows path below has never been compiled by a Windows
// toolchain, let alone run; it is a starting point, not working code,
// and is marked as such in PORTING.md. It keeps the temp-file capture
// strategy verbatim, because the pipe-deadlock reasoning above is not
// platform-specific -- Windows anonymous pipes have a fixed buffer and
// the identical failure mode.
//
// The two paths are split behind ONE function, `plum_spawn_capture`.
// Slot bookkeeping, argument splitting, checked capture reads and
// cleanup are shared by both platforms.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <limits.h>
#include <wchar.h>
#include <pthread.h>
#include "path_shim.h"

#if defined(_WIN32)
#include <windows.h>
#else
#include <unistd.h>
#include <sys/wait.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <time.h>
#endif

#define PLUM_PROC_PATH_MAX 4096

// Cleanup must also work when allocation just failed. Stored capture paths
// are bounded, so decode into stack storage rather than allocating again.
static int plum_capture_remove(const char *path) {
#if defined(_WIN32)
    wchar_t wide[PLUM_PROC_PATH_MAX];
    if (!MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, PLUM_PROC_PATH_MAX)) return -1;
    return DeleteFileW(wide) ? 0 : -1;
#else
    return remove(path);
#endif
}

#define PLUM_PROC_ARG_SEP '\t'
#define PLUM_PROC_MAX_SLOTS 256

// The extra state an ASYNCHRONOUS child needs, and a synchronous run
// does not have to carry. NULL on a slot filled by `process_run`.
//
// A running child is the one case where the captured output cannot be
// read yet, so the PATHS have to outlive the spawn call -- which is
// the whole difference between this and the blocking path. Everything
// else here is what has to be closed or deleted once the child is
// gone, held so that `process_child_free` can do it without asking
// the caller to.
typedef struct {
    long long pid;       // POSIX: pid_t. Windows: (intptr_t)HANDLE.
    void *h_out;         // Windows only: the child's stdout file handle.
    void *h_err;         // Windows only: the child's stderr file handle.
    char *out_path;
    char *err_path;
    char *in_path;       // Windows only; POSIX unlinks it at spawn.
    int finished;        // exit status collected, output read back
    // The policy `process_child_free` applies to a child still running.
    // 0 kills and reaps it. There is no way to set this from Plum yet
    // and that is deliberate -- it exists so that offering one later is
    // an addition rather than a redesign. See DESIGN.md.
    int drop_policy;
} PlumChildState;

typedef struct {
    pthread_mutex_t lock;
    long long identity;
    int reserved; // allocator lock, independent of the operation lock
    int in_use;
    long long exit_code;
    char *out_data;
    char *err_data;
    long long out_length;
    long long err_length;
    int capture_failed;
    PlumChildState *child;
} PlumProcessSlot;

static PlumProcessSlot plum_process_slots[PLUM_PROC_MAX_SLOTS];

static pthread_mutex_t plum_process_allocator = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t plum_process_once = PTHREAD_ONCE_INIT;
static int plum_process_ready;

static void plum_process_init(void) {
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        if (pthread_mutex_init(&plum_process_slots[i].lock, NULL) != 0) {
            while (i > 0) pthread_mutex_destroy(&plum_process_slots[--i].lock);
            return;
        }
    }
    plum_process_ready = 1;
}

// Never hold the allocator lock while taking a slot lock or performing I/O.
static void plum_process_available(PlumProcessSlot *slot) {
    pthread_mutex_lock(&plum_process_allocator);
    slot->reserved = 0;
    pthread_mutex_unlock(&plum_process_allocator);
}

// Returns a reserved, locked index. Every failed launch must release it.
static long long plum_process_reserve(void) {
    pthread_once(&plum_process_once, plum_process_init);
    if (!plum_process_ready) return -1;
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        PlumProcessSlot *slot = &plum_process_slots[i];
        pthread_mutex_lock(&plum_process_allocator);
        int available = !slot->reserved;
        if (available) slot->reserved = 1;
        pthread_mutex_unlock(&plum_process_allocator);
        if (!available) continue;
        pthread_mutex_lock(&slot->lock);
        if (slot->identity > LLONG_MAX - PLUM_PROC_MAX_SLOTS) {
            // Retire instead of wrapping to an identity a stale caller holds.
            pthread_mutex_unlock(&slot->lock);
            continue;
        }
        slot->identity = slot->identity == 0 ? i + 1 : slot->identity + PLUM_PROC_MAX_SLOTS;
        return i;
    }
    return -1;
}

static PlumProcessSlot *plum_process_lock(long long handle) {
    if (handle <= 0) return NULL;
    pthread_once(&plum_process_once, plum_process_init);
    if (!plum_process_ready) return NULL;
    PlumProcessSlot *slot = &plum_process_slots[(handle - 1) % PLUM_PROC_MAX_SLOTS];
    pthread_mutex_lock(&slot->lock);
    if (!slot->in_use || slot->identity != handle) {
        pthread_mutex_unlock(&slot->lock);
        return NULL;
    }
    return slot;
}

// Defined further down with the asynchronous children. Declared here
// because `process_free` has to hand a spawned slot over to it, and
// that appears first.
static void process_child_free_locked(long long handle);
static long long process_terminate_locked(long long handle, long long hard);

static char *plum_strdup_or_null(const char *s) {
    if (s == NULL) return NULL;
    size_t n = strlen(s) + 1;
    char *p = malloc(n);
    if (p != NULL) memcpy(p, s, n);
    return p;
}

// Shared checked reader: 64-bit positioning, checked allocation/read/close,
// and explicit length. NULL is an error, never a successful empty capture.
extern char *plum_read_whole_file(const char *path, long long *length);

// Publish both streams atomically. On failure retain only the error state;
// no successful prefix or sibling buffer should linger until Child is dropped.
static void plum_process_capture(PlumProcessSlot *slot, const char *out, const char *err) {
    slot->out_data = plum_read_whole_file(out, &slot->out_length);
    slot->err_data = slot->out_data == NULL ? NULL :
        plum_read_whole_file(err, &slot->err_length);
    slot->capture_failed = slot->out_data == NULL || slot->err_data == NULL;
    if (slot->capture_failed) {
        free(slot->out_data);
        free(slot->err_data);
        slot->out_data = slot->err_data = NULL;
        slot->out_length = slot->err_length = 0;
    }
}

// A sentinel distinct from every real exit code. It cannot be -1:
// `process_run` already uses -1 as the exit code of a child killed by a
// signal, which is a SUCCESSFUL run with an unusual outcome, not a
// failure to start anything.
#define PLUM_SPAWN_FAILED (-1000000000LL)

#if defined(_WIN32)
// Call only after the process handle is signaled. DWORD is unsigned;
// widening directly preserves all Windows exit values, including 259
// (STILL_ACTIVE is also a valid exit value for a process already done).
// Keep query failure outside that domain rather than inventing a status.
static long long plum_win_exit_code(HANDLE process) {
    DWORD code;
    if (!GetExitCodeProcess(process, &code)) return PLUM_SPAWN_FAILED;
    return (long long)code;
}
#endif

// `stdin_mode`: -1 inherit the parent's stdin, 0 empty (`/dev/null` /
// `NUL`), 1 feed `stdin_data` of length `stdin_len`.
// `fail_on_exec`: when set, an `execvp` failure is `PLUM_SPAWN_FAILED`
// rather than a child that exited 127 -- so `Process.run` can tell
// "never started" from "started and the program itself returned 127".

static char **plum_split_args(const char *program, const char *args_joined, long long argc, char **copy_out) {
    char *args_copy = strdup(args_joined != NULL ? args_joined : "");
    char **argv = (char **)malloc(sizeof(char *) * (size_t)(argc + 2));
    if (args_copy == NULL || argv == NULL) {
        free(args_copy);
        free(argv);
        *copy_out = NULL;
        return NULL;
    }
    argv[0] = (char *)program;
    long long idx = 1;
    if (argc > 0) {
        argv[idx++] = args_copy;
        for (char *c = args_copy; *c != '\0'; c++) {
            if (*c == PLUM_PROC_ARG_SEP) {
                *c = '\0';
                if (idx <= argc) {
                    argv[idx++] = c + 1;
                }
            }
        }
    }
    argv[idx] = NULL;
    *copy_out = args_copy;
    return argv;
}

static char **plum_split_joined(const char *joined, long long n, char **copy_out) {
    if (n <= 0) {
        *copy_out = NULL;
        return NULL;
    }
    char *copy = strdup(joined != NULL ? joined : "");
    char **items = (char **)malloc(sizeof(char *) * (size_t)(n + 1));
    if (copy == NULL || items == NULL) {
        free(copy);
        free(items);
        *copy_out = NULL;
        return NULL;
    }
    long long idx = 0;
    items[idx++] = copy;
    for (char *c = copy; *c != '\0'; c++) {
        if (*c == PLUM_PROC_ARG_SEP) {
            *c = '\0';
            if (idx < n) {
                items[idx++] = c + 1;
            }
        }
    }
    items[idx] = NULL;
    *copy_out = copy;
    return items;
}

#if !defined(_WIN32)

static void plum_apply_env(char **envp) {
    if (envp == NULL) return;
    for (int i = 0; envp[i] != NULL; i++) {
        char *eq = strchr(envp[i], '=');
        if (eq == NULL) continue;
        *eq = '\0';
        setenv(envp[i], eq + 1, 1);
        *eq = '=';
    }
}

// Runs `argv` to completion with stdout and stderr captured into two
// fresh temp files, whose paths are written into the caller's buffers.
// Returns the child's exit code, or `PLUM_SPAWN_FAILED` if the child
// could never be started -- in which case no temp files are left behind
// and the path buffers hold nothing worth reading.
//
// This is the ONLY platform-specific part of this shim. Everything
// around it is shared.

static long long plum_spawn_capture(const char *program, char **argv, char *out_path, char *err_path,
                                    char **envp, const char *stdin_data, long long stdin_len,
                                    int stdin_mode, int fail_on_exec, PlumChildState *async) {
    const char *tmp = getenv("TMPDIR");
    if (tmp == NULL || tmp[0] == '\0') tmp = "/tmp";
    if (snprintf(out_path, PLUM_PROC_PATH_MAX, "%s/plum_proc_out_XXXXXX", tmp) >= PLUM_PROC_PATH_MAX ||
        snprintf(err_path, PLUM_PROC_PATH_MAX, "%s/plum_proc_err_XXXXXX", tmp) >= PLUM_PROC_PATH_MAX) {
        return PLUM_SPAWN_FAILED;
    }

    int out_fd = mkstemp(out_path);
    int err_fd = (out_fd >= 0) ? mkstemp(err_path) : -1;
    if (out_fd < 0 || err_fd < 0) {
        if (out_fd >= 0) {
            close(out_fd);
            unlink(out_path);
        }
        return PLUM_SPAWN_FAILED;
    }

    int in_fd = -1;
    char in_path[PLUM_PROC_PATH_MAX];
    in_path[0] = '\0';
    if (stdin_mode == 0) {
        in_fd = open("/dev/null", O_RDONLY);
        if (in_fd < 0) {
            close(out_fd); close(err_fd);
            unlink(out_path); unlink(err_path);
            return PLUM_SPAWN_FAILED;
        }
    } else if (stdin_mode > 0) {
        if (snprintf(in_path, PLUM_PROC_PATH_MAX, "%s/plum_proc_in_XXXXXX", tmp) >= PLUM_PROC_PATH_MAX) {
            close(out_fd); close(err_fd);
            unlink(out_path); unlink(err_path);
            return PLUM_SPAWN_FAILED;
        }
        in_fd = mkstemp(in_path);
        if (in_fd < 0) {
            close(out_fd); close(err_fd);
            unlink(out_path); unlink(err_path);
            return PLUM_SPAWN_FAILED;
        }
        long long left = stdin_len;
        const char *p = stdin_data != NULL ? stdin_data : "";
        while (left > 0) {
            ssize_t w = write(in_fd, p, (size_t)left);
            if (w <= 0) {
                close(in_fd); close(out_fd); close(err_fd);
                unlink(in_path); unlink(out_path); unlink(err_path);
                return PLUM_SPAWN_FAILED;
            }
            p += w;
            left -= w;
        }
        if (lseek(in_fd, 0, SEEK_SET) < 0) {
            close(in_fd); close(out_fd); close(err_fd);
            unlink(in_path); unlink(out_path); unlink(err_path);
            return PLUM_SPAWN_FAILED;
        }
    }

    int errpipe[2] = {-1, -1};
    if (fail_on_exec) {
        if (pipe(errpipe) != 0) {
            if (in_fd >= 0) close(in_fd);
            if (in_path[0] != '\0') unlink(in_path);
            close(out_fd); close(err_fd);
            unlink(out_path); unlink(err_path);
            return PLUM_SPAWN_FAILED;
        }
        fcntl(errpipe[1], F_SETFD, FD_CLOEXEC);
    }

    pid_t pid = fork();
    if (pid < 0) {
        if (in_fd >= 0) close(in_fd);
        if (in_path[0] != '\0') unlink(in_path);
        if (errpipe[0] >= 0) { close(errpipe[0]); close(errpipe[1]); }
        close(out_fd);
        close(err_fd);
        unlink(out_path);
        unlink(err_path);
        return PLUM_SPAWN_FAILED;
    }
    if (pid == 0) {
        if (in_fd >= 0) {
            dup2(in_fd, STDIN_FILENO);
            close(in_fd);
        }
        dup2(out_fd, STDOUT_FILENO);
        dup2(err_fd, STDERR_FILENO);
        close(out_fd);
        close(err_fd);
        if (fail_on_exec) close(errpipe[0]);
        plum_apply_env(envp);
        execvp(program, argv);
        if (fail_on_exec) {
            int e = errno;
            (void)write(errpipe[1], &e, sizeof(e));
        }
        _exit(127); // only reached if execvp itself failed
    }

    if (in_fd >= 0) close(in_fd);
    if (in_path[0] != '\0') unlink(in_path);
    close(out_fd);
    close(err_fd);
    if (fail_on_exec) {
        close(errpipe[1]);
        int exec_err = 0;
        ssize_t n = read(errpipe[0], &exec_err, sizeof(exec_err));
        close(errpipe[0]);
        if (n == (ssize_t)sizeof(exec_err)) {
            int status = 0;
            waitpid(pid, &status, 0);
            unlink(out_path);
            unlink(err_path);
            return PLUM_SPAWN_FAILED;
        }
    }
    // Asynchronous: the child is running, and the caller collects its
    // status later through `process_poll`. Everything above still ran,
    // including the exec-failure handshake -- a `spawn` that could not
    // start the program must fail HERE, not at the first poll, or the
    // error surfaces detached from the call that caused it.
    if (async != NULL) {
        async->pid = (long long)pid;
        return 0;
    }
    int status = 0;
    waitpid(pid, &status, 0);
    return WIFEXITED(status) ? (long long)WEXITSTATUS(status) : -1;
}

#else

// Windows has no `argv`. `CreateProcess` takes ONE command-line string
// and the child pulls its arguments back out, so every argument has to
// be quoted here in a way the child's own parser will undo. Getting
// this wrong is not cosmetic: a Windows path routinely contains a
// space (`C:\Users\Some Name\...`), and an unquoted one silently
// becomes two arguments.
//
// The rule below is the documented, `CommandLineToArgvW`-compatible
// one, and the backslash handling is the part that looks wrong and is
// not: a backslash is only an escape when it PRECEDES a quote, so runs
// of backslashes are doubled in that position and left alone
// everywhere else. `\` at the end of an argument is such a position,
// because the closing quote follows it.
//
// `_spawnvp` would have been shorter and is not used: the CRT joins
// `argv` with plain spaces and adds no quoting of its own, so it has
// this identical problem plus a layer of indirection over it.
static int plum_win_needs_quotes(const char *s) {
    if (*s == '\0') return 1; // an empty argument must survive as one
    for (const char *c = s; *c != '\0'; c++) {
        if (*c == ' ' || *c == '\t' || *c == '"') return 1;
    }
    return 0;
}

// Appends one quoted argument. Returns 0 on success, -1 if it would not
// fit -- never a truncated command line, which would run something
// other than what was asked for.
static int plum_win_append_arg(char *buf, size_t cap, size_t *len, const char *arg) {
    size_t n = *len;
    size_t backslashes;
    const char *c;

#define PLUM_PUT(ch) do { if (n + 1 >= cap) return -1; buf[n++] = (ch); } while (0)

    if (n > 0) PLUM_PUT(' ');

    if (!plum_win_needs_quotes(arg)) {
        for (c = arg; *c != '\0'; c++) PLUM_PUT(*c);
        *len = n;
        return 0;
    }

    PLUM_PUT('"');
    for (c = arg; *c != '\0'; c++) {
        backslashes = 0;
        while (*c == '\\') { backslashes++; c++; }
        if (*c == '\0') {
            // Trailing backslashes precede the closing quote, so they
            // must be doubled or they would escape it.
            for (size_t i = 0; i < backslashes * 2; i++) PLUM_PUT('\\');
            break;
        }
        if (*c == '"') {
            for (size_t i = 0; i < backslashes * 2 + 1; i++) PLUM_PUT('\\');
            PLUM_PUT('"');
        } else {
            for (size_t i = 0; i < backslashes; i++) PLUM_PUT('\\');
            PLUM_PUT(*c);
        }
    }
    PLUM_PUT('"');

#undef PLUM_PUT

    *len = n;
    return 0;
}

// Every launch uses an explicit stdio allowlist. The source handles remain
// non-inheritable; only these short-lived duplicates can enter this child.
// No global spawn lock is needed, and other tasks' captures cannot leak into
// unrelated children and prevent their deletion after their owner exits.
static BOOL plum_win_create_process(wchar_t *cmdline, wchar_t *env_block,
                                    HANDLE input, HANDLE output, HANDLE error,
                                    PROCESS_INFORMATION *process) {
    STARTUPINFOEXW si;
    ZeroMemory(&si, sizeof(si));
    si.StartupInfo.cb = sizeof(si);
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    HANDLE sources[3] = { input, output, error };
    HANDLE inherited[3] = { NULL, NULL, NULL };
    HANDLE *stdio[3] = { &si.StartupInfo.hStdInput, &si.StartupInfo.hStdOutput,
                         &si.StartupInfo.hStdError };
    SIZE_T count = 0, bytes = 0;
    int initialized = 0;
    BOOL result = FALSE;
    for (int i = 0; i < 3; i++) {
        if (sources[i] == NULL || sources[i] == INVALID_HANDLE_VALUE) continue;
        if (!DuplicateHandle(GetCurrentProcess(), sources[i], GetCurrentProcess(),
            &inherited[count], 0, TRUE, DUPLICATE_SAME_ACCESS)) goto done;
        *stdio[i] = inherited[count++];
    }
    if (count > 0) {
        InitializeProcThreadAttributeList(NULL, 1, 0, &bytes);
        if (!bytes) goto done;
        si.lpAttributeList = malloc(bytes);
        if (si.lpAttributeList == NULL) goto done;
        if (!InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &bytes)) goto done;
        initialized = 1;
        if (!UpdateProcThreadAttribute(si.lpAttributeList, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherited, count * sizeof(HANDLE), NULL, NULL)) goto done;
    }
    ZeroMemory(process, sizeof(*process));
    result = CreateProcessW(NULL, cmdline, NULL, NULL, count > 0,
        CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
        env_block, NULL, &si.StartupInfo, process);
done:
    if (initialized) DeleteProcThreadAttributeList(si.lpAttributeList);
    free(si.lpAttributeList);
    for (SIZE_T i = 0; i < count; i++) CloseHandle(inherited[i]);
    return result;
}

// Keep capture paths in UTF-8 too: the shared whole-file reader and
// cleanup routines use the same filesystem boundary as public Os APIs.
static HANDLE plum_win_temp_handle(const char *prefix, char *path_out) {
    wchar_t base[PLUM_PROC_PATH_MAX], path[PLUM_PROC_PATH_MAX];
    wchar_t wp[4] = {0};
    for (size_t i = 0; i < 3 && prefix[i]; i++) wp[i] = (unsigned char)prefix[i];
    DWORD n = GetTempPathW(PLUM_PROC_PATH_MAX, base);
    if (!n || n >= PLUM_PROC_PATH_MAX || !GetTempFileNameW(base, wp, 0, path))
        return INVALID_HANDLE_VALUE;
    if (!plum_path_utf8(path, path_out, PLUM_PROC_PATH_MAX)) {
        DeleteFileW(path); return INVALID_HANDLE_VALUE;
    }
    SECURITY_ATTRIBUTES sa = {sizeof(sa), NULL, FALSE};
    HANDLE h = CreateFileW(path, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                          &sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) { DeleteFileW(path); path_out[0] = 0; }
    return h;
}

static wchar_t *plum_win_utf16(const char *text, int max_units) {
    return plum_path_wide_limit(text, max_units);
}
// Quoting only examines ASCII syntax, so it is safe to quote UTF-8
// before converting. Both spawn paths use this one command-line builder.
static wchar_t *plum_win_command_line(char **argv) {
    // A valid command line has at most 32766 UTF-16 units. Each unit
    // takes at most three UTF-8 bytes, so even rejected input needs no
    // more than this bounded quoting buffer.
    const size_t max_bytes = 3 * 32766 + 1;
    size_t cap = 1, len = 0;
    for (size_t i = 0; argv[i] != NULL; i++) {
        size_t n = strlen(argv[i]);
        if (n >= max_bytes) return NULL;
        size_t growth = n * 2 + 3;
        cap = growth > max_bytes - cap ? max_bytes : cap + growth;
    }
    char *quoted = malloc(cap);
    if (quoted == NULL) return NULL;
    for (size_t i = 0; argv[i] != NULL; i++) {
        if (plum_win_append_arg(quoted, cap, &len, argv[i]) != 0) {
            free(quoted);
            return NULL;
        }
    }
    quoted[len] = '\0';
    // Count UTF-16 units before allocating the wide command line.
    wchar_t *wide = plum_win_utf16(quoted, 32767);
    free(quoted);
    return wide;
}

typedef struct {
    wchar_t *text;
    int key_len;
    size_t order; // later entries win, including duplicate overlay keys
    int owned;   // parent entries borrow the GetEnvironmentStringsW snapshot
} PlumWinEnvEntry;

static int plum_win_env_key_compare(const PlumWinEnvEntry *a, const PlumWinEnvEntry *b) {
    return CompareStringOrdinal(a->text, a->key_len, b->text, b->key_len, TRUE) - CSTR_EQUAL;
}

static int plum_win_env_compare(const void *left, const void *right) {
    const PlumWinEnvEntry *a = left, *b = right;
    int cmp = plum_win_env_key_compare(a, b);
    if (cmp != 0) return cmp;
    return (a->order > b->order) - (a->order < b->order);
}

// Success with *out == NULL means inherit. A requested overlay either
// produces a sorted, unique UTF-16 block or fails; OOM must not silently
// change which environment the child receives. All scratch storage is
// released here; only the final block survives until CreateProcessW.
static int plum_win_env_block(char **extra, wchar_t **out) {
    wchar_t *parent = NULL, *block = NULL;
    PlumWinEnvEntry *entries = NULL;
    size_t count = 0, used = 0, units = 1;
    int result = -1;
    *out = NULL;
    if (extra == NULL || extra[0] == NULL) return 0;
    parent = GetEnvironmentStringsW();
    if (parent == NULL) goto done;
    for (const wchar_t *p = parent; *p; p += wcslen(p) + 1) count++;
    for (size_t i = 0; extra[i] != NULL; i++) {
        if (count == SIZE_MAX) goto done;
        count++;
    }
    if (count > SIZE_MAX / sizeof(*entries)) goto done;
    entries = calloc(count, sizeof(*entries));
    if (entries == NULL) goto done;
    for (wchar_t *p = parent; *p; p += wcslen(p) + 1) {
        entries[used].text = p;
        entries[used].order = used;
        used++;
    }
    for (size_t i = 0; extra[i] != NULL; i++) {
        wchar_t *wide = plum_win_utf16(extra[i], INT_MAX);
        if (wide == NULL) goto done;
        entries[used].text = wide;
        entries[used].order = used;
        entries[used].owned = 1;
        used++;
        // Overlay names must be nonempty. Leading '=' is reserved for
        // Windows' inherited per-drive current-directory entries.
        if (*wide == L'=' || wcschr(wide, L'=') == NULL) goto done;
    }
    for (size_t i = 0; i < used; i++) {
        wchar_t *p = entries[i].text;
        // Preserve inherited entries such as '=C:=C:\\work'.
        wchar_t *eq = wcschr(p + (*p == L'='), L'=');
        if (eq == NULL || eq == p || eq - p > INT_MAX) goto done;
        entries[i].key_len = (int)(eq - p);
    }
    qsort(entries, used, sizeof(*entries), plum_win_env_compare);
    for (size_t i = 0; i < used; i++) {
        if (i + 1 < used && plum_win_env_key_compare(&entries[i], &entries[i + 1]) == 0) continue;
        size_t n = wcslen(entries[i].text) + 1;
        if (n > SIZE_MAX - units) goto done;
        units += n;
    }
    if (units > SIZE_MAX / sizeof(*block)) goto done;
    block = malloc(units * sizeof(*block));
    if (block == NULL) goto done;
    size_t pos = 0;
    for (size_t i = 0; i < used; i++) {
        if (i + 1 < used && plum_win_env_key_compare(&entries[i], &entries[i + 1]) == 0) continue;
        size_t n = wcslen(entries[i].text) + 1;
        memcpy(block + pos, entries[i].text, n * sizeof(*block));
        pos += n;
    }
    block[pos] = L'\0';
    *out = block;
    result = 0;
done:
    for (size_t i = 0; i < used; i++) {
        if (entries[i].owned) free(entries[i].text);
    }
    free(entries);
    if (parent != NULL) FreeEnvironmentStringsW(parent);
    return result;
}

// Write directly from the caller's buffer with constant scratch space.
// Bound each OS request to 1 MiB before narrowing to DWORD; short writes advance
// by their actual count, while failure or no progress aborts the launch.
static int plum_win_write_stdin(HANDLE file, const char *data, long long length) {
    if (length < 0 || (unsigned long long)length > SIZE_MAX ||
        (length > 0 && data == NULL)) return -1;
    size_t left = (size_t)length;
    while (left > 0) {
        DWORD request = left > 1024 * 1024 ? 1024 * 1024 : (DWORD)left;
        DWORD written = 0;
        if (!WriteFile(file, data, request, &written, NULL) ||
            written == 0 || written > request) return -1;
        data += written;
        left -= written;
    }
    return 0;
}

static long long plum_spawn_capture(const char *program, char **argv, char *out_path, char *err_path,
                                    char **envp, const char *stdin_data, long long stdin_len,
                                    int stdin_mode, int fail_on_exec, PlumChildState *async) {
    wchar_t *cmdline = plum_win_command_line(argv);
    wchar_t *env_block = NULL;
    HANDLE ho, he;
    PROCESS_INFORMATION pi;
    if (cmdline == NULL) return PLUM_SPAWN_FAILED;
    if (plum_win_env_block(envp, &env_block) != 0) {
        free(cmdline);
        return PLUM_SPAWN_FAILED;
    }

    ho = plum_win_temp_handle("plo", out_path);
    he = (ho != INVALID_HANDLE_VALUE) ? plum_win_temp_handle("ple", err_path) : INVALID_HANDLE_VALUE;
    if (ho == INVALID_HANDLE_VALUE || he == INVALID_HANDLE_VALUE) {
        if (ho != INVALID_HANDLE_VALUE) { CloseHandle(ho); plum_capture_remove(out_path); }
        free(cmdline);
        free(env_block);
        return PLUM_SPAWN_FAILED;
    }

    HANDLE hi = INVALID_HANDLE_VALUE;
    int hi_owned = 0;
    char in_path[PLUM_PROC_PATH_MAX];
    in_path[0] = '\0';
    if (stdin_mode < 0) {
        hi = GetStdHandle(STD_INPUT_HANDLE);
    } else if (stdin_mode == 0) {
        SECURITY_ATTRIBUTES sa;
        sa.nLength = sizeof(sa);
        sa.lpSecurityDescriptor = NULL;
        sa.bInheritHandle = FALSE;
        hi = CreateFileW(L"NUL", GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                         &sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
        hi_owned = 1;
    } else {
        HANDLE hw = plum_win_temp_handle("pli", in_path);
        SECURITY_ATTRIBUTES sa;
        if (hw == INVALID_HANDLE_VALUE) {
            CloseHandle(ho); CloseHandle(he);
            plum_capture_remove(out_path); plum_capture_remove(err_path);
            free(cmdline);
            free(env_block);
            return PLUM_SPAWN_FAILED;
        }
        int input_failed = plum_win_write_stdin(hw, stdin_data, stdin_len);
        if (!CloseHandle(hw)) input_failed = -1;
        if (input_failed) {
            CloseHandle(ho); CloseHandle(he);
            plum_capture_remove(in_path); plum_capture_remove(out_path); plum_capture_remove(err_path);
            free(cmdline);
            free(env_block);
            return PLUM_SPAWN_FAILED;
        }
        sa.nLength = sizeof(sa);
        sa.lpSecurityDescriptor = NULL;
        sa.bInheritHandle = FALSE;
        wchar_t *wide_in = plum_path_wide(in_path);
        hi = wide_in ? CreateFileW(wide_in, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                                  &sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL) : INVALID_HANDLE_VALUE;
        free(wide_in);
        hi_owned = 1;
    }
    if (hi == INVALID_HANDLE_VALUE) {
        if (hi_owned) { /* already invalid */ }
        CloseHandle(ho); CloseHandle(he);
        plum_capture_remove(out_path); plum_capture_remove(err_path);
        if (in_path[0] != '\0') plum_capture_remove(in_path);
        free(cmdline);
        free(env_block);
        return PLUM_SPAWN_FAILED;
    }

    // `lpApplicationName` is NULL so that `CreateProcess` searches PATH
    // and appends `.exe`, which is what makes `clang` resolve the same
    // way it does everywhere else. The ambiguity that normally makes
    // that risky -- an unquoted program path containing spaces -- does
    // not apply, because the program was quoted above like any other
    // argument.
    if (!plum_win_create_process(cmdline, env_block, hi, ho, he, &pi)) {
        CloseHandle(ho);
        CloseHandle(he);
        if (hi_owned) CloseHandle(hi);
        plum_capture_remove(out_path);
        plum_capture_remove(err_path);
        if (in_path[0] != '\0') plum_capture_remove(in_path);
        free(cmdline);
        free(env_block);
        return PLUM_SPAWN_FAILED;
    }

    free(cmdline);
    free(env_block);
    CloseHandle(pi.hThread);

    // Asynchronous: keep the process handle to wait on, and retain the
    // parent's capture handles until collection. Child stdio uses its own
    // inherited duplicates, independent of the parent's handle lifetimes.
    // The input file cannot be deleted yet either: Windows refuses to
    // unlink a file that is open, which is why POSIX can unlink it at
    // spawn and this cannot.
    if (async != NULL) {
        async->pid = (long long)(intptr_t)pi.hProcess;
        async->h_out = (void *)ho;
        async->h_err = (void *)he;
        if (hi_owned) CloseHandle(hi);
        if (in_path[0] != '\0') async->in_path = plum_strdup_or_null(in_path);
        (void)program;
        (void)fail_on_exec;
        return 0;
    }

    long long code = WaitForSingleObject(pi.hProcess, INFINITE) == WAIT_OBJECT_0 ?
        plum_win_exit_code(pi.hProcess) : PLUM_SPAWN_FAILED;
    CloseHandle(pi.hProcess);

    // Closed only after the child has exited, so everything it wrote is
    // flushed and the files can be read back in full.
    CloseHandle(ho);
    CloseHandle(he);
    if (hi_owned) CloseHandle(hi);
    if (in_path[0] != '\0') plum_capture_remove(in_path);
    (void)program;
    (void)fail_on_exec;
    if (code == PLUM_SPAWN_FAILED) {
        plum_capture_remove(out_path);
        plum_capture_remove(err_path);
    }
    return code;
}

#endif

// `args_joined` holds `argc` arguments (NOT including `program` itself,
// which becomes `argv[0]`), separated by `PLUM_PROC_ARG_SEP`. Blocks
// until the child exits. Returns a handle, or -1 if the process could
// never even be started (no free slot, `fork`/temp-file-creation
// failure) — NOT for the child's own exit code being non-zero, which
// is a normal, successful `process_run` outcome retrievable via `
// process_exit_code` (a failing compile is expected/routine, not a
// shim-level error).
static long long process_run_locked(long long slot, const char *program, const char *args_joined, long long argc) {
    char *args_copy = NULL;
    char **argv = plum_split_args(program, args_joined, argc, &args_copy);
    if (argv == NULL) {
        return -1;
    }

    char out_path[PLUM_PROC_PATH_MAX];
    char err_path[PLUM_PROC_PATH_MAX];
    long long code = plum_spawn_capture(program, argv, out_path, err_path, NULL, NULL, 0, -1, 0, NULL);
    if (code == PLUM_SPAWN_FAILED) {
        free(args_copy);
        free(argv);
        return -1;
    }

    plum_process_slots[slot].in_use = 1;
    plum_process_slots[slot].exit_code = code;
    plum_process_capture(&plum_process_slots[slot], out_path, err_path);

    plum_capture_remove(out_path);
    plum_capture_remove(err_path);
    free(args_copy);
    free(argv);
    return slot;
}

// Structured run: argv (not a shell), optional env overlay (`NAME=value`
// entries, TAB-joined like args), optional stdin bytes. `has_stdin` of
// 0 feeds the child an empty stdin; 1 feeds `stdin_data` of length
// `stdin_len`. Exec failure is a launch error (handle -1), not exit 127.
static long long process_run_ex_locked(long long slot, const char *program, const char *args_joined, long long argc,
                         const char *env_joined, long long envc,
                         const char *stdin_data, long long stdin_len, long long has_stdin) {
    char *args_copy = NULL;
    char **argv = plum_split_args(program, args_joined, argc, &args_copy);
    if (argv == NULL) {
        return -1;
    }
    char *env_copy = NULL;
    char **envp = plum_split_joined(env_joined, envc, &env_copy);
    if (envc > 0 && envp == NULL) {
        free(args_copy);
        free(argv);
        return -1;
    }

    char out_path[PLUM_PROC_PATH_MAX];
    char err_path[PLUM_PROC_PATH_MAX];
    int stdin_mode = has_stdin ? 1 : 0;
    long long code = plum_spawn_capture(program, argv, out_path, err_path, envp,
                                        stdin_data, stdin_len, stdin_mode, 1, NULL);
    if (code == PLUM_SPAWN_FAILED) {
        free(args_copy);
        free(argv);
        free(env_copy);
        free(envp);
        return -1;
    }

    plum_process_slots[slot].in_use = 1;
    plum_process_slots[slot].exit_code = code;
    plum_process_capture(&plum_process_slots[slot], out_path, err_path);

    plum_capture_remove(out_path);
    plum_capture_remove(err_path);
    free(args_copy);
    free(argv);
    free(env_copy);
    free(envp);
    return slot;
}

static long long process_exit_code_locked(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return -1;
    }
    return plum_process_slots[handle].exit_code;
}

static const char *process_stdout_data_locked(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return "";
    }
    return plum_process_slots[handle].out_data != NULL ? plum_process_slots[handle].out_data : "";
}

static const char *process_stderr_data_locked(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return "";
    }
    return plum_process_slots[handle].err_data != NULL ? plum_process_slots[handle].err_data : "";
}

// A completed capture failure is sticky, independent of the exit status.
static long long process_capture_status_locked(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) return -1;
    return plum_process_slots[handle].capture_failed ? -1 : 0;
}

static long long process_stdout_length_locked(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) return 0;
    return plum_process_slots[handle].out_length;
}

static long long process_stderr_length_locked(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) return 0;
    return plum_process_slots[handle].err_length;
}

static void process_free_locked(long long handle) {
    process_child_free_locked(handle);
}

// --- Asynchronous children (issue #7) ---
//
// `process_run` blocks, which is the right shape for a compiler
// shelling out to `clang` and the wrong one for an application that
// wants to stay responsive while something runs. These add the other
// shape: start it, look in on it, stop it.
//
// The temp-file capture is what makes this cheap. A pipe-based child
// would need somebody draining it the whole time the child runs -- the
// deadlock this file's header describes -- so `poll` could not be a
// question you ask occasionally. A file has no fixed buffer, so nothing
// has to be read until the child is gone, and `poll` really is just
// `waitpid(WNOHANG)`.
//
// **The status is collected exactly once.** The transition from running
// to finished reads both files, deletes them, and fills the same
// `out_data`/`err_data` the blocking path fills -- so
// `process_exit_code`, `process_stdout_data` and `process_stderr_data`
// work unchanged afterwards, and work repeatedly. There is deliberately
// no way to read a child's output before it exits: that would mean
// defining what a partial read of a file somebody is still writing
// means, and no caller has asked for it.

static void plum_child_collect(long long handle, long long code, int capture) {
    PlumProcessSlot *slot = &plum_process_slots[handle];
    PlumChildState *c = slot->child;
    if (c == NULL || c->finished) return;
    slot->exit_code = code;
#if defined(_WIN32)
    // Closed only now, so everything the child wrote is flushed before
    // the files are read back -- the same ordering the blocking path
    // depends on.
    if (c->h_out != NULL) CloseHandle((HANDLE)c->h_out);
    if (c->h_err != NULL) CloseHandle((HANDLE)c->h_err);
    c->h_out = NULL;
    c->h_err = NULL;
    if (c->pid != 0) CloseHandle((HANDLE)(intptr_t)c->pid);
    c->pid = 0;
#endif
    if (capture) plum_process_capture(slot, c->out_path, c->err_path);
    if (c->out_path != NULL) plum_capture_remove(c->out_path);
    if (c->err_path != NULL) plum_capture_remove(c->err_path);
    if (c->in_path != NULL) plum_capture_remove(c->in_path);
    c->finished = 1;
}

#if defined(_WIN32)
static long long plum_win_child_collect(long long handle) {
    HANDLE process = (HANDLE)(intptr_t)plum_process_slots[handle].child->pid;
    long long code = plum_win_exit_code(process);
    if (code == PLUM_SPAWN_FAILED) return -1;
    plum_child_collect(handle, code, 1);
    return 1;
}
#endif

static PlumProcessSlot *plum_slot_of(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return NULL;
    }
    return &plum_process_slots[handle];
}

// Starts `program` without waiting for it. Same argument, environment
// and stdin packing as `process_run_ex`; same -1 for a child that could
// not be started at all, including a program that does not exist.
static long long process_spawn_locked(long long slot, const char *program, const char *args_joined, long long argc,
                        const char *env_joined, long long envc,
                        const char *stdin_data, long long stdin_len, long long has_stdin) {
    char *args_copy = NULL;
    char **argv = plum_split_args(program, args_joined, argc, &args_copy);
    if (argv == NULL) return -1;
    char *env_copy = NULL;
    char **envp = plum_split_joined(env_joined, envc, &env_copy);
    if (envc > 0 && envp == NULL) {
        free(args_copy);
        free(argv);
        return -1;
    }

    PlumChildState *child = calloc(1, sizeof(PlumChildState));
    if (child == NULL) {
        free(args_copy); free(argv); free(env_copy); free(envp);
        return -1;
    }

    char out_path[PLUM_PROC_PATH_MAX];
    char err_path[PLUM_PROC_PATH_MAX];
    int stdin_mode = has_stdin ? 1 : 0;
    long long rc = plum_spawn_capture(program, argv, out_path, err_path, envp,
                                      stdin_data, stdin_len, stdin_mode, 1, child);
    free(args_copy); free(argv); free(env_copy); free(envp);
    if (rc == PLUM_SPAWN_FAILED) {
        free(child->in_path);
        free(child);
        return -1;
    }

    child->out_path = plum_strdup_or_null(out_path);
    child->err_path = plum_strdup_or_null(err_path);
    plum_process_slots[slot].in_use = 1;
    plum_process_slots[slot].exit_code = -1;
    plum_process_slots[slot].out_data = NULL;
    plum_process_slots[slot].err_data = NULL;
    plum_process_slots[slot].out_length = plum_process_slots[slot].err_length = 0;
    plum_process_slots[slot].capture_failed = 0;
    plum_process_slots[slot].child = child;
    if (child->out_path == NULL || child->err_path == NULL) {
        // Tracking allocation failed after launch. Reap without reading,
        // then use the stack paths to delete even an untracked capture.
        process_child_free_locked(slot);
        plum_capture_remove(out_path);
        plum_capture_remove(err_path);
        return -1;
    }
    return slot;
}

// 1 if the child has exited (its result is now readable), 0 if it is
// still running, -1 if the handle is not a live child.
static long long process_poll_locked(long long handle) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL || slot->child == NULL) return -1;
    if (slot->child->finished) return 1;
#if defined(_WIN32)
    HANDLE h = (HANDLE)(intptr_t)slot->child->pid;
    DWORD w = WaitForSingleObject(h, 0);
    if (w == WAIT_TIMEOUT) return 0;
    if (w != WAIT_OBJECT_0) return -1;
    return plum_win_child_collect(handle);
#else
    int status = 0;
    pid_t r;
    do { r = waitpid((pid_t)slot->child->pid, &status, WNOHANG); } while (r < 0 && errno == EINTR);
    if (r == 0) return 0;
    if (r < 0) return -1;
    plum_child_collect(handle, WIFEXITED(status) ? (long long)WEXITSTATUS(status) : -1, 1);
    return 1;
#endif
}

// Waits for the child. A negative `timeout_nanos` waits forever.
// Returns 1 if it exited, 0 if the timeout expired first, -1 on error.
//
// POSIX waits poll with a capped 1–20ms backoff, releasing the slot lock
// between checks. Windows waits pin the OS process with a duplicate handle.
// Neither strategy blocks termination or unrelated process operations.
long long process_child_wait(long long handle, long long timeout_nanos) {
#if defined(_WIN32)
    PlumProcessSlot *slot = plum_process_lock(handle);
    if (slot == NULL) return -1;
    if (slot->child == NULL || slot->child->finished) {
        long long result = slot->child == NULL ? -1 : 1;
        pthread_mutex_unlock(&slot->lock);
        return result;
    }
    // A duplicate pins the OS object while another task terminates or frees
    // the slot. Revalidate the identity before publishing the result.
    HANDLE process = NULL;
    int duplicated = DuplicateHandle(GetCurrentProcess(), (HANDLE)(intptr_t)slot->child->pid,
        GetCurrentProcess(), &process, 0, FALSE, DUPLICATE_SAME_ACCESS);
    pthread_mutex_unlock(&slot->lock);
    if (!duplicated) return -1;
    long long ms = timeout_nanos < 0 ? -1 : timeout_nanos / 1000000LL;
    if (timeout_nanos > 0 && timeout_nanos % 1000000LL != 0) ms++;
    long long result;
    for (;;) {
        DWORD chunk = ms < 0 ? INFINITE : ms > 0x7FFFFFFELL ? 0x7FFFFFFE : (DWORD)ms;
        DWORD w = WaitForSingleObject(process, chunk);
        if (w == WAIT_OBJECT_0) { result = 1; break; }
        if (w != WAIT_TIMEOUT) { result = -1; break; }
        ms -= chunk;
        if (ms == 0) { result = 0; break; }
    }
    CloseHandle(process);
    slot = plum_process_lock(handle);
    if (slot == NULL) return -1;
    if (slot->child->finished) result = 1;
    else if (result == 1) result = plum_win_child_collect((handle - 1) % PLUM_PROC_MAX_SLOTS);
    pthread_mutex_unlock(&slot->lock);
    return result;
#else
    // A blocking waitpid under the slot lock would prevent terminate/drop.
    // Poll and reap under that lock instead, so concurrent waiters cannot reap
    // twice or signal a PID between reaping it and recording completion.
    long long waited = 0, step = 1000000LL;
    for (;;) {
        PlumProcessSlot *slot = plum_process_lock(handle);
        if (slot == NULL) return -1;
        long long result = process_poll_locked((handle - 1) % PLUM_PROC_MAX_SLOTS);
        pthread_mutex_unlock(&slot->lock);
        if (result != 0) return result;
        if (timeout_nanos >= 0 && waited >= timeout_nanos) return 0;
        long long nap = step;
        if (timeout_nanos >= 0 && nap > timeout_nanos - waited) nap = timeout_nanos - waited;
        struct timespec req = { (time_t)(nap / 1000000000LL), (long)(nap % 1000000000LL) };
        while (nanosleep(&req, &req) != 0 && errno == EINTR) { }
        if (timeout_nanos >= 0) waited += nap;
        if (step < 20000000LL) step *= 2;
        if (step > 20000000LL) step = 20000000LL;
    }
#endif
}

// Asks the child to stop. `hard` of 0 is a polite request the child can
// handle or ignore (SIGTERM, and on Windows the only thing there is);
// non-zero cannot be refused (SIGKILL, `TerminateProcess`).
//
// Windows has no SIGTERM, so both spellings are `TerminateProcess` --
// a real difference, documented rather than papered over: a program
// that relies on a child cleaning up after a polite terminate will not
// get that on Windows.
//
// Returns 0 on success, -1 if the handle is not a live child. Signalling
// a child that has ALREADY exited is success, not failure: the caller
// asked for it to be gone and it is.
static long long process_terminate_locked(long long handle, long long hard) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL || slot->child == NULL) return -1;
    if (slot->child->finished) return 0;
#if defined(_WIN32)
    (void)hard;
    return TerminateProcess((HANDLE)(intptr_t)slot->child->pid, 1) ? 0 : -1;
#else
    return kill((pid_t)slot->child->pid, hard ? SIGKILL : SIGTERM) == 0 ? 0 : -1;
#endif
}

// The operating system's identifier for the child, for a caller that
// has to report it or hand it to something else. Windows reports the
// process id rather than the handle, which is what a user would
// recognise in Task Manager.
static long long process_child_pid_locked(long long handle) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL || slot->child == NULL) return -1;
#if defined(_WIN32)
    if (slot->child->pid == 0) return -1;
    return (long long)GetProcessId((HANDLE)(intptr_t)slot->child->pid);
#else
    return slot->child->pid;
#endif
}

// Releases the slot, and is what the `Child` handle runs when the value
// dies.
//
// A child still running is KILLED and reaped. The alternative -- leave
// it alive -- sounds gentler and is not: nothing can reach that process
// again, since the value that could poll it, wait for it, signal it or
// read its output has just been destroyed. It would also leak the
// temp files it is still writing to, and on POSIX leave a zombie until
// this program exits, because the only process that could reap it has
// stopped tracking it.
//
// `drop_policy` is where a future choice would live. It is not settable
// from Plum today.
static void process_child_free_locked(long long handle) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL) return;
    PlumChildState *c = slot->child;
    if (c != NULL) {
        if (!c->finished) {
            (void)process_terminate_locked(handle, 1);
#if defined(_WIN32)
            HANDLE h = (HANDLE)(intptr_t)c->pid;
            WaitForSingleObject(h, 5000);
            // Drop discards the status and output. A status-query failure
            // must not obstruct resource cleanup or fabricate a result.
            plum_child_collect(handle, -1, 0);
#else
            int status = 0;
            pid_t r;
            do { r = waitpid((pid_t)c->pid, &status, 0); } while (r < 0 && errno == EINTR);
            plum_child_collect(handle, (r > 0 && WIFEXITED(status)) ? (long long)WEXITSTATUS(status) : -1, 0);
#endif
        }
        free(c->out_path);
        free(c->err_path);
        free(c->in_path);
        free(c);
        slot->child = NULL;
    }
    free(slot->out_data);
    free(slot->err_data);
    slot->out_data = NULL;
    slot->err_data = NULL;
    slot->out_length = slot->err_length = 0;
    slot->capture_failed = 0;
    slot->in_use = 0;
}

// The ABI boundary owns identity validation and locking. Internal helpers use
// indices only while the corresponding slot is locked.
#define PLUM_PROCESS_ACCESSOR(type, name, invalid) \
    type name(long long handle) { \
        PlumProcessSlot *slot = plum_process_lock(handle); \
        if (slot == NULL) return invalid; \
        type result = name##_locked((handle - 1) % PLUM_PROC_MAX_SLOTS); \
        pthread_mutex_unlock(&slot->lock); \
        return result; \
    }
PLUM_PROCESS_ACCESSOR(long long, process_exit_code, -1)
PLUM_PROCESS_ACCESSOR(const char *, process_stdout_data, "")
PLUM_PROCESS_ACCESSOR(const char *, process_stderr_data, "")
PLUM_PROCESS_ACCESSOR(long long, process_capture_status, -1)
PLUM_PROCESS_ACCESSOR(long long, process_stdout_length, 0)
PLUM_PROCESS_ACCESSOR(long long, process_stderr_length, 0)
PLUM_PROCESS_ACCESSOR(long long, process_poll, -1)
PLUM_PROCESS_ACCESSOR(long long, process_child_pid, -1)
#undef PLUM_PROCESS_ACCESSOR

long long process_terminate(long long handle, long long hard) {
    PlumProcessSlot *slot = plum_process_lock(handle);
    if (slot == NULL) return -1;
    long long result = process_terminate_locked((handle - 1) % PLUM_PROC_MAX_SLOTS, hard);
    pthread_mutex_unlock(&slot->lock);
    return result;
}

void process_child_free(long long handle) {
    PlumProcessSlot *slot = plum_process_lock(handle);
    if (slot == NULL) return;
    process_free_locked((handle - 1) % PLUM_PROC_MAX_SLOTS);
    plum_process_available(slot);
    pthread_mutex_unlock(&slot->lock);
}
void process_free(long long handle) { process_child_free(handle); }

// Finish a reserved launch. Failed launches release the reservation; successful
// launches publish the complete state before exposing its identity to callers.
static long long plum_process_publish(long long index, long long result) {
    PlumProcessSlot *slot = &plum_process_slots[index];
    if (result < 0) plum_process_available(slot);
    long long handle = result < 0 ? -1 : slot->identity;
    pthread_mutex_unlock(&slot->lock);
    return handle;
}

long long process_run(const char *program, const char *args, long long argc) {
    long long index = plum_process_reserve();
    if (index < 0) return -1;
    return plum_process_publish(index, process_run_locked(index, program, args, argc));
}
long long process_run_ex(const char *program, const char *args, long long argc,
                         const char *env, long long envc,
                         const char *input, long long length, long long has_input) {
    long long index = plum_process_reserve();
    if (index < 0) return -1;
    return plum_process_publish(index, process_run_ex_locked(index, program, args, argc,
        env, envc, input, length, has_input));
}
long long process_spawn(const char *program, const char *args, long long argc,
                        const char *env, long long envc,
                        const char *input, long long length, long long has_input) {
    long long index = plum_process_reserve();
    if (index < 0) return -1;
    return plum_process_publish(index, process_spawn_locked(index, program, args, argc,
        env, envc, input, length, has_input));
}

// The inherited-stdio counterpart to `plum_spawn_capture`, and the
// second place this file needs a platform split. It was missed when the
// first one was written -- the POSIX `fork`/`waitpid` here sat outside
// any guard and was found by the Windows CI leg, which is what that leg
// is for.
#if !defined(_WIN32)

static long long plum_spawn_inherit(const char *program, char **argv) {
    pid_t pid = fork();
    if (pid < 0) return -1;
    if (pid == 0) {
        execvp(program, argv);
        _exit(127);
    }
    int status = 0;
    if (waitpid(pid, &status, 0) < 0) return -1;
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    // 128 + signal is the shell convention, kept so `plum run` reports
    // a killed child the way anything else in a pipeline would.
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return -1;
}

#else

static long long plum_spawn_inherit(const char *program, char **argv) {
    wchar_t *cmdline = plum_win_command_line(argv);
    PROCESS_INFORMATION pi;
    if (cmdline == NULL) return -1;

    // Preserve only stdio, even for `plum run`. Blanket inheritance here
    // would otherwise pick up another task's temporary stdio duplicates.
    if (!plum_win_create_process(cmdline, NULL, GetStdHandle(STD_INPUT_HANDLE),
        GetStdHandle(STD_OUTPUT_HANDLE), GetStdHandle(STD_ERROR_HANDLE), &pi)) {
        free(cmdline);
        return -1;
    }
    free(cmdline);
    CloseHandle(pi.hThread);

    long long code = WaitForSingleObject(pi.hProcess, INFINITE) == WAIT_OBJECT_0 ?
        plum_win_exit_code(pi.hProcess) : PLUM_SPAWN_FAILED;
    CloseHandle(pi.hProcess);
    (void)program;
    return code == PLUM_SPAWN_FAILED ? -1 : code;
}

#endif

// Runs a command with stdio INHERITED — no capture, no temp files — and
// returns its exit status. `process_run` above is for a caller that
// wants the output as a value; this is for a caller that wants the
// child to simply BE the program: `plum run` streaming its output as it
// happens, and a child that can read stdin.
//
// Deliberately separate rather than a flag on `process_run`: that one's
// whole design is the temp-file capture described above, and threading
// "except don't" through it would leave every path harder to read.
long long process_run_inherit(const char *program, const char *args_joined, long long argc) {
    char *args_copy = strdup(args_joined);
    char **argv = malloc(sizeof(char *) * (size_t)(argc + 2));
    if (args_copy == NULL || argv == NULL) {
        free(args_copy);
        free(argv);
        return -1;
    }
    // Same separator-splitting as `process_run` above; see its own note
    // on why the first element is special-cased.
    argv[0] = (char *)program;
    long long idx = 1;
    if (argc > 0) {
        argv[idx++] = args_copy;
        for (char *c = args_copy; *c != '\0'; c++) {
            if (*c == PLUM_PROC_ARG_SEP) {
                *c = '\0';
                if (idx <= argc) {
                    argv[idx++] = c + 1;
                }
            }
        }
    }
    argv[idx] = NULL;

    long long code = plum_spawn_inherit(program, argv);
    free(args_copy);
    free(argv);
    return code;
}
