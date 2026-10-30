// Sleeps for a day, in two halves, and reports the clock either side. Virtual time is the point:
// the guest experiences the wait and the run does not, so a test that takes a day to fail finishes
// in the time its own code takes to execute.
#include <stdio.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    struct timespec before, after, half = { .tv_sec = 12 * 60 * 60, .tv_nsec = 0 };
    clock_gettime(CLOCK_MONOTONIC, &before);
    nanosleep(&half, 0);
    nanosleep(&half, 0);
    clock_gettime(CLOCK_MONOTONIC, &after);

    char line[64];
    int n = snprintf(line, sizeof line, "%lld\n", (long long)(after.tv_sec - before.tv_sec));
    return write(1, line, (size_t)n) == n ? 0 : 1;
}
