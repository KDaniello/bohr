// A process killed by a fault while its other threads are busy calling into the world.
//
// The kernel ends every thread of the group, and the host reports their deaths in an order of its
// own; the trace must show them in one order on every run.

#include <pthread.h>
#include <unistd.h>

static void *calls_forever(void *ignored) {
    (void)ignored;
    while (1) {
        getpid();
    }
    return NULL;
}

int main(void) {
    pthread_t threads[4];
    for (int i = 0; i < 4; i++) {
        if (pthread_create(&threads[i], NULL, calls_forever, NULL) != 0) {
            return 1;
        }
    }
    usleep(1000);
    volatile int *nothing = 0;
    return *nothing;
}
