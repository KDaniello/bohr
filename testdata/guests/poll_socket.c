/* Waits for a datagram with poll(2), the way a resolver waits for its answer.
 *
 * Node 1 sends one datagram to node 0 after 100 ms. Node 0 first polls with a timeout of nought
 * (nothing is there yet), then waits up to five seconds; it exits 0 only if the second poll reports
 * its socket readable and the datagram is the one sent. Run as two nodes, poll_socket.toml. */
#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

enum { PORT = 7100 };

static struct sockaddr_in addr_of(int node) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(PORT);
    a.sin_addr.s_addr = htonl(0x0A420000u | (uint32_t)(node + 1));
    return a;
}

int main(int argc, char **argv) {
    int self = argc > 1 ? atoi(argv[1]) : 0;
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0)
        return 2;
    struct sockaddr_in me = addr_of(self);
    if (bind(fd, (struct sockaddr *)&me, sizeof me) != 0)
        return 3;

    if (self == 1) {
        struct timespec pause = {0, 100000000};
        nanosleep(&pause, 0);
        struct sockaddr_in to = addr_of(0);
        return sendto(fd, "ping", 4, 0, (struct sockaddr *)&to, sizeof to) == 4 ? 0 : 4;
    }

    struct pollfd p = {fd, POLLIN, 0};
    if (poll(&p, 1, 0) != 0 || p.revents != 0)
        return 5;
    if (poll(&p, 1, 5000) != 1 || !(p.revents & POLLIN))
        return 6;
    char got[8] = {0};
    if (recv(fd, got, sizeof got, 0) != 4 || memcmp(got, "ping", 4) != 0)
        return 7;
    return 0;
}
