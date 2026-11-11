/* A store whose log takes puts and tombstones, which acknowledges a delete before its tombstone
 * is durable: a crash in between brings the deleted value back.
 *
 * Every start replays the log and reads every key back first. Caught by R1.2.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

enum { KEYS = 4 };

static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

/* What the log says each key holds: its value, or nothing. */
static void replay(char held[KEYS][8]) {
    memset(held, 0, sizeof(char[KEYS][8]));
    FILE *log = fopen("/data.log", "r");
    char line[64];
    while (log && fgets(line, sizeof line, log)) {
        int key = -1;
        char value[8] = "";
        if (sscanf(line, "P %d %7s", &key, value) == 2 && key >= 0 && key < KEYS)
            snprintf(held[key], 8, "%s", value);
        else if (sscanf(line, "D %d", &key) == 1 && key >= 0 && key < KEYS)
            held[key][0] = 0;
    }
    if (log)
        fclose(log);
}

int main(void) {
    char held[KEYS][8];
    int again = access("/data.log", F_OK) == 0;
    replay(held);
    for (int i = 0; i < KEYS; i++) {
        char key[8];
        snprintf(key, sizeof key, "k%d", i);
        bohr_kv_read(key, held[i][0] ? held[i] : 0);
    }
    bohr_kv_ready();
    if (again)
        return 0;
    int log = open("/data.log", O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (log < 0)
        return 2;
    unsigned long seq = 0;
    for (int i = 0; i < KEYS; i++) {
        char line[16], key[8];
        int len = snprintf(line, sizeof line, "P %d v1\n", i);
        snprintf(key, sizeof key, "k%d", i);
        BOHR_KV_PUT("kv.begun", key, "v1", ++seq);
        if (write(log, line, (size_t)len) != len || fsync(log) != 0)
            return 2;
        BOHR_KV_PUT("kv.acked", key, "v1", seq);
    }
    for (int i = 0; i < KEYS; i++) {
        char line[16], key[8];
        int len = snprintf(line, sizeof line, "D %d\n", i);
        snprintf(key, sizeof key, "k%d", i);
        BOHR_KV_DELETE("kv.begun", key, ++seq);
        if (write(log, line, (size_t)len) != len)
            return 2;
        /* The defect: the delete is acknowledged before its tombstone is synced. */
        BOHR_KV_DELETE("kv.acked", key, seq);
        pause_a_moment();
        if (fsync(log) != 0)
            return 2;
    }
    return 0;
}
