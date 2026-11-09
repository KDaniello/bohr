/* Sleeps until one second past the wall clock's "now", absolutely, after the wall clock was moved.
 *
 * An absolute CLOCK_REALTIME deadline is a wall time, so it is reached one second of monotonic time
 * later however far the wall clock was stepped beforehand. Exits 0 when between 0.9 and 1.1 s of
 * monotonic time passed, 1 otherwise. Run with a clock_step before 0.1 s (wall_deadline.toml). */
#include <time.h>

static double seconds(clockid_t clock) {
    struct timespec t;
    clock_gettime(clock, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

int main(void) {
    struct timespec pause = {0, 100000000};
    nanosleep(&pause, 0);

    struct timespec until;
    clock_gettime(CLOCK_REALTIME, &until);
    until.tv_sec += 1;
    double before = seconds(CLOCK_MONOTONIC);
    clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &until, 0);
    double slept = seconds(CLOCK_MONOTONIC) - before;
    return slept > 0.9 && slept < 1.1 ? 0 : 1;
}
