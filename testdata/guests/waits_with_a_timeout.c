// A condition nobody signals, waited on with a deadline ten milliseconds away: the wait must end
// with ETIMEDOUT at the deadline, on a machine of its own with no cluster to arm a timer for it.
#include <errno.h>
#include <pthread.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t never = PTHREAD_COND_INITIALIZER;
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_nsec += 10 * 1000 * 1000;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_sec += 1;
        deadline.tv_nsec -= 1000000000L;
    }
    pthread_mutex_lock(&lock);
    int got = pthread_cond_timedwait(&never, &lock, &deadline);
    pthread_mutex_unlock(&lock);
    if (got != ETIMEDOUT) {
        return 1;
    }
    write(1, "timed out\n", 10);
    return 0;
}
