#include <string.h>
long long edit_text(unsigned char *data, long long capacity) {
    if (capacity < 4) return -1;
    memcpy(data, "h\xc3\xa9", 4);
    return 7;
}
long long fill_binary(unsigned char *data, long long capacity) {
    if (capacity < 3) return -1;
    data[0] = 65; data[1] = 0; data[2] = 66;
    return 3;
}
long long no_terminator(unsigned char *data, long long capacity) {
    memset(data, 'x', (size_t)capacity);
    return 0;
}
long long last_terminator(unsigned char *data, long long capacity) {
    if (capacity <= 0) return -1;
    memset(data, 'x', (size_t)capacity);
    data[capacity - 1] = 0;
    return 9;
}
long long invalid_utf8(unsigned char *data, long long capacity) {
    if (capacity < 1) return -1;
    data[0] = 255;
    return 1;
}
