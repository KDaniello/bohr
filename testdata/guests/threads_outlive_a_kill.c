/* Two threads, both alive and both waiting, for long enough that a scheduled kill lands on them.
 *
 * The shape every service has and the one this engine has to survive taking away: a node is more
 * than one task, and killing it has to end all of them and leave the supervisor able to carry on
 * with whatever else was running. A guest that finished before the kill fired would prove nothing,
 * so both threads outlast any tick a scenario is likely to name.
 *
 * Sleeping rather than spinning, because sleeping costs virtual time and no real time: the run is
 * as long as the scenario says and as cheap as an empty loop. */

#include <pthread.h>
#include <stdio.h>
#include <time.h>

#define ROUNDS 2000

static void rest(void) {
    struct timespec ms = {0, 1000000};
    nanosleep(&ms, 0);
}

static void *waiting(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        rest();
    }
    return 0;
}

int main(void) {
    pthread_t other;
    if (pthread_create(&other, 0, waiting, 0) != 0) {
        return 1;
    }
    for (int i = 0; i < ROUNDS; i++) {
        rest();
    }
    pthread_join(other, 0);
    printf("both threads finished\n");
    return 0;
}
