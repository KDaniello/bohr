// A poll with no timeout on a descriptor that is ready: Linux answers at once, and so must a world
// that holds the descriptor. A world that could not answer an endless poll refused this outright.
#include <poll.h>
#include <unistd.h>

int main(void) {
    struct pollfd out = { .fd = 1, .events = POLLOUT, .revents = 0 };
    if (poll(&out, 1, -1) != 1 || !(out.revents & POLLOUT)) {
        return 1;
    }
    write(1, "ready\n", 6);
    return 0;
}
