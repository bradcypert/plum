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
#include <termios.h>
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

// --- Raw mode, the alternate screen, and the cursor ---
//
// Each of these is entered by a call that SAVES what it is about to
// change, and left by one that puts it back. On the Plum side each is a
// `handle`, so leaving happens when the value dies -- including on a
// panic, which is what issue #1 added and what a terminal program needs
// more than most: a crash that leaves a terminal in raw mode with no
// cursor is a crash that also takes the user's shell with it.
//
// --- Why entering is COUNTED rather than refused ---
//
// Entering twice must not save the already-changed state as the thing
// to restore, or leaving puts the terminal back to raw. The first entry
// saves and applies; later ones only count; the last leave restores.
// Counting rather than refusing means a library and its caller can both
// ask without knowing about each other, and it is order-independent,
// which matters because handles in different scopes need not die in the
// order they were born.

static int plum_raw_depth = 0;
static int plum_alt_depth = 0;
static int plum_cursor_depth = 0;

#if !defined(_WIN32)
static struct termios plum_saved_termios;
#else
static DWORD plum_saved_in_mode = 0;
static DWORD plum_saved_out_mode = 0;
#endif

// Returns 0 on success, -1 if there is no terminal to change.
//
// **`ISIG` is cleared, so Ctrl+C arrives as a BYTE (0x03) rather than a
// signal.** That is what raw mode means, and here it is also what makes
// cleanup work: the default action for SIGINT terminates the process
// WITHOUT running `atexit` handlers, so a Ctrl+C in raw mode with
// signals still enabled would leave the terminal raw and the cursor
// hidden. Delivered as a byte, the program can exit through `exit()`,
// which restores everything on the way out. A raw-mode program is
// responsible for noticing 0x03 and quitting.
//
// `VMIN=1, VTIME=0`: a read blocks until at least one byte. The timeout
// belongs to `poll`, which is what `Os.read_stdin_timeout` already
// uses; asking termios for a timeout as well would give two mechanisms
// racing over one deadline.
long long term_enter_raw(void) {
#if defined(_WIN32)
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    if (plum_raw_depth == 0) {
        if (!GetConsoleMode(h, &plum_saved_in_mode)) return -1;
        DWORD mode = plum_saved_in_mode;
        mode &= ~(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
        // The reason one decoder can serve every platform: with this
        // set, the console delivers the same VT escape sequences a
        // POSIX terminal does, instead of INPUT_RECORD structures.
        mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;
        if (!SetConsoleMode(h, mode)) return -1;
    }
    plum_raw_depth++;
    return 0;
#else
    if (!isatty(STDIN_FILENO)) return -1;
    if (plum_raw_depth == 0) {
        if (tcgetattr(STDIN_FILENO, &plum_saved_termios) != 0) return -1;
        struct termios raw = plum_saved_termios;
        raw.c_iflag &= (tcflag_t)~(IXON | ICRNL | BRKINT | INPCK | ISTRIP);
        raw.c_oflag &= (tcflag_t)~(OPOST);
        raw.c_lflag &= (tcflag_t)~(ECHO | ICANON | IEXTEN | ISIG);
        raw.c_cflag |= (tcflag_t)CS8;
        raw.c_cc[VMIN] = 1;
        raw.c_cc[VTIME] = 0;
        if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw) != 0) return -1;
    }
    plum_raw_depth++;
    return 0;
#endif
}

// Takes no argument it uses: a `handle`'s cleanup is called with the
// number the cell holds, and these have no per-instance state -- the
// depth counter is what decides whether this one is the last.
void term_leave_raw(long long ignored) {
    (void)ignored;
    if (plum_raw_depth <= 0) return;
    plum_raw_depth--;
    if (plum_raw_depth > 0) return;
#if defined(_WIN32)
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h != INVALID_HANDLE_VALUE && h != NULL) SetConsoleMode(h, plum_saved_in_mode);
#else
    tcsetattr(STDIN_FILENO, TCSAFLUSH, &plum_saved_termios);
#endif
}

// Windows needs telling that escape sequences written to stdout are
// escape sequences. POSIX terminals need no equivalent.
#if defined(_WIN32)
static int plum_enable_vt_output(void) {
    HANDLE h = GetStdHandle(STD_OUTPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    if (!GetConsoleMode(h, &plum_saved_out_mode)) return -1;
    return SetConsoleMode(h, plum_saved_out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) ? 0 : -1;
}
#endif

// `\033[?1049h` and `?1049l`: the xterm alternate screen. Entering
// leaves the user's scrollback untouched and leaving puts their shell
// back exactly as it was, which is why a full-screen program should use
// it rather than clearing.
//
// Flushed immediately, unlike `term_write`. A mode change that has not
// reached the terminal yet is a mode change that has not happened, and
// everything drawn after it would land on the wrong screen.
long long term_enter_alt(void) {
    // Refused when stdout is not a terminal. Escape sequences written
    // into a file or a pipe are not invisible -- they are corruption,
    // and the caller gets an `Err` they can act on instead.
    if (!term_is_tty(1)) return -1;
    if (plum_alt_depth == 0) {
#if defined(_WIN32)
        if (plum_enable_vt_output() != 0) return -1;
#endif
        if (fputs("\033[?1049h", stdout) == EOF) return -1;
        if (fflush(stdout) != 0) return -1;
    }
    plum_alt_depth++;
    return 0;
}

void term_leave_alt(long long ignored) {
    (void)ignored;
    if (plum_alt_depth <= 0) return;
    plum_alt_depth--;
    if (plum_alt_depth > 0) return;
    fputs("\033[?1049l", stdout);
    fflush(stdout);
}

long long term_hide_cursor(void) {
    if (!term_is_tty(1)) return -1;
    if (plum_cursor_depth == 0) {
#if defined(_WIN32)
        if (plum_enable_vt_output() != 0) return -1;
#endif
        if (fputs("\033[?25l", stdout) == EOF) return -1;
        if (fflush(stdout) != 0) return -1;
    }
    plum_cursor_depth++;
    return 0;
}

void term_show_cursor(long long ignored) {
    (void)ignored;
    if (plum_cursor_depth <= 0) return;
    plum_cursor_depth--;
    if (plum_cursor_depth > 0) return;
    fputs("\033[?25h", stdout);
    fflush(stdout);
}
