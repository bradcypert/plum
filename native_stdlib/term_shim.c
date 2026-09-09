// The terminal platform layer: is this a terminal, how big is it, and
// writing to it (issue #3).
//
// Deliberately the SMALL half of that issue. Turning input bytes into
// semantic key events is #35, and it is pure Plum with no C in it at
// all -- which is exactly why it is not here. What needs a shim is what
// cannot be done from Plum: asking the operating system a question
// about a file descriptor.
//
// Same ABI conventions as its neighbours: every function takes and
// returns only `long long`, or a `const char *` for text.
//
// **The size is a QUERY then two READS**, the same split
// `tcp_recv_n`/`tcp_recv_data` and `file_read_n`/`file_read_data` use,
// and for the same reason: Plum's extern surface has no multi-value
// return, and a terminal size is two numbers that must come from ONE
// observation. Asking for the width and the height separately would
// let a resize land between them and report a size the terminal never
// had.

#include <stdio.h>

#if defined(_WIN32)
#include <windows.h>
#include <io.h>
#else
#include <unistd.h>
#include <sys/ioctl.h>
#endif

static long long plum_term_cols = 0;
static long long plum_term_rows = 0;

// 0 stdin, 1 stdout, 2 stderr -- the numbers Plum's `Stream` enum maps
// to. Anything else is not a terminal, which is the safe answer.
long long term_is_tty(long long which) {
    if (which < 0 || which > 2) return 0;
#if defined(_WIN32)
    return _isatty((int)which) ? 1 : 0;
#else
    return isatty((int)which) ? 1 : 0;
#endif
}

// Asks the terminal how big it is, storing the answer for the two
// readers below. Returns 0 on success and -1 when there is no terminal
// to ask -- output redirected to a file, for instance, which is not an
// error the caller did anything wrong to cause.
//
// Measured on STDOUT, not stdin: the size that matters is the one of
// the thing being drawn to. A program whose stdin is a pipe and whose
// stdout is a terminal still has a size worth knowing.
long long term_size_query(void) {
#if defined(_WIN32)
    CONSOLE_SCREEN_BUFFER_INFO info;
    HANDLE h = GetStdHandle(STD_OUTPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    if (!GetConsoleScreenBufferInfo(h, &info)) return -1;
    // The WINDOW, not the buffer. A Windows console's buffer is
    // routinely taller than the window -- that is what the scrollback
    // is -- so `dwSize` would report a height nobody can see.
    plum_term_cols = (long long)(info.srWindow.Right - info.srWindow.Left + 1);
    plum_term_rows = (long long)(info.srWindow.Bottom - info.srWindow.Top + 1);
#else
    struct winsize ws;
    if (ioctl(STDOUT_FILENO, TIOCGWINSZ, &ws) != 0) return -1;
    plum_term_cols = (long long)ws.ws_col;
    plum_term_rows = (long long)ws.ws_row;
#endif
    // A terminal reporting zero is a terminal that does not know, and a
    // caller dividing by it would be worse off than one told no.
    if (plum_term_cols <= 0 || plum_term_rows <= 0) return -1;
    return 0;
}

long long term_size_cols(void) { return plum_term_cols; }
long long term_size_rows(void) { return plum_term_rows; }

// Writes to stdout with NO trailing newline and NO flush.
//
// Both omissions are the point. A terminal program positions its own
// output with escape sequences, so an automatic newline would corrupt
// every frame it draws; and flushing per write turns one redraw into
// hundreds of syscalls. `term_flush` is separate so a caller can build
// a frame and publish it in one go.
//
// Returns -1 if the write failed -- a closed pipe, most likely, which a
// long-running program should notice rather than draw into forever.
long long term_write(const char *s) {
    if (s == NULL) return 0;
    if (fputs(s, stdout) == EOF) return -1;
    return 0;
}

long long term_flush(void) { return fflush(stdout) == 0 ? 0 : -1; }
