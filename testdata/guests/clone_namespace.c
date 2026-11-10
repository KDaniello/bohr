// A child in a namespace of its own, which the world does not answer for: refused by name.

#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    long child = syscall(SYS_clone, CLONE_NEWUTS | SIGCHLD, 0, 0, 0, 0);
    if (child == 0) {
        _exit(0);
    }
    return 0;
}
