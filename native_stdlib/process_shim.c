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
// **Unix-only (Linux/macOS)** — same documented v1 scope as every
// other native_stdlib shim.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

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

    char out_path[] = "/tmp/plum_proc_out_XXXXXX";
    char err_path[] = "/tmp/plum_proc_err_XXXXXX";
    int out_fd = mkstemp(out_path);
    int err_fd = (out_fd >= 0) ? mkstemp(err_path) : -1;
    if (out_fd < 0 || err_fd < 0) {
        if (out_fd >= 0) {
            close(out_fd);
            unlink(out_path);
        }
        free(args_copy);
        free(argv);
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        close(out_fd);
        close(err_fd);
        unlink(out_path);
        unlink(err_path);
        free(args_copy);
        free(argv);
        return -1;
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

    plum_process_slots[slot].in_use = 1;
    plum_process_slots[slot].exit_code = WIFEXITED(status) ? (long long)WEXITSTATUS(status) : -1;
    plum_process_slots[slot].out_data = plum_read_whole_file_or_empty(out_path);
    plum_process_slots[slot].err_data = plum_read_whole_file_or_empty(err_path);

    unlink(out_path);
    unlink(err_path);
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
