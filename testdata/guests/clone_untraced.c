// A child created with `CLONE_UNTRACED`, which would leave the tracer's schedule.
//
// The flag is cleared before the call reaches the host, so the child is traced like any other and
// its write reaches the world. Untraced, its filter would find no tracer and fail every call.

#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    long child = syscall(SYS_clone, CLONE_UNTRACED | SIGCHLD, 0, 0, 0, 0);
    if (child == 0) {
        write(1, "child\n", 6);
        _exit(0);
    }
    if (child < 0) {
        return 1;
    }
    write(1, "parent\n", 7);
    return 0;
}
