// A tight loop of the cheapest possible syscall, which is where interception costs the most: every
// iteration is one trap out and one answer back, with nothing else to amortise it against. The
// slowdown measured here is the worst case the tool has.
//
// The count is high enough that process startup, which both the native and the traced side pay, is
// a rounding error against the loop. At a lower count the ratio measures how long `execve` took.
#include <unistd.h>

#define ITERATIONS 200000

int main(void) {
    pid_t last = 0;
    for (int i = 0; i < ITERATIONS; i++) {
        last = getpid();
    }
    return last > 0 ? 0 : 1;
}
