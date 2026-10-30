#include <stdio.h>
#include <string.h>
#if defined(_WIN32)
#include <io.h>
#include <fcntl.h>
#endif
void emit_capture(void) {
#if defined(_WIN32)
    // Avoid text-mode newline translation in this byte-exact fixture.
    _setmode(1, _O_BINARY);
    _setmode(2, _O_BINARY);
#endif
    char block[4096];
    memset(block, 'a', sizeof(block));
    for (int i = 0; i < 256; i++) fwrite(block, 1, sizeof(block), stdout);
    fwrite("b\0c\n", 1, 4, stdout);
    fwrite("e\0r\r\n", 1, 5, stderr);
}
