/* A value written to a temporary file, synced, renamed into place and acknowledged, with the
 * directory never synced: the rename is not durable until something syncs the namespace, and a
 * crash before then loses an acknowledged put.
 *
 * Every start reads every key back first; the laws compare what it reads with what was
 * acknowledged. Caught by R1.1.
 */
#include <stdio.h>
#include <sys/stat.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

enum { KEYS = 5 };

static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

static void read_back(void) {
    for (int i = 0; i < KEYS; i++) {
        char path[32], value[16] = "", key[8];
        snprintf(path, sizeof path, "/data/k%d", i);
        snprintf(key, sizeof key, "k%d", i);
        int fd = open(path, O_RDONLY);
        ssize_t got = fd < 0 ? -1 : read(fd, value, sizeof value - 1);
        bohr_kv_read(key, got > 0 ? value : 0);
        if (fd >= 0)
            close(fd);
    }
    bohr_kv_ready();
}

int main(void) {
    struct stat seen;
    int again = stat("/data", &seen) == 0;
    read_back();
    if (again)
        return 0;
    mkdir("/data", 0755);
    for (int i = 0; i < KEYS; i++) {
        char tmp[32], path[32], key[8];
        snprintf(tmp, sizeof tmp, "/data/k%d.tmp", i);
        snprintf(path, sizeof path, "/data/k%d", i);
        snprintf(key, sizeof key, "k%d", i);
        int fd = open(tmp, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd < 0 || write(fd, "v1", 2) != 2 || fsync(fd) != 0)
            return 2;
        close(fd);
        BOHR_KV_PUT("kv.begun", key, "v1", (unsigned long)i + 1);
        if (rename(tmp, path) != 0)
            return 2;
        /* The defect: the rename is acknowledged without syncing /data. */
        BOHR_KV_PUT("kv.acked", key, "v1", (unsigned long)i + 1);
        pause_a_moment();
    }
    return 0;
}
