// A process that ends while its other threads are still alive.
//
// The ordinary shape of a threaded program stopping: the main thread calls `exit_group` and the
// rest are taken down by the kernel wherever they happen to be. The world declares the node gone
// on the call that made it, and the backend goes on reporting each thread's own exit afterwards --
// which the kernel read as an exit for a task nobody had created, and refused the run over.
// Measured on `solana-test-validator`, which ends this way with twenty-seven threads.
//
// The threads block rather than spin, so which of them is where when the process ends is not a
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

int main(void) {
    pthread_t threads[3];
    for (int i = 0; i < 3; i++) {
        if (pthread_create(&threads[i], NULL, waits_for_nothing, NULL) != 0) {
            return 1;
        }
    }
    // Long enough for every thread to reach the wait, in a world where sleeping costs no real
    // time; short enough that a run of this guest is still one event stream and not a wait.
    usleep(1000);
    return 0;
}
