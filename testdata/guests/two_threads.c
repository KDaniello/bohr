// Two threads taking turns through a mutex, which is the shape every race in a real program has:
// the outcome depends on who the scheduler admits between the read and the write.
//
// Each thread appends its own mark, so the printed order is the interleaving the scheduler chose.
#include <pthread.h>
#include <unistd.h>

#define STEPS 8

static char marks[2 * STEPS + 1];
static int next_mark;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void *step(void *arg) {
    char me = (char)(long)arg;
    for (int i = 0; i < STEPS; i++) {
        pthread_mutex_lock(&lock);
        marks[next_mark++] = me;
        pthread_mutex_unlock(&lock);
    }
    return 0;
}

int main(void) {
    pthread_t other;
    if (pthread_create(&other, 0, step, (void *)(long)'b') != 0) {
        return 1;
    }
    step((void *)(long)'a');
    pthread_join(other, 0);

    marks[2 * STEPS] = '\n';
    return write(1, marks, sizeof marks) > 0 ? 0 : 1;
}
