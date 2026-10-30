// Reads the processor's cycle counter directly, which no syscall interception can see on its own.
// Present so that a guest reaching past the kernel for time is covered by the same guarantee as one
// that asks politely.
#include <stdint.h>
#include <unistd.h>
#include <x86intrin.h>

int main(void) {
    uint64_t first = __rdtsc();
    uint64_t second = __rdtsc();

    static const char DIGITS[] = "0123456789abcdef";
    char line[40];
    char *p = line;
    for (int half = 0; half < 2; half++) {
        uint64_t v = half ? second : first;
        for (int i = 15; i >= 0; i--) {
            p[i] = DIGITS[v & 0xf];
            v >>= 4;
        }
        p += 16;
        *p++ = half ? '\n' : ' ';
    }
    return write(1, line, (size_t)(p - line)) > 0 ? 0 : 1;
}
