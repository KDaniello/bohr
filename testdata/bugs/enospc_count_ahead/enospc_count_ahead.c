/* A log whose count of records is written ahead of the record. On ENOSPC the record is not
 * written and the store stops, the count one ahead; every start after it finds fewer records than
 * the count says and refuses to open, though the disk has room again.
 *
 * The count is a fixed-width number overwritten in place, which needs no new space: only the
 * record's append can be refused. Every start reads every counted record back first. Caught by
 * R7.2: the node refused space in one start does not answer in the next.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

enum { RECORDS = 6, RECORD = 4, COUNT = 9 };

static void pause_a_moment(void) {
    struct timespec how_long = {0, 200 * 1000};
    nanosleep(&how_long, 0);
}

static long number_in(const char *path) {
    char text[24] = "";
    int fd = open(path, O_RDONLY);
    if (fd >= 0 && read(fd, text, sizeof text - 1) < 0)
        text[0] = 0;
    if (fd >= 0)
        close(fd);
    return atol(text);
}

static int counted(int fd, long value) {
    char text[16];
    snprintf(text, sizeof text, "%08ld\n", value);
    return pwrite(fd, text, COUNT, 0) == COUNT && fsync(fd) == 0;
}

int main(void) {
    if (access("/count", F_OK) == 0) {
        long count = number_in("/count");
        int log = open("/log", O_RDONLY);
        off_t held = log >= 0 ? lseek(log, 0, SEEK_END) : 0;
        char text[RECORDS * RECORD + 1] = "";
        if (log >= 0 && pread(log, text, sizeof text - 1, 0) < 0)
            text[0] = 0;
        if (log >= 0)
            close(log);
        /* A log shorter than its count is read as a torn one, and refused. */
        if (held != count * RECORD)
            return 3;
        for (long r = 0; r < count; r++) {
            char key[24];
            snprintf(key, sizeof key, "r%02ld", r);
            bohr_kv_read(key, strstr(text, key) ? "1" : 0);
        }
        bohr_kv_ready();
        return 0;
    }
    int count = open("/count", O_CREAT | O_RDWR, 0644);
    int log = open("/log", O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (count < 0 || log < 0)
        return 1;
    /* Creating the count is retried until the disk has room: a correct answer to a full disk. */
    while (!counted(count, 0))
        pause_a_moment();
    bohr_kv_ready();
    for (int r = 0; r < RECORDS; r++) {
        char key[16], line[24];
        snprintf(key, sizeof key, "r%02d", r);
        snprintf(line, sizeof line, "%s\n", key);
        BOHR_KV_PUT("kv.begun", key, "1", (unsigned long)r + 1);
        /* The defect: the record is counted before it is written. */
        if (!counted(count, r + 1))
            return 1;
        if (write(log, line, RECORD) != RECORD || fsync(log) != 0)
            return 1;
        BOHR_KV_PUT("kv.acked", key, "1", (unsigned long)r + 1);
        pause_a_moment();
    }
    return 0;
}
