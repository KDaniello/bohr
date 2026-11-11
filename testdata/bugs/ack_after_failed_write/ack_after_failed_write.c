/* A store that appends each record and acknowledges it without reading what the write answered:
 * a write the disk refuses with ENOSPC is acknowledged all the same.
 *
 * The start reads every key back once its records are written. Caught by R7.1: a key acknowledged
 * after its write was refused, and read back absent.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

static void pause_a_moment(void) {
    struct timespec how_long = {0, 200 * 1000};
    nanosleep(&how_long, 0);
}

/* Whether the store holds `key`, read without the C library's buffers, which allocate. */
static int has(const char *path, const char *key) {
    static char text[4096];
    char want[32];
    int fd = open(path, O_RDONLY), n = 0, got;
    while (fd >= 0 && n < (int)sizeof text - 1 && (got = read(fd, text + n, sizeof text - 1 - n)) > 0)
        n += got;
    if (fd >= 0)
        close(fd);
    text[n] = 0;
    snprintf(want, sizeof want, "%s\n", key);
    for (char *at = text; (at = strstr(at, want)); at++)
        if (at == text || at[-1] == '\n')
            return 1;
    return 0;
}

enum { RECORDS = 8 };

int main(void) {
    int store = open("/store.kv", O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (store < 0)
        return 2;
    bohr_kv_ready();
    for (int r = 0; r < RECORDS; r++) {
        char key[8], line[16];
        snprintf(key, sizeof key, "k%d", r);
        int len = snprintf(line, sizeof line, "%s\n", key);
        BOHR_KV_PUT("kv.begun", key, "1", (unsigned long)r + 1);
        /* The defect: the write's answer is never looked at. */
        if (write(store, line, (size_t)len) < 0) {
        }
        if (fsync(store) != 0)
            break;
        BOHR_KV_PUT("kv.acked", key, "1", (unsigned long)r + 1);
        pause_a_moment();
    }
    for (int r = 0; r < RECORDS; r++) {
        char key[8];
        snprintf(key, sizeof key, "k%d", r);
        bohr_kv_read(key, has("/store.kv", key) ? "1" : 0);
    }
    return 0;
}
