// Names taken from a directory the program opened, rather than from wherever it happens to stand.
//
// The `*at` calls carry a directory descriptor and this model read none of them: every relative
// name went to the working directory, so a program that opens its directory once and writes
// everything under it by descriptor -- which is RocksDB, and `std::fs` wherever it can -- had its
// files put somewhere else and was told the call had succeeded. A refusal would have been kinder.
// What is asserted below is therefore as much about where the file is not as about where it is.

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    char buf[256];

    if (mkdir("/tmp/under", 0755) != 0) {
        return 1;
    }
    int dir = open("/tmp/under", O_RDONLY | O_DIRECTORY);
    if (dir < 0) {
        return 2;
    }

    // Created by descriptor and by a bare name.
    int fd = openat(dir, "written-here", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "in the directory", 16) != 16) {
        return 3;
    }
    close(fd);

    // It is under the directory that was named...
    fd = open("/tmp/under/written-here", O_RDONLY);
    if (fd < 0) {
        return 4;
    }
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    if (n != 16 || memcmp(buf, "in the directory", 16) != 0) {
        return 5;
    }
    // ...and not beside the working directory, which is where it used to go.
    struct stat ignored;
    if (syscall(SYS_newfstatat, AT_FDCWD, "/written-here", &ignored, 0) != -1) {
        return 6;
    }

    // The other calls that carry one, asked about the same name.
    if (faccessat(dir, "written-here", F_OK, 0) != 0) {
        return 7;
    }
    if (syscall(SYS_newfstatat, dir, "written-here", &ignored, 0) != 0) {
        return 8;
    }
    if (mkdirat(dir, "a-directory", 0755) != 0) {
        return 9;
    }
    if (symlinkat("written-here", dir, "the-link") != 0) {
        return 10;
    }
    memset(buf, 0, sizeof buf);
    if (readlinkat(dir, "the-link", buf, sizeof buf - 1) != 12 ||
        strcmp(buf, "written-here") != 0) {
        return 11;
    }
    if (syscall(SYS_renameat, dir, "written-here", dir, "moved-here") != 0) {
        return 12;
    }
    if (syscall(SYS_newfstatat, dir, "moved-here", &ignored, 0) != 0) {
        return 13;
    }
    if (unlinkat(dir, "moved-here", 0) != 0) {
        return 14;
    }
    if (unlinkat(dir, "a-directory", AT_REMOVEDIR) != 0) {
        return 15;
    }

    // A descriptor that is not a directory, and one that is not open at all, are different
    // mistakes and the kernel names them differently.
    fd = open("/tmp/an-ordinary-file", O_WRONLY | O_CREAT, 0644);
    if (fd < 0) {
        return 16;
    }
    if (openat(fd, "under-a-file", O_RDONLY) != -1 || errno != ENOTDIR) {
        return 17;
    }
    if (openat(4096, "under-nothing", O_RDONLY) != -1 || errno != EBADF) {
        return 18;
    }
    // An absolute name ignores the descriptor entirely, whatever it is.
    if (openat(fd, "/tmp/an-ordinary-file", O_RDONLY) < 0) {
        return 19;
    }
    return 0;
}
