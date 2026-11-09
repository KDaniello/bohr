// A lease measured on the wall clock.
//
// The holder takes a ten-second lease and keeps it while CLOCK_REALTIME says the term has not run
// out. CLOCK_MONOTONIC is what it is checked against: a lease still believed held once ten seconds
// of monotonic time have passed has outlived its term, which is the window in which another holder
// could have taken it. Only a wall clock set back opens that window. Skew and drift move both
// clocks together, and a wall clock set forward ends the lease early, which is safe.
#include <stdint.h>
#include <time.h>

enum { TERM_SECONDS = 10 };

static int64_t nanos(clockid_t clock) {
    struct timespec t;
    clock_gettime(clock, &t);
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

int main(void) {
    const int64_t term = (int64_t)TERM_SECONDS * 1000000000;
    // A millisecond of slack: the two clocks are read by two calls, and each call costs time.
    const int64_t slack = 1000000;
    const int64_t wall_start = nanos(CLOCK_REALTIME);
    const int64_t mono_start = nanos(CLOCK_MONOTONIC);
    const struct timespec beat = {0, 100000000};

    for (;;) {
        nanosleep(&beat, 0);
        if (nanos(CLOCK_REALTIME) >= wall_start + term) {
            return 0;
        }
        if (nanos(CLOCK_MONOTONIC) - mono_start > term + slack) {
            return 1;
        }
    }
}
