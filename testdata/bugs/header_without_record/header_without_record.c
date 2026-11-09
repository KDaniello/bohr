// A store that appends a record, then rewrites a header that points at it, and syncs once after
// both.
//
// The header is the first sector of the file and names how many records there are and where the
// last one starts; records follow from 4 KiB on. On start the store reads the header and checks
// the record it names, and exits 1 when that record is not there: the header survived a crash the
// record did not. That needs a crash that keeps a later write and loses an earlier one — the
// header and not the record — which a crash that keeps all unsynced writes or none never does.
// The correct store syncs the record before it writes the header.

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define MAGIC 0x6b6f6f6264726f63ULL
#define RECORDS 4096
#define RECORD 64

struct header {
    uint64_t magic, count, last;
};

struct record {
    uint64_t magic, seq;
    char body[RECORD - 16];
};

int main(void) {
    int fd = open("store.db", O_RDWR | O_CREAT, 0644);
    if (fd < 0) return 3;
    struct header h = {0};
    struct stat st;
    if (fstat(fd, &st) != 0) return 3;
    if (st.st_size >= (off_t)sizeof h && pread(fd, &h, sizeof h, 0) == sizeof h && h.magic == MAGIC
        && h.count > 0) {
        struct record r = {0};
        ssize_t got = pread(fd, &r, sizeof r, (off_t)h.last);
        if (got != sizeof r || r.magic != MAGIC || r.seq != h.count - 1) {
            fprintf(stderr, "the header names record %llu at %llu, which is not there\n",
                    (unsigned long long)(h.count - 1), (unsigned long long)h.last);
            return 1;
        }
    } else {
        h = (struct header){MAGIC, 0, 0};
    }
    struct timespec pause = {0, 1000 * 1000};
    for (;;) {
        struct record r = {MAGIC, h.count, {0}};
        snprintf(r.body, sizeof r.body, "record %llu", (unsigned long long)h.count);
        uint64_t at = RECORDS + h.count * RECORD;
        if (pwrite(fd, &r, sizeof r, (off_t)at) != sizeof r) return 3;
        h.count += 1;
        h.last = at;
        if (pwrite(fd, &h, sizeof h, 0) != sizeof h) return 3;
        if (fsync(fd) != 0) return 3;
        nanosleep(&pause, 0);
    }
}
