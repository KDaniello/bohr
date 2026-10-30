// Copies a file to standard output, which is the shape of the reproducibility claim a user checks
// first: the same command, the same seed, the same bytes on the pipe.
//
// Reads a bounded amount so the run ends on its own when the source has no end, as `/dev/urandom`
// does.
#include <fcntl.h>
#include <sys/types.h>
#include <unistd.h>

#define LIMIT 4096

int main(int argc, char **argv) {
    if (argc < 2) {
        return 2;
    }

    int fd = open(argv[1], O_RDONLY);
    if (fd < 0) {
        return 1;
    }

    char buf[512];
    size_t total = 0;
    while (total < LIMIT) {
        size_t want = sizeof buf;
        if (LIMIT - total < want) {
            want = LIMIT - total;
        }
        ssize_t n = read(fd, buf, want);
        if (n <= 0) {
            break;
        }
        if (write(1, buf, (size_t)n) != n) {
            close(fd);
            return 1;
        }
        total += (size_t)n;
    }

    close(fd);
    return 0;
}
