// One thread signals another, which is waiting, and the signal's handler runs on the thread it was
// sent to. The waiter checks that its own handler ran and that nobody else's did; it exits 0 only
// when both hold, so a run that delivered the signal to the wrong task or not at all fails itself.
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t handled_by_waiter;
static volatile sig_atomic_t handled_elsewhere;
static pthread_t waiter_id;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static int waiting;
// Never posted: a wait on it ends only when a signal interrupts it.
static sem_t never;

static void on_usr1(int sig) {
    (void)sig;
    if (pthread_equal(pthread_self(), waiter_id)) {
        handled_by_waiter = 1;
    } else {
        handled_elsewhere = 1;
    }
}

static void *waiter(void *arg) {
    (void)arg;
    pthread_mutex_lock(&lock);
    waiting = 1;
    pthread_cond_broadcast(&ready);
    pthread_mutex_unlock(&lock);
    // Waits until the signal arrives: the wait ends with EINTR, the handler has run on this
    // thread, and the loop sees it.
    while (!handled_by_waiter) {
        sem_wait(&never);
    }
    return 0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, 0);
    sem_init(&never, 0, 0);

    if (pthread_create(&waiter_id, 0, waiter, 0) != 0) {
        return 2;
    }
    pthread_mutex_lock(&lock);
    while (!waiting) {
        pthread_cond_wait(&ready, &lock);
    }
    pthread_mutex_unlock(&lock);

    pthread_kill(waiter_id, SIGUSR1);
    pthread_join(waiter_id, 0);
    if (!handled_by_waiter || handled_elsewhere) {
        return 1;
    }
    write(1, "handled by the waiter\n", 22);
    return 0;
}
