// A lost update between two threads: each reads the counter, adds one, and writes it back, with no
// lock and no atomic. An interleaving that puts the second read between the first read and the
// first write loses an increment, and the total comes out short.
//
// The syscall in the loop is not scenery. Interception switches tasks at syscall boundaries, so a
// loop that never enters the kernel is a loop the scheduler cannot interleave; this is the shape of
// real code that races, which logs, polls or yields between the steps that conflict.
#include <sched.h>
#include <stdio.h>
#include <pthread.h>
#include <unistd.h>

#define STEPS 64
#define THREADS 2

static volatile long counter;

static void *bump(void *arg) {
    (void)arg;
    for (int i = 0; i < STEPS; i++) {
        long seen = counter;
        sched_yield();
        counter = seen + 1;
    }
    return 0;
}

int main(void) {
    pthread_t other;
    if (pthread_create(&other, 0, bump, 0) != 0) {
        return 2;
    }
    bump(0);
    pthread_join(other, 0);

    char line[64];
    int n = snprintf(line, sizeof line, "counter=%ld expected=%d\n", counter, STEPS * THREADS);
    if (write(1, line, (size_t)n) != n) {
        return 2;
    }
    // The defect is the point of this program, so losing an update is how it reports success at
    // being broken: a non-zero exit is what the oracle turns into a finding.
    return counter == STEPS * THREADS ? 0 : 1;
}
