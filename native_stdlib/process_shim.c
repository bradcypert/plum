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
// **Not thread-safe** (a single, static, unlocked slot table) —
// acceptable for v1: every other blocking `native_stdlib` primitive
// (`net_shim.c`'s sockets included) is already scoped to single-
// threaded/sequential use, and a compiler shelling out to `clang` one
// invocation at a time (the actual motivating use case) never needs
// concurrent calls anyway.
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
// Slot bookkeeping, argument splitting, reading the captured output
// back and cleaning up are shared and were not touched, so the POSIX
// behaviour is unchanged by the port rather than merely believed to
// be.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

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
    int in_use;
    long long exit_code;
    char *out_data;
    char *err_data;
    PlumChildState *child;
} PlumProcessSlot;

static PlumProcessSlot plum_process_slots[PLUM_PROC_MAX_SLOTS];

// Defined further down with the asynchronous children. Declared here
// because `process_free` has to hand a spawned slot over to it, and
// that appears first.
void process_child_free(long long handle);
long long process_terminate(long long handle, long long hard);

static char *plum_strdup_or_null(const char *s) {
    if (s == NULL) return NULL;
    size_t n = strlen(s) + 1;
    char *p = malloc(n);
    if (p != NULL) memcpy(p, s, n);
    return p;
}

static char *plum_read_whole_file_or_empty(const char *path) {
    FILE *fp = fopen(path, "rb");
    if (fp == NULL) {
        return strdup("");
    }
    fseek(fp, 0, SEEK_END);
    long size = ftell(fp);
    rewind(fp);
    if (size < 0) {
        fclose(fp);
        return strdup("");
    }
    char *buf = malloc((size_t)size + 1);
    if (buf == NULL) {
        fclose(fp);
        return strdup("");
    }
    size_t n = fread(buf, 1, (size_t)size, fp);
    buf[n] = '\0';
    fclose(fp);
    return buf;
}

// A sentinel distinct from every real exit code. It cannot be -1:
// `process_run` already uses -1 as the exit code of a child killed by a
// signal, which is a SUCCESSFUL run with an unusual outcome, not a
// failure to start anything.
#define PLUM_SPAWN_FAILED (-1000000000LL)

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

// `GetTempFileNameA` creates the file, which is what is wanted here --
// the handle is opened over it immediately afterwards.
static HANDLE plum_win_temp_handle(const char *prefix, char *path_out) {
    char base[PLUM_PROC_PATH_MAX];
    SECURITY_ATTRIBUTES sa;
    DWORD n = GetTempPathA((DWORD)sizeof(base), base);
    if (n == 0 || n >= sizeof(base)) return INVALID_HANDLE_VALUE;
    if (GetTempFileNameA(base, prefix, 0, path_out) == 0) return INVALID_HANDLE_VALUE;

    sa.nLength = sizeof(sa);
    sa.lpSecurityDescriptor = NULL;
    sa.bInheritHandle = TRUE; // the child must be able to write to it
    return CreateFileA(path_out, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                       &sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
}

static int plum_win_env_key_overridden(char **extra, const char *entry) {
    const char *eq = strchr(entry, '=');
    size_t klen = eq ? (size_t)(eq - entry) : strlen(entry);
    int i;
    for (i = 0; extra[i] != NULL; i++) {
        if (strncmp(extra[i], entry, klen) == 0 && extra[i][klen] == '=') return 1;
    }
    return 0;
}

// NULL means inherit. A non-NULL extra list is overlaid on the parent
// environment and returned as a double-NUL block the caller frees.
static char *plum_win_env_block(char **extra) {
    char *parent;
    const char *p;
    size_t cap = 1;
    size_t n = 0;
    char *block;
    int i;
    if (extra == NULL) return NULL;
    parent = GetEnvironmentStringsA();
    if (parent == NULL) return NULL;
    for (i = 0; extra[i] != NULL; i++) cap += strlen(extra[i]) + 1;
    for (p = parent; *p != '\0'; ) {
        size_t elen = strlen(p);
        if (!plum_win_env_key_overridden(extra, p)) cap += elen + 1;
        p += elen + 1;
    }
    block = (char *)malloc(cap + 1);
    if (block == NULL) {
        FreeEnvironmentStringsA(parent);
        return NULL;
    }
    for (i = 0; extra[i] != NULL; i++) {
        size_t elen = strlen(extra[i]);
        memcpy(block + n, extra[i], elen + 1);
        n += elen + 1;
    }
    for (p = parent; *p != '\0'; ) {
        size_t elen = strlen(p);
        if (!plum_win_env_key_overridden(extra, p)) {
            memcpy(block + n, p, elen + 1);
            n += elen + 1;
        }
        p += elen + 1;
    }
    block[n] = '\0';
    FreeEnvironmentStringsA(parent);
    return block;
}

static long long plum_spawn_capture(const char *program, char **argv, char *out_path, char *err_path,
                                    char **envp, const char *stdin_data, long long stdin_len,
                                    int stdin_mode, int fail_on_exec, PlumChildState *async) {
    char *cmdline;
    size_t cap = 0;
    size_t len = 0;
    int i;
    HANDLE ho, he;
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    DWORD code = 0;

    // Worst case per argument: every character doubled, plus two quotes
    // and a separating space.
    for (i = 0; argv[i] != NULL; i++) cap += strlen(argv[i]) * 2 + 3;
    cap += 1;
    cmdline = (char *)malloc(cap);
    if (cmdline == NULL) return PLUM_SPAWN_FAILED;
    for (i = 0; argv[i] != NULL; i++) {
        if (plum_win_append_arg(cmdline, cap, &len, argv[i]) != 0) {
            free(cmdline);
            return PLUM_SPAWN_FAILED;
        }
    }
    cmdline[len] = '\0';

    ho = plum_win_temp_handle("plo", out_path);
    he = (ho != INVALID_HANDLE_VALUE) ? plum_win_temp_handle("ple", err_path) : INVALID_HANDLE_VALUE;
    if (ho == INVALID_HANDLE_VALUE || he == INVALID_HANDLE_VALUE) {
        if (ho != INVALID_HANDLE_VALUE) { CloseHandle(ho); DeleteFileA(out_path); }
        free(cmdline);
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
        sa.bInheritHandle = TRUE;
        hi = CreateFileA("NUL", GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                         &sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
        hi_owned = 1;
    } else {
        HANDLE hw = plum_win_temp_handle("pli", in_path);
        DWORD written = 0;
        SECURITY_ATTRIBUTES sa;
        if (hw == INVALID_HANDLE_VALUE) {
            CloseHandle(ho); CloseHandle(he);
            DeleteFileA(out_path); DeleteFileA(err_path);
            free(cmdline);
            return PLUM_SPAWN_FAILED;
        }
        if (stdin_len > 0 && stdin_data != NULL) {
            if (!WriteFile(hw, stdin_data, (DWORD)stdin_len, &written, NULL) || written != (DWORD)stdin_len) {
                CloseHandle(hw); CloseHandle(ho); CloseHandle(he);
                DeleteFileA(in_path); DeleteFileA(out_path); DeleteFileA(err_path);
                free(cmdline);
                return PLUM_SPAWN_FAILED;
            }
        }
        CloseHandle(hw);
        sa.nLength = sizeof(sa);
        sa.lpSecurityDescriptor = NULL;
        sa.bInheritHandle = TRUE;
        hi = CreateFileA(in_path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                         &sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
        hi_owned = 1;
    }
    if (hi == INVALID_HANDLE_VALUE) {
        if (hi_owned) { /* already invalid */ }
        CloseHandle(ho); CloseHandle(he);
        DeleteFileA(out_path); DeleteFileA(err_path);
        if (in_path[0] != '\0') DeleteFileA(in_path);
        free(cmdline);
        return PLUM_SPAWN_FAILED;
    }

    char *env_block = plum_win_env_block(envp);

    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = hi;
    si.hStdOutput = ho;
    si.hStdError = he;
    ZeroMemory(&pi, sizeof(pi));

    // `lpApplicationName` is NULL so that `CreateProcess` searches PATH
    // and appends `.exe`, which is what makes `clang` resolve the same
    // way it does everywhere else. The ambiguity that normally makes
    // that risky -- an unquoted program path containing spaces -- does
    // not apply, because the program was quoted above like any other
    // argument.
    if (!CreateProcessA(NULL, cmdline, NULL, NULL, TRUE, 0, env_block, NULL, &si, &pi)) {
        CloseHandle(ho);
        CloseHandle(he);
        if (hi_owned) CloseHandle(hi);
        DeleteFileA(out_path);
        DeleteFileA(err_path);
        if (in_path[0] != '\0') DeleteFileA(in_path);
        free(cmdline);
        free(env_block);
        return PLUM_SPAWN_FAILED;
    }

    CloseHandle(pi.hThread);

    // Asynchronous: keep the process handle to wait on, and keep the
    // two file handles OPEN -- they are the child's stdout and stderr,
    // and closing them here would close the child's own descriptors.
    // The input file cannot be deleted yet either: Windows refuses to
    // unlink a file that is open, which is why POSIX can unlink it at
    // spawn and this cannot.
    if (async != NULL) {
        async->pid = (long long)(intptr_t)pi.hProcess;
        async->h_out = (void *)ho;
        async->h_err = (void *)he;
        if (hi_owned) CloseHandle(hi);
        if (in_path[0] != '\0') async->in_path = plum_strdup_or_null(in_path);
        free(cmdline);
        free(env_block);
        (void)program;
        (void)fail_on_exec;
        return 0;
    }

    WaitForSingleObject(pi.hProcess, INFINITE);
    if (!GetExitCodeProcess(pi.hProcess, &code)) code = (DWORD)-1;
    CloseHandle(pi.hProcess);

    // Closed only after the child has exited, so everything it wrote is
    // flushed and the files can be read back in full.
    CloseHandle(ho);
    CloseHandle(he);
    if (hi_owned) CloseHandle(hi);
    if (in_path[0] != '\0') DeleteFileA(in_path);
    free(cmdline);
    free(env_block);
    (void)program;
    (void)fail_on_exec;
    return (long long)(int)code;
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
long long process_run(const char *program, const char *args_joined, long long argc) {
    long long slot = -1;
    for (long long i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        if (!plum_process_slots[i].in_use) {
            slot = i;
            break;
        }
    }
    if (slot < 0) {
        return -1;
    }

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
    plum_process_slots[slot].out_data = plum_read_whole_file_or_empty(out_path);
    plum_process_slots[slot].err_data = plum_read_whole_file_or_empty(err_path);

    remove(out_path);
    remove(err_path);
    free(args_copy);
    free(argv);
    return slot;
}

// Structured run: argv (not a shell), optional env overlay (`NAME=value`
// entries, TAB-joined like args), optional stdin bytes. `has_stdin` of
// 0 feeds the child an empty stdin; 1 feeds `stdin_data` of length
// `stdin_len`. Exec failure is a launch error (handle -1), not exit 127.
long long process_run_ex(const char *program, const char *args_joined, long long argc,
                         const char *env_joined, long long envc,
                         const char *stdin_data, long long stdin_len, long long has_stdin) {
    long long slot = -1;
    for (long long i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        if (!plum_process_slots[i].in_use) {
            slot = i;
            break;
        }
    }
    if (slot < 0) {
        return -1;
    }

    char *args_copy = NULL;
    char **argv = plum_split_args(program, args_joined, argc, &args_copy);
    if (argv == NULL) {
        return -1;
    }
    char *env_copy = NULL;
    char **envp = plum_split_joined(env_joined, envc, &env_copy);

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
    plum_process_slots[slot].out_data = plum_read_whole_file_or_empty(out_path);
    plum_process_slots[slot].err_data = plum_read_whole_file_or_empty(err_path);

    remove(out_path);
    remove(err_path);
    free(args_copy);
    free(argv);
    free(env_copy);
    free(envp);
    return slot;
}

long long process_exit_code(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return -1;
    }
    return plum_process_slots[handle].exit_code;
}

const char *process_stdout_data(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return "";
    }
    return plum_process_slots[handle].out_data;
}

const char *process_stderr_data(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return "";
    }
    return plum_process_slots[handle].err_data;
}

void process_free(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return;
    }
    // A slot filled by `process_spawn` carries more than these two
    // buffers, so it is released by the function that knows about all
    // of it rather than half-released here.
    if (plum_process_slots[handle].child != NULL) {
        process_child_free(handle);
        return;
    }
    free(plum_process_slots[handle].out_data);
    free(plum_process_slots[handle].err_data);
    plum_process_slots[handle].in_use = 0;
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

static void plum_child_collect(long long handle, long long code) {
    PlumProcessSlot *slot = &plum_process_slots[handle];
    PlumChildState *c = slot->child;
    if (c == NULL || c->finished) return;
    slot->exit_code = code;
    slot->out_data = plum_read_whole_file_or_empty(c->out_path);
    slot->err_data = plum_read_whole_file_or_empty(c->err_path);
    if (c->out_path != NULL) remove(c->out_path);
    if (c->err_path != NULL) remove(c->err_path);
    if (c->in_path != NULL) remove(c->in_path);
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
    c->finished = 1;
}

static PlumProcessSlot *plum_slot_of(long long handle) {
    if (handle < 0 || handle >= PLUM_PROC_MAX_SLOTS || !plum_process_slots[handle].in_use) {
        return NULL;
    }
    return &plum_process_slots[handle];
}

// Starts `program` without waiting for it. Same argument, environment
// and stdin packing as `process_run_ex`; same -1 for a child that could
// not be started at all, including a program that does not exist.
long long process_spawn(const char *program, const char *args_joined, long long argc,
                        const char *env_joined, long long envc,
                        const char *stdin_data, long long stdin_len, long long has_stdin) {
    long long slot = -1;
    for (long long i = 0; i < PLUM_PROC_MAX_SLOTS; i++) {
        if (!plum_process_slots[i].in_use) { slot = i; break; }
    }
    if (slot < 0) return -1;

    char *args_copy = NULL;
    char **argv = plum_split_args(program, args_joined, argc, &args_copy);
    if (argv == NULL) return -1;
    char *env_copy = NULL;
    char **envp = plum_split_joined(env_joined, envc, &env_copy);

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
    plum_process_slots[slot].child = child;
    return slot;
}

// 1 if the child has exited (its result is now readable), 0 if it is
// still running, -1 if the handle is not a live child.
long long process_poll(long long handle) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL || slot->child == NULL) return -1;
    if (slot->child->finished) return 1;
#if defined(_WIN32)
    HANDLE h = (HANDLE)(intptr_t)slot->child->pid;
    DWORD w = WaitForSingleObject(h, 0);
    if (w == WAIT_TIMEOUT) return 0;
    if (w != WAIT_OBJECT_0) return -1;
    DWORD code = 0;
    if (!GetExitCodeProcess(h, &code)) code = (DWORD)-1;
    plum_child_collect(handle, (long long)(int)code);
    return 1;
#else
    int status = 0;
    pid_t r = waitpid((pid_t)slot->child->pid, &status, WNOHANG);
    if (r == 0) return 0;
    if (r < 0) return -1;
    plum_child_collect(handle, WIFEXITED(status) ? (long long)WEXITSTATUS(status) : -1);
    return 1;
#endif
}

// Waits for the child. A negative `timeout_nanos` waits forever.
// Returns 1 if it exited, 0 if the timeout expired first, -1 on error.
//
// A bounded wait POLLS with a capped backoff rather than using a timed
// wait primitive, because POSIX has no portable one: `waitpid` cannot
// take a deadline, and the alternatives (`sigtimedwait` on SIGCHLD,
// `pidfd_open`) are per-platform and interact with any other signal
// handling in the program. The backoff starts at 1ms and caps at 20ms,
// so a short wait stays responsive and a long one costs about fifty
// wakeups a second. An UNBOUNDED wait does not poll at all -- it blocks
// in `waitpid`, which is the case that would otherwise spin for hours.
long long process_child_wait(long long handle, long long timeout_nanos) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL || slot->child == NULL) return -1;
    if (slot->child->finished) return 1;

    if (timeout_nanos < 0) {
#if defined(_WIN32)
        HANDLE h = (HANDLE)(intptr_t)slot->child->pid;
        if (WaitForSingleObject(h, INFINITE) != WAIT_OBJECT_0) return -1;
        DWORD code = 0;
        if (!GetExitCodeProcess(h, &code)) code = (DWORD)-1;
        plum_child_collect(handle, (long long)(int)code);
        return 1;
#else
        int status = 0;
        pid_t r;
        do { r = waitpid((pid_t)slot->child->pid, &status, 0); } while (r < 0 && errno == EINTR);
        if (r < 0) return -1;
        plum_child_collect(handle, WIFEXITED(status) ? (long long)WEXITSTATUS(status) : -1);
        return 1;
#endif
    }

#if defined(_WIN32)
    {
        HANDLE h = (HANDLE)(intptr_t)slot->child->pid;
        long long ms = timeout_nanos / 1000000LL;
        if (timeout_nanos % 1000000LL != 0) ms++;
        DWORD w = WaitForSingleObject(h, ms > 0x7FFFFFFELL ? 0x7FFFFFFE : (DWORD)ms);
        if (w == WAIT_TIMEOUT) return 0;
        if (w != WAIT_OBJECT_0) return -1;
        DWORD code = 0;
        if (!GetExitCodeProcess(h, &code)) code = (DWORD)-1;
        plum_child_collect(handle, (long long)(int)code);
        return 1;
    }
#else
    {
        long long waited = 0;
        long long step = 1000000LL; // 1ms
        for (;;) {
            long long r = process_poll(handle);
            if (r != 0) return r;
            if (waited >= timeout_nanos) return 0;
            long long left = timeout_nanos - waited;
            long long nap = step < left ? step : left;
            struct timespec req;
            req.tv_sec = (time_t)(nap / 1000000000LL);
            req.tv_nsec = (long)(nap % 1000000000LL);
            while (nanosleep(&req, &req) != 0 && errno == EINTR) { }
            waited += nap;
            step *= 2;
            if (step > 20000000LL) step = 20000000LL;
        }
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
long long process_terminate(long long handle, long long hard) {
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
long long process_child_pid(long long handle) {
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
void process_child_free(long long handle) {
    PlumProcessSlot *slot = plum_slot_of(handle);
    if (slot == NULL) return;
    PlumChildState *c = slot->child;
    if (c != NULL) {
        if (!c->finished) {
            (void)process_terminate(handle, 1);
#if defined(_WIN32)
            HANDLE h = (HANDLE)(intptr_t)c->pid;
            WaitForSingleObject(h, 5000);
            DWORD code = 0;
            if (!GetExitCodeProcess(h, &code)) code = (DWORD)-1;
            plum_child_collect(handle, (long long)(int)code);
#else
            int status = 0;
            pid_t r;
            do { r = waitpid((pid_t)c->pid, &status, 0); } while (r < 0 && errno == EINTR);
            plum_child_collect(handle, (r > 0 && WIFEXITED(status)) ? (long long)WEXITSTATUS(status) : -1);
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
    slot->in_use = 0;
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
    char *cmdline;
    size_t cap = 0;
    size_t len = 0;
    int i;
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    DWORD code = 0;

    for (i = 0; argv[i] != NULL; i++) cap += strlen(argv[i]) * 2 + 3;
    cap += 1;
    cmdline = (char *)malloc(cap);
    if (cmdline == NULL) return -1;
    for (i = 0; argv[i] != NULL; i++) {
        if (plum_win_append_arg(cmdline, cap, &len, argv[i]) != 0) {
            free(cmdline);
            return -1;
        }
    }
    cmdline[len] = '\0';

    // No `STARTF_USESTDHANDLES`: leaving the field unset is what makes
    // the child inherit this process's console, which is the entire
    // point of this function as against `plum_spawn_capture`.
    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);
    ZeroMemory(&pi, sizeof(pi));

    if (!CreateProcessA(NULL, cmdline, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
        free(cmdline);
        return -1;
    }
    CloseHandle(pi.hThread);

    WaitForSingleObject(pi.hProcess, INFINITE);
    if (!GetExitCodeProcess(pi.hProcess, &code)) code = (DWORD)-1;
    CloseHandle(pi.hProcess);
    free(cmdline);
    (void)program;
    return (long long)(int)code;
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
