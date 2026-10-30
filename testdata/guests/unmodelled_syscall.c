// Asks for a network socket, which no world model in this build offers. The run must stop and name
// the call, because a syscall that quietly reaches the host is a hole in determinism that nothing
// downstream would notice.
#include <sys/socket.h>

int main(void) {
    return socket(AF_INET, SOCK_STREAM, 0) < 0 ? 1 : 0;
}
