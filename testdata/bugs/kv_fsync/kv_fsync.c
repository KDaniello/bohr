/* A key-value store that acknowledges before it syncs.
 *
 * The put path writes the record and prints "ACK" — and only then calls fsync. A crash
 * between the acknowledgement and the sync loses an acknowledged write, which is the
 * defect: on restart the store finds fewer records than it acknowledged and exits 1.
 *
 * Depth 2: a crash must land in the window between one write's ACK and its fsync. Under
 * a scheduled or drawn node kill the window is a few virtual microseconds wide, which is
 * exactly the kind of moment guided targeting is measured on.
 */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

enum { RECORD = 64, PUTS = 16 };

static int count_records(int fd) {
    char record[RECORD];
    int n = 0;
    while (pread(fd, record, sizeof record, (off_t)n * RECORD) == (ssize_t)sizeof record)
        n++;
    return n;
}

int main(void) {
    int meta = open("acked.count", O_CREAT | O_RDWR, 0644);
    int data = open("store.db", O_CREAT | O_RDWR, 0644);
    if (meta < 0 || data < 0)
        return 2;

    /* The acknowledged count survives crashes because it is synced before the ACK;
     * the data is what the bug loses. */
    char buf[32];
    ssize_t got = pread(meta, buf, sizeof buf - 1, 0);
    int acked = 0;
    if (got > 0) {
        buf[got] = '\0';
        sscanf(buf, "%d", &acked);
    }

    int have = count_records(data);
    if (have < acked) {
        fprintf(stderr, "lost %d acknowledged records\n", acked - have);
        return 1;
    }

    char record[RECORD];
    for (int i = have; i < PUTS; i++) {
        memset(record, 'k', sizeof record);
        if (pwrite(data, record, sizeof record, (off_t)i * RECORD) != (ssize_t)sizeof record)
            return 2;

        int len = snprintf(buf, sizeof buf, "%d", i + 1);
        if (pwrite(meta, buf, (size_t)len, 0) != len || fsync(meta) != 0)
            return 2;
        printf("ACK %d\n", i + 1);

        /* The defect: the ACK above went out before this sync. */
        if (fsync(data) != 0)
            return 2;
    }
    return 0;
}
