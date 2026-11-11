/* A store created by writing its manifest before its entry point, which refuses to open forever
 * once a creation was interrupted between the two.
 *
 * Caught by R1.4: the node the world restarted never answers again.
 */
#include <stdio.h>
#include <time.h>

#include "../../guests/bohr_kv.h"

static void pause_a_moment(void) {
    struct timespec how_long = {0, 1000 * 1000};
    nanosleep(&how_long, 0);
}

static int create(const char *path, const char *content) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    int ok = fd >= 0 && write(fd, content, strlen(content)) == (ssize_t)strlen(content) &&
             fsync(fd) == 0;
    if (fd >= 0)
        close(fd);
    return ok;
}

int main(void) {
    int manifest = access("/store.manifest", F_OK) == 0;
    int entry = access("/store.entry", F_OK) == 0;
    /* The defect: a manifest without an entry point is called corrupt, not a creation to finish. */
    if (manifest && !entry) {
        fprintf(stderr, "ERROR: the store is corrupted: entry point missing\n");
        return 1;
    }
    if (!manifest) {
        if (!create("/store.manifest", "version 1\n"))
            return 2;
        pause_a_moment();
        if (!create("/store.entry", "root 0\n"))
            return 2;
    }
    bohr_kv_ready();
    BOHR_KV_PUT("kv.begun", "opened", "yes", 1);
    BOHR_KV_PUT("kv.acked", "opened", "yes", 1);
    bohr_kv_read("opened", "yes");
    return 0;
}
