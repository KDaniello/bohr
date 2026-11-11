/* bohr's vDSO: the clock a guest reads without a system call.
 *
 * The supervisor writes the node's clock into the page one page below this image before every
 * resume, and bohr runs one task at a time, so a read here never races a write. Each read costs
 * what a `clock_gettime` answered by the world costs: the k-th read since the resume returns the
 * clock plus k steps, and the supervisor charges the reads to the node's time at the next stop.
 * Where the page holds nothing yet, the call is made as a system call, which stops as without a
 * vDSO.
 *
 * Built freestanding and position independent, with no relocation: the image is copied into the
 * guest, never loaded, so the page is found from the image's own header.
 */
#include <stdint.h>

struct timespec_ {
    int64_t tv_sec;
    int64_t tv_nsec;
};

struct timeval_ {
    int64_t tv_sec;
    int64_t tv_usec;
};

/* The page the supervisor writes, in nanoseconds as the world presents them, and the reads made
 * since, which this image counts. */
struct clock_page {
    uint64_t valid;
    uint64_t monotonic_ns;
    uint64_t wall_ns;
    uint64_t reads;
    uint64_t step_ns;
};

extern const char __ehdr_start[] __attribute__((visibility("hidden")));

static inline volatile struct clock_page *page(void) {
    return (volatile struct clock_page *)(__ehdr_start - 4096);
}

/* One read: the clock plus the steps of the reads before it since the resume. */
static uint64_t read_ns(volatile struct clock_page *p, int wall) {
    uint64_t k = p->reads;
    p->reads = k + 1;
    return (wall ? p->wall_ns : p->monotonic_ns) + k * p->step_ns;
}

static long sys3(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}

enum { CLOCK_GETTIME = 228, GETTIMEOFDAY = 96, TIME = 201, GETCPU = 309 };

/* The world's split: these ids read the wall clock, which a step moves; every other id reads the
 * node's elapsed time. */
static int reads_wall(int id) {
    return id == 0 || id == 5 || id == 8 || id == 11;
}

int __vdso_clock_gettime(int id, struct timespec_ *ts) {
    volatile struct clock_page *p = page();
    if (!p->valid)
        return (int)sys3(CLOCK_GETTIME, id, (long)ts, 0);
    uint64_t ns = read_ns(p, reads_wall(id));
    ts->tv_sec = (int64_t)(ns / 1000000000u);
    ts->tv_nsec = (int64_t)(ns % 1000000000u);
    return 0;
}

int __vdso_gettimeofday(struct timeval_ *tv, void *tz) {
    volatile struct clock_page *p = page();
    if (!p->valid || tz)
        return (int)sys3(GETTIMEOFDAY, (long)tv, (long)tz, 0);
    if (tv) {
        uint64_t ns = read_ns(p, 1);
        tv->tv_sec = (int64_t)(ns / 1000000000u);
        tv->tv_usec = (int64_t)(ns % 1000000000u / 1000u);
    }
    return 0;
}

int64_t __vdso_time(int64_t *t) {
    volatile struct clock_page *p = page();
    if (!p->valid)
        return sys3(TIME, (long)t, 0, 0);
    int64_t seconds = (int64_t)(read_ns(p, 1) / 1000000000u);
    if (t)
        *t = seconds;
    return seconds;
}

/* The CPU is the world's answer, so it is asked. */
long __vdso_getcpu(unsigned *cpu, unsigned *node, void *cache) {
    return sys3(GETCPU, (long)cpu, (long)node, (long)cache);
}
