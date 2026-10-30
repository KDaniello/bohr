/* A guest that replaces itself with another program.
 *
 * A cluster gives every node the same command line with its index substituted, which is right for
 * a cluster of like nodes and wrong for one where the nodes are different programs — a service and
 * the thing that drives it. Replacing itself is how one command line becomes several, and what is
 * under test is that the supervisor keeps hold of the task across it: the filter it installed, the
 * tracing it set up, and the identity the run knows it by.
 *
 * Takes the program to become and its arguments. Prints nothing itself; whatever it becomes does
 * the talking. */

#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: becomes_another <program> [args...]\n");
        return 2;
    }
    execv(argv[1], &argv[1]);
    /* Only reached where the exec failed, and then the reason is the whole of the news. */
    perror("execv");
    return 2;
}
