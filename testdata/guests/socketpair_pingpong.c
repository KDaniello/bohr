/* A ping-pong over a socketpair between two threads.
 *
 * One thread writes a token, the other reads it and writes it back, a fixed number of
 * rounds. The socketpair is intra-node: no draws, no network, just the blocking read on
 * each end parking the thread until its peer writes. The printed round count is the same
 * every run of a seed. */
#include <pthread.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

enum { ROUNDS = 64 };

static int sv[2];

static void *ponger(void *arg) {
    (void)arg;
    char token;
    for (int i = 0; i < ROUNDS; i++) {
        if (read(sv[1], &token, 1) != 1)
            break;
        if (write(sv[1], &token, 1) != 1)
            break;
    }
    return NULL;
}

int main(void) {
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0)
        return 1;

    pthread_t t;
    pthread_create(&t, NULL, ponger, NULL);

    char token = 'x';
    int rounds = 0;
    for (; rounds < ROUNDS; rounds++) {
        if (write(sv[0], &token, 1) != 1)
            break;
        if (read(sv[0], &token, 1) != 1)
            break;
    }

    pthread_join(t, NULL);
    printf("%d\n", rounds);
    return 0;
}
