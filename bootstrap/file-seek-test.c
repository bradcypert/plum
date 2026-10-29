// Exercise the actual platform adapter without allocating or writing a
// multi-gigabyte file. tmpfile is binary on Windows as well as POSIX.
#include "../native_stdlib/file_shim.c"
#include <assert.h>

int main(void) {
    FILE *f = tmpfile();
    assert(f != NULL);
    slots[0] = f;
    assert(file_write(1, "abc", 3) == 3);
    const long long large = 4294967313LL;
    assert(file_seek(1, 2147483648LL, 0) == 2147483648LL);
    assert(file_seek(1, large, 0) == large);
    assert(file_seek(1, 0, 1) == large);
    assert(file_seek(1, -large + 1, 1) == 1);
    assert(file_seek(1, large, 2) == large + 3);
    assert(file_seek(1, -large, 1) == 3);
    assert(file_seek(1, -1, 2) == 2);
    assert(fgetc(f) == 'c');
    assert(file_seek(1, 0, 2) == 3); // seeking did not extend the file
    assert(file_seek(1, -1, 0) == -1);
    assert(file_seek(1, -4, 2) == -1);
    assert(file_seek(1, 0, 3) == -1);
    assert(file_seek(0, 0, 0) == -1);
    assert(file_seek(257, 0, 0) == -1);
    assert(file_close_checked(1) == 0);
    assert(file_seek(1, 0, 0) == -1);
    puts("native file seek: ok");
    return 0;
}
