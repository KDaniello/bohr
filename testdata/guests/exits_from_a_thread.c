// A process that ends with a failure from a thread that is not its first.
//
// The shape a Go program fails in: whichever thread notices the fatal error calls `exit_group`,
// and that is seldom the one the process started on. Linux takes every thread down and each
// reports its own end; the status the process failed with has to reach the verdict however those
// reports arrive, and in the same order on every run.
//
// The other threads block rather than spin, so where each one is when the process ends is not a
// race the guest has to win for the shape to appear.

#include <pthread.h>
#include <stdlib.h>
#include <unistd.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;

static void *waits_for_nothing(void *ignored) {
    (void)ignored;
    pthread_mutex_lock(&lock);
    while (1) {
        pthread_cond_wait(&never, &lock);
    }
    return NULL;
}

static void *fails(void *ignored) {
    (void)ignored;
    usleep(1000);
    exit(3);
}

int main(void) {
    pthread_t threads[4];
    for (int i = 0; i < 3; i++) {
        if (pthread_create(&threads[i], NULL, waits_for_nothing, NULL) != 0) {
            return 1;
        }
    }
    if (pthread_create(&threads[3], NULL, fails, NULL) != 0) {
        return 1;
    }
    pthread_join(threads[3], NULL);
    return 0;
}
