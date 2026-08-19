// Blocking reads from standard input, for the language server.
//
// The runtime can already WRITE (printf) and read whole FILES, but had
// no way to read stdin at all — which is what an LSP server does for a
// living.
//
// **The buffers are owned HERE**, reused across calls and valid until
// the next call to the same function. The obvious alternative — malloc
// and hand ownership to the caller — does not work portably: the real
// compiler materializes an extern's `CStr` RETURN as a Plum string, so
// the pointer cannot be handed back to a `free`-shaped extern, and
// Plum has no `free` of its own. Caller-owned buffers would therefore
// leak one allocation per message in a process designed to run for
// hours.
//
// Not thread-safe, and does not need to be: a language server reads its
// requests on one thread, in order.
//
// Deliberately blocking and synchronous. A language server is a request
// loop: read one message, answer it, read the next. Nothing here needs
// to be asynchronous, and pretending otherwise would mean inventing a
// concurrency story for a problem that does not have one.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *line_buf = NULL;
static size_t line_cap = 0;
static char *body_buf = NULL;
static size_t body_cap = 0;

static int ensure(char **buf, size_t *cap, size_t need) {
    if (*cap >= need) return 1;
    size_t next = *cap ? *cap : 256;
    while (next < need) next *= 2;
    char *bigger = (char *)realloc(*buf, next);
    if (!bigger) return 0;
    *buf = bigger;
    *cap = next;
    return 1;
}

// One line, without its newline. Returns "" at EOF — which is how the
// server learns its client has gone away, since it cannot distinguish a
// null pointer from an empty string once the value crosses into Plum.
const char *stdin_read_line(void) {
    size_t len = 0;
    if (!ensure(&line_buf, &line_cap, 256)) return "";
    for (;;) {
        int c = fgetc(stdin);
        if (c == EOF) break;
        if (c == '\n') break;
        if (!ensure(&line_buf, &line_cap, len + 2)) return "";
        line_buf[len++] = (char)c;
    }
    // LSP framing is CRLF; the caller only ever wants the content.
    if (len > 0 && line_buf[len - 1] == '\r') len--;
    line_buf[len] = '\0';
    return line_buf;
}

// Exactly `n` bytes, or "" if the stream ended early. An LSP message
// body is length-prefixed, so a short read is a protocol error rather
// than something to paper over.
const char *stdin_read_n(long long n) {
    if (n < 0) return "";
    if (!ensure(&body_buf, &body_cap, (size_t)n + 1)) return "";
    size_t got = fread(body_buf, 1, (size_t)n, stdin);
    if (got != (size_t)n) return "";
    body_buf[n] = '\0';
    return body_buf;
}

// Writes without a trailing newline, which `println` cannot do and an
// LSP header needs.
void stdout_write(const char *s) { fputs(s, stdout); }

// stdout is a pipe here, not a terminal, so it is block-buffered: a
// response written but not flushed leaves the client waiting forever
// while the server waits for the next request. The one deadlock this
// design can produce, and the one line that prevents it.
void stdout_flush(void) { fflush(stdout); }
