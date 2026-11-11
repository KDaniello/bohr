/* A store that applies a batch key by key, each into a buffer allocated as it goes: an allocation
 * refused in the middle of a batch leaves its first key applied and the rest not.
 *
 * The start reads every key back once its batches are done. Caught by R1.3: a batch read back in
 * part.
 */
#include <sys/mman.h>

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

enum { BATCHES = 24 };

static int apply(int store, const char *key) {
    char *buffer = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buffer == MAP_FAILED)
        return 0;
    int len = snprintf(buffer, 4096, "%s\n", key);
    int ok = write(store, buffer, (size_t)len) == len && fsync(store) == 0;
    munmap(buffer, 4096);
    return ok;
}

int main(void) {
    int store = open("/store.kv", O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (store < 0)
        return 2;
    bohr_kv_ready();
    for (int b = 0; b < BATCHES; b++) {
        char a[8], c[8];
        snprintf(a, sizeof a, "a%d", b);
        snprintf(c, sizeof c, "b%d", b);
        const char *keys[2] = {a, c}, *values[2] = {"1", "1"};
        BOHR_KV_BATCH("kv.begun", keys, values, 2, (unsigned long)b + 1);
        /* The defect: each key is applied as soon as its own buffer is had. */
        if (apply(store, a) && apply(store, c))
            BOHR_KV_BATCH("kv.acked", keys, values, 2, (unsigned long)b + 1);
        pause_a_moment();
    }
    for (int b = 0; b < BATCHES; b++) {
        char a[8], c[8];
        snprintf(a, sizeof a, "a%d", b);
        snprintf(c, sizeof c, "b%d", b);
        bohr_kv_read(a, has("/store.kv", a) ? "1" : 0);
        bohr_kv_read(c, has("/store.kv", c) ? "1" : 0);
    }
    return 0;
}
