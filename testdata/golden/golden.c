// The golden guest: one workload per part of the supervisor, each a known number of operations.
//
//     golden <workload> <count> [node]
//
// Not a test of a program, but a load on one subsystem at a time: the syscall path, the clock,
// files and their syncs, mappings, threads and what they wait on, readiness, sockets inside a node
// and between nodes, the filter that lets allocation through, a guest that only computes. Each
// workload does exactly <count> operations, so what a run costs divides into a cost per operation;
// <count> of 0 does everything but the operations, which is how startup is taken out.
//
// Every call is checked and a failure exits 1 with the call named: a workload the supervisor
// answers wrongly is a finding, never a fast run. Offsets come from a fixed xorshift sequence, so
// a workload is the same calls on every run and anywhere.

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define PAGE 4096
#define SMALL_FILE (256 * PAGE)
#define BIG_FILE (64L * 1024 * 1024)
#define UDP_PORT 7000

static long count;

static void fail(const char *what) {
    fprintf(stderr, "golden: %s: %s\n", what, strerror(errno));
    exit(1);
}

static uint64_t state = 0x9e3779b97f4a7c15ULL;
static uint64_t next(void) {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    return state;
}

static void w_syscall(void) {
    for (long i = 0; i < count; i++) {
        if (syscall(SYS_getpid) <= 0) fail("getpid");
    }
}

static void w_clock(void) {
    struct timespec ts;
    for (long i = 0; i < count; i++) {
        if (clock_gettime(i % 2 ? CLOCK_REALTIME : CLOCK_MONOTONIC, &ts) != 0) fail("clock_gettime");
    }
}

static int open_file(const char *path) {
    int fd = open(path, O_RDWR | O_CREAT, 0644);
    if (fd < 0) fail("open");
    return fd;
}

static void write_page(int fd, off_t at) {
    static char page[PAGE];
    page[0] = (char)next();
    if (pwrite(fd, page, PAGE, at) != PAGE) fail("pwrite");
}

static void w_write(void) {
    int fd = open_file("golden-write.dat");
    for (long i = 0; i < count; i++) write_page(fd, (off_t)(i % 2048) * PAGE);
    close(fd);
}

static void w_fsync(void) {
    int fd = open_file("golden-fsync.dat");
    if (ftruncate(fd, SMALL_FILE) != 0) fail("ftruncate");
    for (long i = 0; i < count; i++) {
        write_page(fd, (off_t)(next() % (SMALL_FILE / PAGE)) * PAGE);
        if (fsync(fd) != 0) fail("fsync");
    }
    close(fd);
}

static void w_bigfile(void) {
    int fd = open_file("golden-big.dat");
    if (ftruncate(fd, BIG_FILE) != 0) fail("ftruncate");
    for (long i = 0; i < count; i++) {
        write_page(fd, (off_t)(next() % (BIG_FILE / PAGE)) * PAGE);
        if (fsync(fd) != 0) fail("fsync");
    }
    close(fd);
}

static void w_read(void) {
    int fd = open_file("golden-read.dat");
    if (ftruncate(fd, SMALL_FILE) != 0) fail("ftruncate");
    static char page[PAGE];
    for (long i = 0; i < count; i++) {
        if (pread(fd, page, PAGE, (off_t)(next() % (SMALL_FILE / PAGE)) * PAGE) != PAGE) fail("pread");
    }
    close(fd);
}

static void w_mmap(void) {
    int fd = open_file("golden-map.dat");
    if (ftruncate(fd, SMALL_FILE) != 0) fail("ftruncate");
    char *map = mmap(0, SMALL_FILE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) fail("mmap");
    for (long i = 0; i < count; i++) {
        map[(next() % (SMALL_FILE / PAGE)) * PAGE] = (char)i;
        if (msync(map, SMALL_FILE, MS_SYNC) != 0) fail("msync");
    }
    munmap(map, SMALL_FILE);
    close(fd);
}

// Two threads hand one token back and forth: each operation is one hand-over and one wake.
static int there[2], back[2];

static void *echo_pipe(void *ignored) {
    (void)ignored;
    char token;
    for (long i = 0; i < count; i++) {
        if (read(there[0], &token, 1) != 1) fail("read");
        if (write(back[1], &token, 1) != 1) fail("write");
    }
    return 0;
}

static void w_pipe(void) {
    if (pipe(there) != 0 || pipe(back) != 0) fail("pipe");
    pthread_t peer;
    if (pthread_create(&peer, 0, echo_pipe, 0) != 0) fail("pthread_create");
    char token = 't';
    for (long i = 0; i < count; i++) {
        if (write(there[1], &token, 1) != 1) fail("write");
        if (read(back[0], &token, 1) != 1) fail("read");
    }
    pthread_join(peer, 0);
}

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t turned = PTHREAD_COND_INITIALIZER;
static long turn;

static void *echo_futex(void *ignored) {
    (void)ignored;
    for (long i = 0; i < count; i++) {
        pthread_mutex_lock(&lock);
        while (turn % 2 == 0) pthread_cond_wait(&turned, &lock);
        turn++;
        pthread_cond_signal(&turned);
        pthread_mutex_unlock(&lock);
    }
    return 0;
}

static void w_futex(void) {
    pthread_t peer;
    if (pthread_create(&peer, 0, echo_futex, 0) != 0) fail("pthread_create");
    for (long i = 0; i < count; i++) {
        pthread_mutex_lock(&lock);
        turn++;
        pthread_cond_signal(&turned);
        while (turn % 2 == 1) pthread_cond_wait(&turned, &lock);
        pthread_mutex_unlock(&lock);
    }
    pthread_join(peer, 0);
}

static void *nothing(void *ignored) { return ignored; }

static void w_threads(void) {
    for (long i = 0; i < count; i++) {
        pthread_t t;
        if (pthread_create(&t, 0, nothing, 0) != 0) fail("pthread_create");
        if (pthread_join(t, 0) != 0) fail("pthread_join");
    }
}

static void w_epoll(void) {
    int ev = eventfd(0, EFD_NONBLOCK);
    int ep = epoll_create1(0);
    if (ev < 0 || ep < 0) fail("eventfd or epoll_create1");
    struct epoll_event want = {.events = EPOLLIN, .data.fd = ev};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, ev, &want) != 0) fail("epoll_ctl");
    for (long i = 0; i < count; i++) {
        uint64_t one = 1;
        if (write(ev, &one, sizeof one) != sizeof one) fail("write eventfd");
        struct epoll_event got;
        if (epoll_wait(ep, &got, 1, -1) != 1) fail("epoll_wait");
        if (read(ev, &one, sizeof one) != sizeof one) fail("read eventfd");
    }
}

static int pair[2];

static void *echo_socket(void *ignored) {
    (void)ignored;
    char message[64];
    for (long i = 0; i < count; i++) {
        if (recv(pair[1], message, sizeof message, 0) != sizeof message) fail("recv");
        if (send(pair[1], message, sizeof message, 0) != sizeof message) fail("send");
    }
    return 0;
}

static void w_socketpair(void) {
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) fail("socketpair");
    pthread_t peer;
    if (pthread_create(&peer, 0, echo_socket, 0) != 0) fail("pthread_create");
    char message[64] = "golden";
    for (long i = 0; i < count; i++) {
        if (send(pair[0], message, sizeof message, 0) != sizeof message) fail("send");
        if (recv(pair[0], message, sizeof message, MSG_WAITALL) != sizeof message) fail("recv");
    }
    pthread_join(peer, 0);
}

// Anonymous mappings made and returned directly, not through malloc: an allocator that keeps what
// it was given asks the system for nothing after its first block, and would measure nothing.
static void w_alloc(void) {
    for (long i = 0; i < count; i++) {
        char *block = mmap(0, 256 * 1024, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (block == MAP_FAILED) fail("mmap");
        block[0] = (char)i;
        if (munmap(block, 256 * 1024) != 0) fail("munmap");
    }
}

static void w_compute(void) {
    uint64_t sum = 0;
    for (long i = 0; i < count; i++) {
        for (int j = 0; j < 10000; j++) sum += next();
    }
    printf("%llu\n", (unsigned long long)sum);
}

static struct sockaddr_in node_address(int node) {
    struct sockaddr_in at = {.sin_family = AF_INET, .sin_port = htons(UDP_PORT)};
    char text[32];
    snprintf(text, sizeof text, "10.66.0.%d", node + 1);
    inet_pton(AF_INET, text, &at.sin_addr);
    return at;
}

// Two nodes: node 0 answers every datagram, node 1 sends one and waits for the answer.
static void w_udp(int node) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0) fail("socket");
    struct sockaddr_in me = node_address(node);
    if (bind(s, (struct sockaddr *)&me, sizeof me) != 0) fail("bind");
    char message[64] = "golden";
    if (node == 0) {
        for (long i = 0; i < count; i++) {
            struct sockaddr_in from;
            socklen_t len = sizeof from;
            if (recvfrom(s, message, sizeof message, 0, (struct sockaddr *)&from, &len) < 0) fail("recvfrom");
            if (sendto(s, message, sizeof message, 0, (struct sockaddr *)&from, len) < 0) fail("sendto");
        }
    } else {
        struct sockaddr_in server = node_address(0);
        // The server binds when it starts, and a datagram sent before that is lost: the client
        // waits ten milliseconds of virtual time first, which costs no real time.
        struct timespec pause = {0, 10 * 1000 * 1000};
        nanosleep(&pause, 0);
        for (long i = 0; i < count; i++) {
            if (sendto(s, message, sizeof message, 0, (struct sockaddr *)&server, sizeof server) < 0) fail("sendto");
            if (recv(s, message, sizeof message, 0) < 0) fail("recv");
        }
    }
    close(s);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: golden <workload> <count> [node]\n");
        return 2;
    }
    const char *workload = argv[1];
    count = atol(argv[2]);
    int node = argc > 3 ? atoi(argv[3]) : 0;
    struct {
        const char *name;
        void (*run)(void);
    } table[] = {
        {"syscall", w_syscall}, {"clock", w_clock},     {"write", w_write},
        {"fsync", w_fsync},     {"bigfile", w_bigfile}, {"read", w_read},
        {"mmap", w_mmap},       {"pipe", w_pipe},       {"futex", w_futex},
        {"threads", w_threads}, {"epoll", w_epoll},     {"socketpair", w_socketpair},
        {"alloc", w_alloc},     {"compute", w_compute},
    };
    if (strcmp(workload, "udp") == 0) {
        w_udp(node);
        return 0;
    }
    for (size_t i = 0; i < sizeof table / sizeof table[0]; i++) {
        if (strcmp(workload, table[i].name) == 0) {
            table[i].run();
            return 0;
        }
    }
    fprintf(stderr, "golden: no workload named %s\n", workload);
    return 2;
}
