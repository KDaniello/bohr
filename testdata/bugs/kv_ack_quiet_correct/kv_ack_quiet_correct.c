/* The same store with the promise kept: nothing is acknowledged before it is durable.
 *
 * The twin of `kv_ack_quiet`, and its whole reason for existing. "Caught on a seeded defect" is
 * half a claim — an oracle that fired on every run would satisfy it — so the gate is the pair, on
 * the same scenario and the same seeds: found on the one below and never on this one.
 *
 * Identical in every other respect, deliberately: same records, same answers, same pause, same
 * kill landing in the same place. The only difference is the order of two calls.
 */
#include <arpa/inet.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

enum { RECORD = 64, PUTS = 16, PORT = 7000 };

/* Where the answer goes. Node 1 of the cluster, which the world addresses as 10.66.0.2. */
static void answer(int sock, int index) {
    struct sockaddr_in peer;
    memset(&peer, 0, sizeof peer);
    peer.sin_family = AF_INET;
    peer.sin_port = htons(PORT);
    peer.sin_addr.s_addr = inet_addr("10.66.0.2");
    char note[16];
    int len = snprintf(note, sizeof note, "ack %d", index);
    sendto(sock, note, (size_t)len, 0, (struct sockaddr *)&peer, sizeof peer);
}

/* The same pause the defective twin takes, so the kill lands in the same place. */
static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

int main(void) {
    int data = open("store.db", O_CREAT | O_RDWR, 0644);
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (data < 0 || sock < 0)
        return 2;

    char record[RECORD];
    for (int i = 0; i < PUTS; i++) {
        memset(record, 'k', sizeof record);
        if (pwrite(data, record, sizeof record, (off_t)i * RECORD) != (ssize_t)sizeof record)
            return 2;

        /* The promise kept: durable first, and only then does anybody hear about it. A kill
         * anywhere in this loop takes nothing that was ever acknowledged. */
        if (fsync(data) != 0)
            return 2;
        answer(sock, i + 1);
        pause_a_moment();
    }
    return 0;
}
