/* Writes 64 blocks to a file and syncs it, either as one io_uring submission batch or, given the
 * argument "plain", as 64 pwrite64 calls and an fsync.
 *
 * The pair measures what a ring is for: a batch crosses into the supervisor once, where plain calls
 * cross once each. Exits 0 only if every write landed whole. Under bohr the ring form needs
 * `[linux] io_uring = "ring"` (ring_files.toml). */
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

enum { IO_URING_SETUP = 425, IO_URING_ENTER = 426, ENTRIES = 128, BLOCKS = 64, BLOCK = 512 };
enum { OP_FSYNC = 3, OP_WRITE = 23, SQE_IO_LINK = 1 << 2, ENTER_GETEVENTS = 1 };

struct sqe {
    uint8_t opcode, flags;
    uint16_t ioprio;
    int32_t fd;
    uint64_t off, addr;
    uint32_t len, op_flags;
    uint64_t user_data;
    uint16_t buf_index, personality;
    int32_t file_index;
    uint64_t pad[2];
};
struct cqe {
    uint64_t user_data;
    int32_t res;
    uint32_t flags;
};

static char block[BLOCK];

static int plain(int fd) {
    for (int i = 0; i < BLOCKS; i++)
        if (pwrite(fd, block, BLOCK, (off_t)i * BLOCK) != BLOCK)
            return 2;
    return fsync(fd) == 0 ? 0 : 3;
}

static int ring(int fd) {
    uint32_t p[30] = {0};
    int r = (int)syscall(IO_URING_SETUP, ENTRIES, p);
    if (r < 0)
        return 10;
    const uint32_t *so = &p[10], *co = &p[20];
    int prot = PROT_READ | PROT_WRITE, share = MAP_SHARED | MAP_POPULATE;
    char *sq = mmap(0, so[6] + p[0] * 4u, prot, share, r, 0);
    char *cq = mmap(0, co[5] + p[1] * sizeof(struct cqe), prot, share, r, 0x8000000);
    struct sqe *sqes = mmap(0, p[0] * sizeof(struct sqe), prot, share, r, 0x10000000);
    if (sq == MAP_FAILED || cq == MAP_FAILED || sqes == MAP_FAILED)
        return 11;
    uint32_t *tail = (uint32_t *)(sq + so[1]), mask = *(uint32_t *)(sq + so[2]);
    uint32_t *array = (uint32_t *)(sq + so[6]);
    uint32_t t = *tail;
    for (int i = 0; i <= BLOCKS; i++, t++) {
        struct sqe *e = &sqes[t & mask];
        memset(e, 0, sizeof *e);
        e->fd = fd;
        e->user_data = (uint64_t)i;
        if (i < BLOCKS) {
            e->opcode = OP_WRITE;
            e->flags = SQE_IO_LINK;
            e->addr = (uintptr_t)block;
            e->len = BLOCK;
            e->off = (uint64_t)i * BLOCK;
        } else {
            e->opcode = OP_FSYNC;
        }
        array[t & mask] = t & mask;
    }
    __atomic_store_n(tail, t, __ATOMIC_RELEASE);
    if (syscall(IO_URING_ENTER, r, BLOCKS + 1, BLOCKS + 1, ENTER_GETEVENTS, 0, 0) != BLOCKS + 1)
        return 12;
    uint32_t head = *(uint32_t *)(cq + co[0]), end = *(uint32_t *)(cq + co[1]);
    uint32_t cmask = *(uint32_t *)(cq + co[2]);
    struct cqe *cqes = (struct cqe *)(cq + co[5]);
    if (end - head != BLOCKS + 1)
        return 13;
    for (; head != end; head++) {
        struct cqe *c = &cqes[head & cmask];
        if (c->res != (c->user_data < BLOCKS ? BLOCK : 0))
            return 14;
    }
    return 0;
}

int main(int argc, char **argv) {
    memset(block, 'r', sizeof block);
    int fd = open("batch.dat", O_CREAT | O_RDWR | O_TRUNC, 0644);
    if (fd < 0)
        return 1;
    int code = argc > 1 && strcmp(argv[1], "plain") == 0 ? plain(fd) : ring(fd);
    close(fd);
    unlink("batch.dat");
    return code;
}
