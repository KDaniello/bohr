/* One thread that gives up its turn, and one that never stops asking the time.
 *
 * The shape a runtime has while it is starting: a worker with nothing to do yet says so, and the
 * thread setting it up waits for a moment to arrive. A scheduler that takes the first at its word
 * and never comes back to it leaves the second waiting for something only the first can do, and the
 * run spends the rest of its budget reading a clock.
 *
 * Each thread reports how many turns it got. What is under test is that the second number is not
 * zero — the yielding thread has to be scheduled again, whatever the policy does with it in
 * between. */

#include <pthread.h>
#include <stdio.h>
#include <sched.h>
#include <time.h>

#define ROUNDS 2000

static volatile long yielded_turns;
static volatile long spun_turns;

static void *yielding(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        sched_yield();
        yielded_turns++;
    }
    return 0;
}

int main(void) {
    pthread_t other;
    if (pthread_create(&other, 0, yielding, 0) != 0) {
        return 1;
    }
    struct timespec now;
    for (int i = 0; i < ROUNDS * 20; i++) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        spun_turns++;
    }
    pthread_join(other, 0);
    printf("yielded %ld spun %ld\n", yielded_turns, spun_turns);
    return yielded_turns == 0 ? 1 : 0;
}
