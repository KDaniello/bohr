/* A batch of two keys written as two durable records with no commit marker: a crash between them
 * leaves one key of the batch and not the other.
 *
 * Every start reads every key back first. Caught by R1.3, which holds a batch to landing whole or
 * not at all whether or not it was acknowledged.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

enum { BATCHES = 3 };

static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

static int has(const char *key) {
    FILE *store = fopen("/batches.kv", "r");
    char line[32], want[32];
    int found = 0;
    snprintf(want, sizeof want, "%s=1\n", key);
    while (store && fgets(line, sizeof line, store))
        found |= strcmp(line, want) == 0;
    if (store)
        fclose(store);
    return found;
}

static int append(int store, const char *key) {
    char line[32];
    int len = snprintf(line, sizeof line, "%s=1\n", key);
    return write(store, line, (size_t)len) == len && fsync(store) == 0;
}

int main(void) {
    int again = access("/batches.kv", F_OK) == 0;
    for (int b = 0; b < BATCHES; b++) {
        char a[8], c[8];
        snprintf(a, sizeof a, "a%d", b);
        snprintf(c, sizeof c, "b%d", b);
        bohr_kv_read(a, has(a) ? "1" : 0);
        bohr_kv_read(c, has(c) ? "1" : 0);
    }
    bohr_kv_ready();
    if (again)
        return 0;
    int store = open("/batches.kv", O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (store < 0)
        return 2;
    for (int b = 0; b < BATCHES; b++) {
        char a[8], c[8];
        snprintf(a, sizeof a, "a%d", b);
        snprintf(c, sizeof c, "b%d", b);
        const char *keys[2] = {a, c}, *values[2] = {"1", "1"};
        BOHR_KV_BATCH("kv.begun", keys, values, 2, (unsigned long)b + 1);
        /* The defect: two records, each durable on its own, and nothing that marks them as one. */
        if (!append(store, a))
            return 2;
        pause_a_moment();
        if (!append(store, c))
            return 2;
        BOHR_KV_BATCH("kv.acked", keys, values, 2, (unsigned long)b + 1);
    }
    return 0;
}
