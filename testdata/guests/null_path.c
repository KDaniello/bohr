// A path argument the guest deliberately does not give.
//
// `glibc` finds out whether a kernel has `statx` by calling it with a null path and reading
// `EFAULT` as yes, then falls back to `newfstatat` with a null path for the same purpose. Both
// happen before a program reaches its own first instruction, so a supervisor that read the null
// pointer as memory it had failed to fetch — and ended the run blaming itself — refused every
// program on the system that is linked the ordinary way. Measured on `python3` and on
// `solana-test-validator`, neither of which got as far as printing its version.
//
// Only `newfstatat` is asserted on. `statx` is declined by the model rather than answered, and
// what a declined call returns is that model's business rather than this guest's.

#include <errno.h>
#include <stddef.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    char statbuf[512];

    errno = 0;
    long rc = syscall(SYS_newfstatat, 0, NULL, statbuf, 0);
    if (rc != -1) {
        return 1;
    }
    if (errno != EFAULT) {
        return 2;
    }

    // And a name that is there still resolves: the null answer must not have cost the reader its
    // ordinary one.
    errno = 0;
    rc = syscall(SYS_newfstatat, 0, "/nowhere-at-all", statbuf, 0);
    if (rc != -1 || errno != ENOENT) {
        return 3;
    }
    return 0;
}
