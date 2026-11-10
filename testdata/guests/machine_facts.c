// Prints what a guest can learn about the machine without a call the world answers.
//
// Each line is a fact the host would otherwise decide: where the stack begins, what the auxiliary
// vector says of the processor and the executed path, the stack limit, the core, the signal state
// inherited from the supervisor, and what a page reads after `MADV_FREE`. Run from two
// directories, on two cores, under two stack limits, every line must be the same.

#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/resource.h>

#ifndef AT_MINSIGSTKSZ
#define AT_MINSIGSTKSZ 51
#endif

int main(int argc, char **argv) {
    (void)argc;
    int local = 0;
    printf("stack %p\n", (void *)&local);
    printf("argv0 %s\n", argv[0]);
    const char *execfn = (const char *)getauxval(AT_EXECFN);
    printf("execfn %zu %s\n", execfn ? strlen(execfn) : 0, execfn ? execfn : "");
    printf("hwcap %lx hwcap2 %lx minsigstksz %lu\n", getauxval(AT_HWCAP), getauxval(AT_HWCAP2),
           getauxval(AT_MINSIGSTKSZ));

    struct rlimit stack;
    getrlimit(RLIMIT_STACK, &stack);
    printf("stack limit %llu\n", (unsigned long long)stack.rlim_cur);

    printf("cpu %d\n", sched_getcpu());

    struct sigaction pipe;
    sigaction(SIGPIPE, NULL, &pipe);
    sigset_t blocked;
    sigprocmask(SIG_BLOCK, NULL, &blocked);
    printf("sigpipe %s blocked %d\n", pipe.sa_handler == SIG_DFL ? "default" : "changed",
           sigisemptyset(&blocked) ? 0 : 1);

    char *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(page, 0x5a, 4096);
    madvise(page, 4096, MADV_FREE);
    printf("freed page %02x\n", (unsigned char)page[0]);
    return 0;
}
