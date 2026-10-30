// Lazy initialisation without a lock. Both threads test the flag, and whichever ordering puts both
// tests before either set has both of them build the resource — a leaked handle, a second
// connection, a buffer nobody owns.
//
// The defect needs two orderings to fall in place rather than one, which is what makes it depth 2:
// the second thread has to reach its test after the first has passed its own and before the first
// has set the flag.
#include <sched.h>
#include <stdio.h>
#include <pthread.h>
#include <unistd.h>

static volatile int ready;
static volatile int built;

static void *use(void *arg) {
    (void)arg;
    if (!ready) {
        // Whatever building the resource costs, the window is here.
        sched_yield();
        built++;
        sched_yield();
        ready = 1;
    }
    return 0;
}

int main(void) {
    pthread_t other;
    if (pthread_create(&other, 0, use, 0) != 0) {
        return 2;
    }
    use(0);
    pthread_join(other, 0);

    char line[64];
    int n = snprintf(line, sizeof line, "built=%d\n", built);
    if (write(1, line, (size_t)n) != n) {
        return 2;
    }
    // One resource, however many threads asked for it.
    return built == 1 ? 0 : 1;
}
