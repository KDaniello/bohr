/* A store that records a request's id in a write apart from the update it guards: a crash between
 * the two, and the retry after the restart applies the update again.
 *
 * Every start reads the counter back, retries every request whose id it has not recorded, and
 * reads the counter again. Caught by R4.1: the counter ends above what every request applied once
 * makes of it.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

enum { REQUESTS = 4 };

static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

static long counter(void) {
    FILE *f = fopen("/counter", "r");
    long value = 0;
    if (f && fscanf(f, "%ld", &value) != 1)
        value = 0;
    if (f)
        fclose(f);
    return value;
}

static int durable(const char *path, const char *text, int append) {
    int fd = open(path, O_CREAT | O_WRONLY | (append ? O_APPEND : O_TRUNC), 0644);
    int ok = fd >= 0 && write(fd, text, strlen(text)) == (ssize_t)strlen(text) && fsync(fd) == 0;
    if (fd >= 0)
        close(fd);
    return ok;
}

static int recorded(int request) {
    FILE *f = fopen("/requests", "r");
    char line[16], want[16];
    int found = 0;
    snprintf(want, sizeof want, "r%d\n", request);
    while (f && fgets(line, sizeof line, f))
        found |= strcmp(line, want) == 0;
    if (f)
        fclose(f);
    return found;
}

static void read_back(void) {
    char value[24];
    snprintf(value, sizeof value, "%ld", counter());
    bohr_kv_read("c", value);
}

int main(void) {
    read_back();
    bohr_kv_ready();
    for (int r = 0; r < REQUESTS; r++) {
        if (recorded(r))
            continue;
        char id[8], value[24], line[16];
        snprintf(id, sizeof id, "r%d", r);
        BOHR_KV_ADD("kv.begun", "c", 1, id);
        snprintf(value, sizeof value, "%ld\n", counter() + 1);
        if (!durable("/counter", value, 0))
            return 2;
        pause_a_moment();
        /* The defect: the id is recorded in a write of its own, after the update it guards. */
        snprintf(line, sizeof line, "r%d\n", r);
        if (!durable("/requests", line, 1))
            return 2;
        BOHR_KV_ADD("kv.acked", "c", 1, id);
    }
    read_back();
    return 0;
}
