// A datagram protocol that trusts what it receives: no checksum, no sequence check beyond reading it.
//
//     no_checksum <count> <node>
//
// Node 1 sends `count` numbered messages to node 0 and waits for each to be answered; node 0
// takes each message as the truth and checks only that it says what the sender meant, which is
// the check a protocol with a checksum would have made before acting. A message altered on the
// wire is acted on, and node 0 exits 1 naming it: that is the defect, a protocol that would have
// applied a corrupted update. Nothing else in the program can fail.

#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define PORT 7100
#define SIZE 32

static struct sockaddr_in node_address(int node) {
    struct sockaddr_in at = {.sin_family = AF_INET, .sin_port = htons(PORT)};
    char text[32];
    snprintf(text, sizeof text, "10.66.0.%d", node + 1);
    inet_pton(AF_INET, text, &at.sin_addr);
    return at;
}

int main(int argc, char **argv) {
    if (argc < 3) return 2;
    long count = atol(argv[1]);
    int node = atoi(argv[2]);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in me = node_address(node);
    if (s < 0 || bind(s, (struct sockaddr *)&me, sizeof me) != 0) return 3;
    char message[SIZE];
    if (node == 0) {
        for (long i = 0; i < count; i++) {
            struct sockaddr_in from;
            socklen_t len = sizeof from;
            if (recvfrom(s, message, SIZE, 0, (struct sockaddr *)&from, &len) != SIZE) return 3;
            char meant[SIZE] = {0};
            snprintf(meant, SIZE, "set balance=%08ld", i);
            if (memcmp(message, meant, SIZE) != 0) {
                fprintf(stderr, "applied an update nobody sent at message %ld\n", i);
                return 1;
            }
            sendto(s, "ok", 2, 0, (struct sockaddr *)&from, len);
        }
    } else {
        struct sockaddr_in server = node_address(0);
        struct timespec pause = {0, 10 * 1000 * 1000};
        nanosleep(&pause, 0);
        char answer[8];
        for (long i = 0; i < count; i++) {
            memset(message, 0, SIZE);
            snprintf(message, SIZE, "set balance=%08ld", i);
            sendto(s, message, SIZE, 0, (struct sockaddr *)&server, sizeof server);
            // A bounded wait: a node 0 that died over an altered update answers nothing, and
            // waiting for it for ever would end the run as a hang instead of as its crash.
            struct pollfd answered = {.fd = s, .events = POLLIN};
            if (poll(&answered, 1, 1000) != 1) return 0;
            if (recv(s, answer, sizeof answer, 0) < 0) return 3;
        }
    }
    return 0;
}
