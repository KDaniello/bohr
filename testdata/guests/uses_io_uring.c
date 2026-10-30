// Asks the kernel for an asynchronous ring, and is told this kernel has none.
//
// Declined rather than refused, and the guest checks which: `ENOSYS` is what a kernel built
// without the option answers, and every program that opens a ring keeps a path for exactly that
// reply. A refusal of the whole run would have been a different claim -- that the program cannot
// be examined at all -- and it stopped a real target inside its own snapshot extraction.
//
// The call is made raw rather than through liburing, because the guests here are built from
// committed C with no dependencies, and because what is under test is the number rather than the
// library that happens to produce it.
#include <errno.h>
#include <sys/syscall.h>
#include <unistd.h>

enum { IO_URING_SETUP = 425, ENTRIES = 8 };

int main(void) {
    // The parameter block io_uring_setup fills in. Zeroed rather than populated: the call is
    // answered before anything reads it.
    unsigned long params[16] = {0};
    errno = 0;
    if (syscall(IO_URING_SETUP, ENTRIES, params) != -1) {
        return 1;
    }
    return errno == ENOSYS ? 0 : 2;
}
