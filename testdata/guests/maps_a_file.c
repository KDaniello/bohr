/* A file reached through a mapping rather than through reads, both ways round.
 *
 * The shape every embedded store has: write the file with a descriptor, read it back through a
 * window onto it, write through that window, and make the writes durable. What is under test is
 * that the two views agree — a mapping showing bytes the file does not hold, or a file missing
 * bytes the mapping was given, is a guest reading something the world never handed it.
 *
 * Exits 0 when every check held, and the number of the check that failed otherwise, so a run that
 * goes wrong says where rather than only that. */

#include <fcntl.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define PATH "/tmp/mapped.db"
#define PAGE 4096

int main(void) {
    int fd = open(PATH, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        return 1;
    }

    char written[PAGE];
    memset(written, 'a', sizeof written);
    if (write(fd, written, sizeof written) != (ssize_t)sizeof written) {
        return 2;
    }

    char *window = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (window == MAP_FAILED) {
        return 3;
    }

    /* What the descriptor wrote is what the window shows. */
    if (window[0] != 'a' || window[PAGE - 1] != 'a') {
        return 4;
    }

    /* A write through the descriptor reaches the window that is already open on it. */
    char again[16];
    memset(again, 'b', sizeof again);
    if (pwrite(fd, again, sizeof again, 0) != (ssize_t)sizeof again) {
        return 5;
    }
    if (window[0] != 'b' || window[15] != 'b' || window[16] != 'a') {
        return 6;
    }

    /* And a write through the window reaches the file, once it has been made durable. */
    memset(window + 64, 'c', 32);
    if (msync(window, PAGE, MS_SYNC) != 0) {
        return 7;
    }

    char read_back[PAGE];
    if (pread(fd, read_back, sizeof read_back, 0) != (ssize_t)sizeof read_back) {
        return 8;
    }
    if (read_back[64] != 'c' || read_back[95] != 'c' || read_back[96] != 'a') {
        return 9;
    }

    if (munmap(window, PAGE) != 0) {
        return 10;
    }
    if (close(fd) != 0) {
        return 11;
    }
    return 0;
}
