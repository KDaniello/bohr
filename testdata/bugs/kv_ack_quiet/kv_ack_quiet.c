/* The kv_fsync defect with the self-check taken out: the store loses an acknowledged write and
 * says nothing about it.
 *
 * Its neighbour `kv_fsync` compares its acknowledgement count against what survived and exits 1,
 * so the crash oracle finds it and it proves nothing about anything else. A real client does not
 * do that — it stays alive, serves what it has, and is simply wrong — so this one serves what it
 * has and exits 0. Only an oracle judging from the boundary can tell the two apart.
 *
 * The acknowledgement goes out over the network rather than to standard output, because that is
 * what the world can see a node doing: `docs/API.md` §5.6 defines an answer as an outbound
 * datagram sent while the node's disk is dirty, and a print is not one.
 *
 * Depth 2: a kill must land between one record's answer and the sync that would have kept it.
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

/* Long enough for a scheduled kill to land inside it. */
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

        /* The defect: the answer goes out here, and the sync that would have made it true
         * happens afterwards. Whatever a kill takes in between was acknowledged and is gone. */
        answer(sock, i + 1);
        pause_a_moment();
        if (fsync(data) != 0)
            return 2;
    }
    return 0;
}
