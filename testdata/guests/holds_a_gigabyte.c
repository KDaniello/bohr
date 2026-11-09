// A node that holds a gibibyte of files — sixteen of 64 MiB, the most one file may be — with a
// thousand pages written across them and not synced, then waits to be killed: the state that makes
// a crash image costly to build, and so the one its cost is measured on.

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

#define FILES 16
#define FILE_BYTES (64L << 20)

int main(void) {
    int fds[FILES];
    for (int i = 0; i < FILES; i++) {
        char name[32];
        snprintf(name, sizeof name, "part-%02d.db", i);
        fds[i] = open(name, O_RDWR | O_CREAT, 0644);
        if (fds[i] < 0 || ftruncate(fds[i], FILE_BYTES) != 0 || fsync(fds[i]) != 0) return 3;
    }
    static char page[4096];
    uint64_t x = 0x9e3779b97f4a7c15ULL;
    for (int i = 0; i < 1000; i++) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        page[0] = (char)x;
        int fd = fds[x % FILES];
        if (pwrite(fd, page, sizeof page, (off_t)((x >> 8) % (FILE_BYTES / 4096)) * 4096) != sizeof page) return 3;
    }
    struct timespec rest = {10, 0};
    nanosleep(&rest, 0);
    return 0;
}
