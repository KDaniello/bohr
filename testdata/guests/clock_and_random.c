// Reaches for the two things a determinism tool must take away: the clock and the system's
// randomness. Writes both so that the run's output is what reproducibility is judged on.
//
// Deliberately avoids stdio. Buffered output pulls in a startup path of its own, and the point of
// this guest is the two calls in the middle, not the library around them.
#define _GNU_SOURCE
#include <stdint.h>
#include <string.h>
#include <sys/random.h>
#include <time.h>
#include <unistd.h>

static char *put_hex(char *out, uint64_t value, int digits) {
    static const char DIGITS[] = "0123456789abcdef";
    for (int i = digits - 1; i >= 0; i--) {
        out[i] = DIGITS[value & 0xf];
        value >>= 4;
    }
    return out + digits;
}

int main(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);

    unsigned char entropy[16];
    ssize_t got = getrandom(entropy, sizeof entropy, 0);

    char line[128];
    char *p = line;
    p = put_hex(p, (uint64_t)ts.tv_sec, 16);
    *p++ = '.';
    p = put_hex(p, (uint64_t)ts.tv_nsec, 16);
    *p++ = ' ';
    for (ssize_t i = 0; i < got; i++) {
        p = put_hex(p, entropy[i], 2);
    }
    *p++ = '\n';

    ssize_t written = write(1, line, (size_t)(p - line));
    return written > 0 ? 0 : 1;
}
