/* Moves a file through an io_uring ring and through nothing else: open, write, sync, read back,
 * stat, close and unlink, every one a submission.
 *
 * Exits 0 only if every completion is what a kernel with the ring would post, and with a code per
 * step otherwise, so a failure names the operation that went wrong. Runs natively on any kernel
 * with io_uring, which is how the expected answers were checked; under bohr it needs
 * `[linux] io_uring = "ring"` (ring_files.toml), and without it exits 10 at setup.
 *
 * The ring is driven by raw calls and hand-laid structures rather than liburing, for the reason
 * uses_io_uring.c gives: the guests are built from committed C with no dependencies. */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

enum { IO_URING_SETUP = 425, IO_URING_ENTER = 426, ENTRIES = 8 };
enum { OP_FSYNC = 3, OP_OPENAT = 18, OP_CLOSE = 19, OP_STATX = 21, OP_READ = 22, OP_WRITE = 23,
       OP_UNLINKAT = 36 };
enum { SQE_IO_LINK = 1 << 2, ENTER_GETEVENTS = 1, STATX_SIZE = 0x200 };

/* `struct io_uring_sqe` and `struct io_uring_cqe`, as far as these operations use them. */
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

static uint32_t *sq_tail, *sq_mask, *sq_array, *cq_head, *cq_tail, *cq_mask;
static struct sqe *sqes;
static struct cqe *cqes;
static int ring;

/* The next free entry, zeroed and already published at the tail. */
static struct sqe *next(uint64_t user_data) {
    uint32_t tail = *sq_tail, slot = tail & *sq_mask;
    struct sqe *e = &sqes[slot];
    memset(e, 0, sizeof *e);
    e->user_data = user_data;
    sq_array[slot] = slot;
    __atomic_store_n(sq_tail, tail + 1, __ATOMIC_RELEASE);
    return e;
}

/* Submits what is queued, waits for as many completions, and files each result by its number. */
static int submit(unsigned n, int64_t *res) {
    if (syscall(IO_URING_ENTER, ring, n, n, ENTER_GETEVENTS, 0, 0) != (long)n)
        return -1;
    uint32_t head = *cq_head;
    while (head != __atomic_load_n(cq_tail, __ATOMIC_ACQUIRE)) {
        struct cqe *c = &cqes[head & *cq_mask];
        res[c->user_data] = c->res;
        head++;
    }
    __atomic_store_n(cq_head, head, __ATOMIC_RELEASE);
    return 0;
}

int main(void) {
    /* `struct io_uring_params`: 120 bytes, the queue offsets at words 10 and 20. */
    uint32_t params[30] = {0};
    ring = (int)syscall(IO_URING_SETUP, ENTRIES, params);
    if (ring < 0)
        return 10;
    const uint32_t *sq_off = &params[10], *cq_off = &params[20];
    size_t sq_len = sq_off[6] + params[0] * 4u, cq_len = cq_off[5] + params[1] * sizeof(struct cqe);
    int prot = PROT_READ | PROT_WRITE, share = MAP_SHARED | MAP_POPULATE;
    char *sq = mmap(0, sq_len, prot, share, ring, 0);
    char *cq = mmap(0, cq_len, prot, share, ring, 0x8000000);
    sqes = mmap(0, params[0] * sizeof(struct sqe), prot, share, ring, 0x10000000);
    if (sq == MAP_FAILED || cq == MAP_FAILED || sqes == MAP_FAILED)
        return 11;
    sq_tail = (uint32_t *)(sq + sq_off[1]);
    sq_mask = (uint32_t *)(sq + sq_off[2]);
    sq_array = (uint32_t *)(sq + sq_off[6]);
    cq_head = (uint32_t *)(cq + cq_off[0]);
    cq_tail = (uint32_t *)(cq + cq_off[1]);
    cq_mask = (uint32_t *)(cq + cq_off[2]);
    cqes = (struct cqe *)(cq + cq_off[5]);

    static const char path[] = "ring.dat";
    static const char text[] = "carried through a ring\n";
    const uint32_t len = sizeof text - 1;
    int64_t res[8] = {0};

    struct sqe *e = next(0);
    e->opcode = OP_OPENAT;
    e->fd = AT_FDCWD;
    e->addr = (uintptr_t)path;
    e->op_flags = O_CREAT | O_RDWR | O_TRUNC;
    e->len = 0644;
    if (submit(1, res) || res[0] < 0)
        return 12;
    int fd = (int)res[0];

    /* The sync is linked to the write, so it runs only if every byte landed. */
    e = next(1);
    e->opcode = OP_WRITE;
    e->flags = SQE_IO_LINK;
    e->fd = fd;
    e->addr = (uintptr_t)text;
    e->len = len;
    e = next(2);
    e->opcode = OP_FSYNC;
    e->fd = fd;
    if (submit(2, res) || res[1] != len)
        return 13;
    /* Its own code, so a sync that failed is told apart from a write that did. */
    if (res[2] != 0)
        return 19;

    char back[64] = {0};
    e = next(3);
    e->opcode = OP_READ;
    e->fd = fd;
    e->addr = (uintptr_t)back;
    e->len = len;
    if (submit(1, res) || res[3] != len || memcmp(back, text, len) != 0)
        return 14;

    unsigned char stx[256] = {0};
    e = next(4);
    e->opcode = OP_STATX;
    e->fd = AT_FDCWD;
    e->addr = (uintptr_t)path;
    e->len = STATX_SIZE;
    e->off = (uintptr_t)stx;
    if (submit(1, res) || res[4] != 0)
        return 15;
    uint64_t size;
    memcpy(&size, stx + 40, sizeof size);
    if (size != len)
        return 16;

    e = next(5);
    e->opcode = OP_CLOSE;
    e->fd = fd;
    e = next(6);
    e->opcode = OP_UNLINKAT;
    e->fd = AT_FDCWD;
    e->addr = (uintptr_t)path;
    if (submit(2, res) || res[5] != 0 || res[6] != 0)
        return 17;

    e = next(7);
    e->opcode = OP_STATX;
    e->fd = AT_FDCWD;
    e->addr = (uintptr_t)path;
    e->len = STATX_SIZE;
    e->off = (uintptr_t)stx;
    if (submit(1, res) || res[7] != -ENOENT)
        return 18;
    return 0;
}
