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

#if defined(_WIN32)
#include <windows.h>
#else
#include <unistd.h>
#include <sys/wait.h>
#endif

#define PLUM_PROC_PATH_MAX 4096

#define PLUM_PROC_ARG_SEP '\t'
#define PLUM_PROC_MAX_SLOTS 256

typedef struct {
    int in_use;
    long long exit_code;
    char *out_data;
    char *err_data;
} PlumProcessSlot;

static PlumProcessSlot plum_process_slots[PLUM_PROC_MAX_SLOTS];

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

// Runs `argv` to completion with stdout and stderr captured into two
// fresh temp files, whose paths are written into the caller's buffers.
// Returns the child's exit code, or `PLUM_SPAWN_FAILED` if the child
// could never be started -- in which case no temp files are left behind
// and the path buffers hold nothing worth reading.
//
// This is the ONLY platform-specific part of this shim. Everything
// around it is shared.

#if !defined(_WIN32)

static long long plum_spawn_capture(const char *program, char **argv, char *out_path, char *err_path) {
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

    pid_t pid = fork();
    if (pid < 0) {
        close(out_fd);
        close(err_fd);
        unlink(out_path);
        unlink(err_path);
        return PLUM_SPAWN_FAILED;
    }
    if (pid == 0) {
        dup2(out_fd, STDOUT_FILENO);
        dup2(err_fd, STDERR_FILENO);
        close(out_fd);
        close(err_fd);
        execvp(program, argv);
        _exit(127); // only reached if execvp itself failed
    }

    close(out_fd);
    close(err_fd);
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

static long long plum_spawn_capture(const char *program, char **argv, char *out_path, char *err_path) {
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

    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    si.hStdOutput = ho;
    si.hStdError = he;
    ZeroMemory(&pi, sizeof(pi));

    // `lpApplicationName` is NULL so that `CreateProcess` searches PATH
    // and appends `.exe`, which is what makes `clang` resolve the same
    // way it does everywhere else. The ambiguity that normally makes
    // that risky -- an unquoted program path containing spaces -- does
    // not apply, because the program was quoted above like any other
    // argument.
    if (!CreateProcessA(NULL, cmdline, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
        CloseHandle(ho);
        CloseHandle(he);
        DeleteFileA(out_path);
        DeleteFileA(err_path);
        free(cmdline);
        return PLUM_SPAWN_FAILED;
    }

    WaitForSingleObject(pi.hProcess, INFINITE);
    if (!GetExitCodeProcess(pi.hProcess, &code)) code = (DWORD)-1;
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);

    // Closed only after the child has exited, so everything it wrote is
    // flushed and the files can be read back in full.
    CloseHandle(ho);
    CloseHandle(he);
    free(cmdline);
    (void)program;
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

    char *args_copy = strdup(args_joined);
    char **argv = malloc(sizeof(char *) * (size_t)(argc + 2));
    if (args_copy == NULL || argv == NULL) {
        free(args_copy);
        free(argv);
        return -1;
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

    char out_path[PLUM_PROC_PATH_MAX];
    char err_path[PLUM_PROC_PATH_MAX];
    long long code = plum_spawn_capture(program, argv, out_path, err_path);
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
    free(plum_process_slots[handle].out_data);
    free(plum_process_slots[handle].err_data);
    plum_process_slots[handle].in_use = 0;
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
    WaitForSingleObject(pi.hProcess, INFINITE);
    if (!GetExitCodeProcess(pi.hProcess, &code)) code = (DWORD)-1;
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
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
