/* Two leaders under a one-directional partition.
 *
 * Node 0 starts as leader and heartbeats node 1; node 1 promotes itself when the
 * heartbeats stop, and announces the claim. Under a cut of only the 0-to-1 direction,
 * node 1 stops hearing node 0 and promotes — while node 0, still reachable from node 1,
 * receives the claim and is still leader itself: two leaders, and the program that
 * observes it exits 1.
 *
 * Depth 3: the cut must be one-directional, must outlast the election timeout, and the
 * claim must cross the surviving direction before the heal.
 *
 * Each claim is also a `log.leader` record of term 1, the only term this protocol has, so the law
 * R2.4 (one decider per round) judges the run from what the nodes said rather than from the exit.
 */
#include <arpa/inet.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include "../../guests/bohr_log.h"

enum { PORT_BASE = 7000, ROUNDS = 40 };

static struct sockaddr_in addr_of(int node) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)(PORT_BASE + node));
    a.sin_addr.s_addr = htonl(0x0A420000u | (uint32_t)(node + 1));
    return a;
}

static void sleep_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

int main(int argc, char **argv) {
    int self = argc > 1 ? atoi(argv[1]) : 0;
    int peer = 1 - self;
    int leader = self == 0;
    if (leader)
        bohr_log_leader(1);

    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0)
        return 2;
    struct sockaddr_in me = addr_of(self);
    if (bind(sock, (struct sockaddr *)&me, sizeof me) != 0)
        return 2;
    struct sockaddr_in other = addr_of(peer);

    int silent_rounds = 0;
    for (int round = 0; round < ROUNDS; round++) {
        if (leader)
            sendto(sock, "HB", 2, 0, (struct sockaddr *)&other, sizeof other);

        sleep_ms(10);

        char msg[8];
        ssize_t got = recvfrom(sock, msg, sizeof msg, MSG_DONTWAIT, NULL, NULL);
        if (got == 2 && memcmp(msg, "HB", 2) == 0) {
            silent_rounds = 0;
        } else if (got == 5 && memcmp(msg, "CLAIM", 5) == 0) {
            if (leader) {
                fprintf(stderr, "two leaders: node %d heard a claim while leading\n", self);
                return 1;
            }
        } else if (!leader) {
            /* The election timeout: three missed heartbeats and the follower promotes. */
            if (++silent_rounds == 3) {
                leader = 1;
                bohr_log_leader(1);
                sendto(sock, "CLAIM", 5, 0, (struct sockaddr *)&other, sizeof other);
            }
        }
    }
    return 0;
}
