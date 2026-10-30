/* Writes, syncs and reads back a file: the disk half of the determinism suite.
 *
 * Every block is synced before the next is written, so under strict semantics the
 * whole file is durable and the read-back is exact; a supervisor that answered
 * fsync dishonestly would change the bytes this program prints. */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

enum { BLOCK = 512, BLOCKS = 32 };

int main(void) {
    int fd = open("data.bin", O_CREAT | O_RDWR, 0644);
    if (fd < 0)
        return 1;

    char block[BLOCK];
    for (int i = 0; i < BLOCKS; i++) {
        memset(block, 'a' + (i % 26), sizeof block);
        if (write(fd, block, sizeof block) != (ssize_t)sizeof block)
            return 2;
        if (fsync(fd) != 0)
            return 3;
    }

    char back[BLOCK];
    if (pread(fd, back, sizeof back, (BLOCKS - 1) * (off_t)BLOCK) != (ssize_t)sizeof back)
        return 4;
    printf("%c..%c\n", back[0], back[BLOCK - 1]);

    close(fd);
    return 0;
}
