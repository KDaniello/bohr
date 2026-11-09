// A timed futex wait that is woken early, then a second wait on the same word with no timeout.
//
// The first wait's timer belongs to the first wait. If it outlived it, the second wait would end
// with ETIMEDOUT at the first one's deadline instead of waiting for the wake that comes later.
// Exits 0 when the second wait ends by the wake, 1 when it ends any other way.

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static uint32_t word;
/* How many waits the main thread has entered, so each wake comes after the wait it is for: in a
 * world without a cluster a sleep moves the clock without yielding, and the waker would otherwise
 * run to its end before the main thread waits at all. */
static volatile int entered;

static long futex(int op, uint32_t val, const struct timespec *timeout) {
    return syscall(SYS_futex, &word, op, val, timeout, 0, 0);
}

static void *waker(void *ignored) {
    (void)ignored;
    while (entered < 1) sched_yield();
    struct timespec one = {1, 0};
    nanosleep(&one, 0);
    futex(FUTEX_WAKE_PRIVATE, 1, 0);
    while (entered < 2) sched_yield();
    struct timespec six = {6, 0};
    nanosleep(&six, 0);
    futex(FUTEX_WAKE_PRIVATE, 1, 0);
    return 0;
}

int main(void) {
    pthread_t t;
    if (pthread_create(&t, 0, waker, 0) != 0) return 2;
    struct timespec five = {5, 0};
    entered = 1;
    futex(FUTEX_WAIT_PRIVATE, 0, &five);
    entered = 2;
    long second = futex(FUTEX_WAIT_PRIVATE, 0, 0);
    int timed_out = second != 0 && errno == ETIMEDOUT;
    pthread_join(t, 0);
    return timed_out ? 1 : 0;
}
