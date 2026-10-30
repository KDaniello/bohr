/* Two threads incrementing a shared counter without a lock.
 *
 * The final value depends on how the increments interleave, which the scheduler decides
 * from the tape — so under Bohr the answer is one fixed number per seed, and printing it
 * is what a determinism test compares. Native, the value varies run to run. */
#include <pthread.h>
#include <stdio.h>

enum { ITERS = 100000 };

static volatile long counter;

static void *bump(void *arg) {
    (void)arg;
    for (int i = 0; i < ITERS; i++)
        counter++;
    return NULL;
}

int main(void) {
    pthread_t a, b;
    pthread_create(&a, NULL, bump, NULL);
    pthread_create(&b, NULL, bump, NULL);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("%ld\n", counter);
    return 0;
}
