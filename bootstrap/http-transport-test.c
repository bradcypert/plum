/* Deterministic fault injection for Http's count-based transport boundary.
 * Only net-smoke's test copy of Http binds these symbols. */
#include <string.h>

static unsigned char input[131072], output[131072];
static long long input_len, input_pos, output_len, split_at, write_limit;
static long long read_error, write_stop, largest_read;

void http_test_reset(long long split, long long limit, long long fail_read,
                     long long stop_write) {
    input_len = input_pos = output_len = largest_read = 0;
    split_at = split;
    write_limit = limit;
    read_error = fail_read;
    write_stop = stop_write;
}
void http_test_input(const char *p, long long n) {
    if (n >= 0 && n <= (long long)sizeof(input)) {
        memcpy(input, p, (size_t)n);
        input_len = n;
    }
}
long long http_test_recv(long long fd, long long max_len) {
    (void)fd;
    if (max_len > largest_read) largest_read = max_len;
    if (input_pos == input_len) return read_error ? -1 : 0;
    long long n = input_len - input_pos;
    if (n > max_len) n = max_len;
    if (input_pos < split_at && input_pos + n > split_at) n = split_at - input_pos;
    input_pos += n;
    return n;
}
/* The native API returns the START of its last chunk, not the next byte. */
static long long last_n;
long long http_test_recv_n(long long fd, long long max_len) {
    last_n = http_test_recv(fd, max_len);
    return last_n;
}
const char *http_test_recv_data(void) {
    return (const char *)input + input_pos - (last_n > 0 ? last_n : 0);
}
long long http_test_send(long long fd, const char *p, long long len) {
    (void)fd;
    if (write_stop >= 0 && output_len >= write_stop) return write_stop == 0 ? 0 : -1;
    long long n = len < write_limit ? len : write_limit;
    if (write_stop > 0 && n > write_stop - output_len) n = write_stop - output_len;
    if (n < 0 || output_len + n > (long long)sizeof(output)) return -1;
    memcpy(output + output_len, p, (size_t)n);
    output_len += n;
    return n;
}
long long http_test_written(void) { return output_len; }
const char *http_test_output(void) { return (const char *)output; }
long long http_test_largest_read(void) { return largest_read; }

/* Allocation volume is sampled before function exit. This distinguishes
 * sharing an outgoing payload from building a second whole-message copy. */
extern long long plum_b_alloc;
long long http_test_allocated_bytes(void) { return plum_b_alloc; }
const char *http_test_large_body(void) {
    memset(input, 'a', 65536);
    return (const char *)input;
}
