// Names that stand for names, numbers that stand for open files, and the two oldest spellings of
// asking the time and taking a directory away.
//
// Every check is made by the guest, so a wrong answer is an exit code rather than a run that
// passed while the model invented something. The numbers are distinct because the point of the
// guest is to say which promise broke, not that one did.

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int write_file(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = write(fd, text, strlen(text));
    close(fd);
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

int main(int argc, char **argv) {
    char buf[512];

    if (write_file("/tmp/target-file", "behind the link") != 0) {
        return 1;
    }

    // A relative target names a file beside the link, not beside the working directory.
    if (symlink("target-file", "/tmp/the-link") != 0) {
        return 2;
    }
    if (symlink("target-file", "/tmp/the-link") != -1 || errno != EEXIST) {
        return 3;
    }

    // Opening follows the link; reading gives what the target holds.
    int fd = open("/tmp/the-link", O_RDONLY);
    if (fd < 0) {
        return 4;
    }
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    if (n != 15 || memcmp(buf, "behind the link", 15) != 0) {
        return 5;
    }

    // `readlink` acts on the link itself and answers the text it was made with, unterminated.
    memset(buf, 'x', sizeof buf);
    n = readlink("/tmp/the-link", buf, sizeof buf);
    if (n != 11 || memcmp(buf, "target-file", 11) != 0 || buf[11] != 'x') {
        return 6;
    }
    // A file is not a link, and a name that is not there is not a file. Callers branch on the
    // difference, and both spellings of the call have to make it the same way.
    if (readlink("/tmp/target-file", buf, sizeof buf) != -1 || errno != EINVAL) {
        return 7;
    }
    if (readlink("/tmp/nowhere-at-all", buf, sizeof buf) != -1 || errno != ENOENT) {
        return 8;
    }
    // Where the program is. Absolute whatever it was started with: a static `glibc` reads this
    // before `main` and calls `abort` on a relative answer, so a program started by a relative
    // path would never run at all.
    memset(buf, 0, sizeof buf);
    n = readlink("/proc/self/exe", buf, sizeof buf - 1);
    if (n <= 0 || buf[0] != '/') {
        return 9;
    }
    if (argc >= 1 && argv[0][0] == '/' && strcmp(buf, argv[0]) != 0) {
        return 19;
    }

    // A duplicate onto a standard descriptor is how a program sends its own output to a file, and
    // what it writes afterwards has to land there rather than on the console.
    fd = open("/tmp/redirected", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || dup2(fd, 1) != 1) {
        return 10;
    }
    if (write(1, "through the duplicate", 21) != 21) {
        return 11;
    }
    close(fd);
    close(1);
    fd = open("/tmp/redirected", O_RDONLY);
    if (fd < 0) {
        return 12;
    }
    n = read(fd, buf, sizeof buf);
    close(fd);
    if (n != 21 || memcmp(buf, "through the duplicate", 21) != 0) {
        return 13;
    }
    if (dup2(4096, 5) != -1 || errno != EBADF) {
        return 14;
    }

    // Whole seconds, returned and written through the pointer, and the same instant both ways.
    time_t written = 0;
    time_t returned = time(&written);
    if (returned < 0 || returned != written) {
        return 15;
    }

    // A directory goes away by being one; a file refuses the call that asks for a directory.
    if (mkdir("/tmp/a-directory", 0755) != 0) {
        return 16;
    }
    if (syscall(SYS_rmdir, "/tmp/a-directory") != 0) {
        return 17;
    }
    if (syscall(SYS_rmdir, "/tmp/target-file") != -1 || errno != ENOTDIR) {
        return 18;
    }
    return 0;
}
