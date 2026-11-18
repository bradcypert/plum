#include <stdint.h>

int8_t ffi_echo_i8(int8_t x) { return x; }
int64_t ffi_widen_i8(int8_t x) { return x; }
int8_t ffi_min_i8(void) { return INT8_MIN; }
int8_t ffi_max_i8(void) { return INT8_MAX; }

int16_t ffi_echo_i16(int16_t x) { return x; }
int64_t ffi_widen_i16(int16_t x) { return x; }
int16_t ffi_min_i16(void) { return INT16_MIN; }
int16_t ffi_max_i16(void) { return INT16_MAX; }

int32_t ffi_echo_i32(int32_t x) { return x; }
int64_t ffi_widen_i32(int32_t x) { return x; }
int32_t ffi_min_i32(void) { return INT32_MIN; }
int32_t ffi_max_i32(void) { return INT32_MAX; }

int64_t ffi_echo_int(int64_t x) { return x; }
int64_t ffi_widen_int(int64_t x) { return x; }
int64_t ffi_min_int(void) { return INT64_MIN; }
int64_t ffi_max_int(void) { return INT64_MAX; }

int64_t ffi_echo_i64(int64_t x) { return x; }
int64_t ffi_widen_i64(int64_t x) { return x; }
int64_t ffi_min_i64(void) { return INT64_MIN; }
int64_t ffi_max_i64(void) { return INT64_MAX; }

uint8_t ffi_echo_u8(uint8_t x) { return x; }
uint64_t ffi_widen_u8(uint8_t x) { return x; }
uint8_t ffi_min_u8(void) { return 0; }
uint8_t ffi_max_u8(void) { return UINT8_MAX; }

uint16_t ffi_echo_u16(uint16_t x) { return x; }
uint64_t ffi_widen_u16(uint16_t x) { return x; }
uint16_t ffi_min_u16(void) { return 0; }
uint16_t ffi_max_u16(void) { return UINT16_MAX; }

uint32_t ffi_echo_u32(uint32_t x) { return x; }
uint64_t ffi_widen_u32(uint32_t x) { return x; }
uint32_t ffi_min_u32(void) { return 0; }
uint32_t ffi_max_u32(void) { return UINT32_MAX; }

uint64_t ffi_echo_u64(uint64_t x) { return x; }
uint64_t ffi_widen_u64(uint64_t x) { return x; }
uint64_t ffi_min_u64(void) { return 0; }
uint64_t ffi_max_u64(void) { return UINT64_MAX; }

/* Keep this fixture freestanding: no dependency on a hosted string header. */
static int ffi_text_matches(const char *text) {
    const char expected[] = "scalar";
    for (unsigned i = 0; i < sizeof expected; ++i) {
        if (text[i] != expected[i]) return 0;
    }
    return 1;
}

uint64_t ffi_mixed(uint8_t a, int16_t b, uint64_t c, int8_t d, uint32_t e, int32_t f, uint16_t g, int64_t h, int flag, double scale, const char *text) {
    return a == UINT8_MAX && b == INT16_MIN && c == UINT64_MAX && d == INT8_MIN
        && e == UINT32_MAX && f == INT32_MIN && g == UINT16_MAX && h == INT64_MIN
        && flag == 1 && scale == 1.5 && ffi_text_matches(text) ? c : 0;
}
uint8_t ffi_byte(const char *text) { return (uint8_t)text[0]; }
const char *ffi_pointer(const char *text) { return text; }
uint8_t ffi_raw_byte(const char *text) { return (uint8_t)text[0]; }
const char *ffi_greeting(void) { return "native"; }
int ffi_truth(void) { return 2; }
int16_t ffi_bool(int flag) { return flag ? -123 : 123; }
void ffi_void(void) { }
void ffi_implicit_void(void) { }
