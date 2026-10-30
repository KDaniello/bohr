// Writes a file, reads it back at an offset, truncates it and checks what the model kept. A
// database is this pattern under a great deal of bookkeeping, and it is what the gate target does
// tens of thousands of times.
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#define SIZE 4096

int main(void) {
    int fd = open("/tmp/bohr-roundtrip", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        return 1;
    }

    char out[SIZE];
    for (int i = 0; i < SIZE; i++) {
        out[i] = (char)(i * 7 + 3);
    }
    if (pwrite(fd, out, SIZE, 0) != SIZE) {
        return 2;
    }

    char in[SIZE];
    if (pread(fd, in, SIZE, 0) != SIZE || memcmp(out, in, SIZE) != 0) {
        return 3;
    }

    // A read past the end returns what is there and no more.
    if (pread(fd, in, 64, SIZE - 16) != 16) {
        return 4;
    }
    if (ftruncate(fd, 100) != 0 || pread(fd, in, SIZE, 0) != 100) {
        return 5;
    }
    if (fsync(fd) != 0 || close(fd) != 0) {
        return 6;
    }
    return write(1, "file ok\n", 8) == 8 ? 0 : 7;
}
