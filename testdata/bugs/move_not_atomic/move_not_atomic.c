/* A move between two counters written as two durable writes, a debit and then a credit: a crash
 * between them leaves the debit without its credit, and the counters' total changes.
 *
 * Every start reads both counters back. Caught by R5.1: what the moves keep sums to less than the
 * counters began at.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

enum { MOVES = 3, AMOUNT = 10 };

static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

static long held(const char *path) {
    FILE *f = fopen(path, "r");
    long value = 0;
    if (f && fscanf(f, "%ld", &value) != 1)
        value = 0;
    if (f)
        fclose(f);
    return value;
}

static int durable(const char *path, long value) {
    char text[24];
    snprintf(text, sizeof text, "%ld\n", value);
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    int ok = fd >= 0 && write(fd, text, strlen(text)) == (ssize_t)strlen(text) && fsync(fd) == 0;
    if (fd >= 0)
        close(fd);
    return ok;
}

static void read_back(void) {
    char a[24], b[24];
    snprintf(a, sizeof a, "%ld", held("/a"));
    snprintf(b, sizeof b, "%ld", held("/b"));
    bohr_kv_read("a", a);
    bohr_kv_read("b", b);
}

int main(void) {
    int again = access("/a", F_OK) == 0;
    if (!again) {
        if (!durable("/a", 100) || !durable("/b", 0))
            return 2;
        BOHR_KV_PUT("kv.acked", "a", "100", 1);
        BOHR_KV_PUT("kv.acked", "b", "0", 2);
    }
    read_back();
    bohr_kv_ready();
    if (again)
        return 0;
    for (int m = 0; m < MOVES; m++) {
        char id[8];
        snprintf(id, sizeof id, "m%d", m);
        BOHR_KV_MOVE("kv.begun", "a", "b", AMOUNT, id);
        /* The defect: the debit and the credit are two writes, each durable on its own. */
        if (!durable("/a", held("/a") - AMOUNT))
            return 2;
        pause_a_moment();
        if (!durable("/b", held("/b") + AMOUNT))
            return 2;
        BOHR_KV_MOVE("kv.acked", "a", "b", AMOUNT, id);
    }
    return 0;
}
