/* A parent thread spawns a worker, waits for it, and reports what it computed.
 *
 * The join is the synchronisation under test: the worker writes a result and ends, the
 * parent blocks until it does, then reads the result. The order is fixed by the futex the
 * join waits on, so the printed sum is one value per seed. */
#include <pthread.h>
#include <stdio.h>

enum { N = 1000 };

static long result;

static void *worker(void *arg) {
    (void)arg;
    long sum = 0;
    for (int i = 1; i <= N; i++)
        sum += i;
    result = sum;
    return NULL;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    printf("%ld\n", result);
    return 0;
}
