// Edge counters for a guest that is being searched.
//
// Link this into a program built with `-fsanitize-coverage=trace-pc-guard` and the compiler calls
// into it at every edge of the control-flow graph. Each call bumps one counter in memory the
// supervisor shares, and the supervisor reads the counters once the run has ended.
//
// The whole runtime is here, and it is meant to stay small enough to read in one sitting: a guest
// author is being asked to link our code into their program, and the only honest answer to "what
// does it do" is one they can check themselves.
//
// Nothing here allocates, takes a lock, or calls anything that could block. It runs between two
// instructions of somebody else's program, possibly on several threads at once, and the cost of
// being careless is a slowdown they will blame on their own code.
//
// See docs/API.md 5.5.

#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

// The size of the private buffer used when there is no supervisor at all. Matches
// DEFAULT_COVERAGE_MAP_BYTES in crates/bohr-backend/src/coverage.rs.
#define BOHR_DEFAULT_CELLS 65536

// The most counters a supervisor may ask for, which is how much address space is reserved for the
// region before its header has been read. Reserving is free: nothing beyond the file is touched,
// and the file is only ever as large as the supervisor made it.
#define BOHR_MAX_CELLS (64u * 1024u * 1024u)

// Bytes of header before the counters: four little-endian 64-bit words. The supervisor writes the
// magic and the number of counters; this runtime writes the program's edge count. The size comes
// through the region rather than through the environment on purpose — in the environment it would
// be part of what the program was started with, and the same run searched with two map sizes would
// be two different runs.
#define BOHR_HEADER_BYTES 32
#define BOHR_MAGIC 0x00424F4852435601ULL

// Where the supervisor says it left the counters.
#define BOHR_FD_VAR "BOHR_COV_FD"

// The counters. Points into the shared mapping once opened, and at a private scratch buffer
// otherwise, so that a program built with this runtime and run without a supervisor pays one
// predictable store per edge rather than a branch and a store.
static uint8_t bohr_unmapped[BOHR_DEFAULT_CELLS];
static uint8_t *bohr_map = bohr_unmapped;
static uint32_t bohr_cells = BOHR_DEFAULT_CELLS;

// The header, once there is one to write into, and the edge count to put there.
static uint8_t *bohr_header;
static uint32_t bohr_edges;

// Records how many edges this program has, where a supervisor is listening.
//
// Written on every guard range and again when the map opens, because the order of a constructor
// against the compiler's own initialisers is not something either side gets to choose.
static void bohr_publish_edges(void) {
    if (!bohr_header) {
        return;
    }
    uint64_t count = bohr_edges;
    for (unsigned i = 0; i < 8; i++) {
        bohr_header[16 + i] = (uint8_t)(count >> (8 * i));
    }
}

// One little-endian 64-bit word of the header.
static uint64_t bohr_word(const uint8_t *at) {
    uint64_t value = 0;
    for (unsigned i = 0; i < 8; i++) {
        value |= (uint64_t)at[i] << (8 * i);
    }
    return value;
}

// A positive number named by an environment variable, or zero where it names none.
static long bohr_from_env(const char *name) {
    const char *named = getenv(name);
    if (!named) {
        return 0;
    }
    char *end = NULL;
    long value = strtol(named, &end, 10);
    if (end == named || *end != '\0' || value < 0) {
        return 0;
    }
    return value;
}

// The previous edge, shifted. Thread-local because two threads walking different paths would
// otherwise each see the other's last location and record edges neither of them took.
static __thread uint32_t bohr_prev;

// Opens the shared counters, if a supervisor left any.
//
// A constructor rather than a lazy check on the first edge: the first edge happens inside the
// dynamic loader on some builds, and a lazy path there is a lock nobody asked for.
__attribute__((constructor)) static void bohr_cov_open(void) {
    if (!getenv(BOHR_FD_VAR)) {
        return;
    }
    long fd = bohr_from_env(BOHR_FD_VAR);
    // The largest region a supervisor may have made, mapped before its header can say how large it
    // really is. Only address space is reserved; nothing past the file is ever touched, because
    // every access below is bounded by the counter count the header carries.
    size_t reserved = BOHR_HEADER_BYTES + (size_t)BOHR_MAX_CELLS;
    void *shared = mmap(NULL, reserved, PROT_READ | PROT_WRITE, MAP_SHARED, (int)fd, 0);
    if (shared == MAP_FAILED) {
        return;
    }
    uint8_t *region = (uint8_t *)shared;
    uint64_t cells = bohr_word(region + 8);
    // A runtime and a supervisor of different ages meeting over one descriptor: without the magic
    // this would read whatever was there as a length and index off the end of the mapping.
    if (bohr_word(region) != BOHR_MAGIC || cells == 0 || cells > BOHR_MAX_CELLS) {
        munmap(shared, reserved);
        return;
    }
    bohr_header = region;
    bohr_map = region + BOHR_HEADER_BYTES;
    bohr_cells = (uint32_t)cells;
    bohr_publish_edges();
}

// Called once per instrumented edge, with a token identifying it.
//
// The index is the pair rather than the destination: a block reached from two different callers is
// two edges, and collapsing them to one is what makes a coverage map stop distinguishing paths.
void __sanitizer_cov_trace_pc_guard(uint32_t *guard) {
    uint32_t here = *guard;
    uint32_t at = (bohr_prev ^ here) % bohr_cells;
    // Saturating: a counter that wrapped to zero would report an edge walked 256 times as one
    // never walked at all, which is the one wrong answer this map must not give.
    if (bohr_map[at] != 0xFF) {
        bohr_map[at]++;
    }
    bohr_prev = here >> 1;
}

// The same, for compilers that offer no guard.
//
// GCC implements `-fsanitize-coverage=trace-pc` and not the guard form, so the location has to
// come from somewhere else: the return address is where the edge is, and under the conditions a
// guest already runs in — statically linked, position-dependent, address randomisation off — it is
// the same number on every run. Those conditions are not a coincidence; they are what makes any of
// this reproducible, and the coverage map inherits them.
void __sanitizer_cov_trace_pc(void) {
    uintptr_t here = (uintptr_t)__builtin_return_address(0);
    uint32_t folded = (uint32_t)(here ^ (here >> 32));
    uint32_t at = (bohr_prev ^ folded) % bohr_cells;
    if (bohr_map[at] != 0xFF) {
        bohr_map[at]++;
    }
    bohr_prev = folded >> 1;
}

// Called once per compilation unit with the range of guards belonging to it.
//
// Each guard is given a number, so that what identifies an edge is its position in the program
// rather than an address the loader chose. Two runs of one binary therefore agree on every index,
// which is what makes the map comparable between runs — and comparing maps is the entire point.
void __sanitizer_cov_trace_pc_guard_init(uint32_t *start, uint32_t *stop) {
    static uint32_t next = 1;
    for (uint32_t *guard = start; guard < stop; guard++) {
        *guard = next++;
    }
    // What the supervisor cannot work out for itself: how many edges there are to cover, and
    // therefore whether the map it mapped is big enough to tell them apart.
    bohr_edges = next - 1;
    bohr_publish_edges();
}
