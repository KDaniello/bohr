// Submits work through a ring without having one, which no correct program does.
//
// Opening a ring is answered here; entering one is not, and the difference is the whole of the
// argument. A ring that was never created has nothing to submit through, so a guest reaching this
// call is in a state this model cannot account for -- and a ring that did exist would carry work
// past the interception entirely, which is the hole the refusal is for.
#include <sys/syscall.h>
#include <unistd.h>

enum { IO_URING_ENTER = 426 };

int main(void) {
    return syscall(IO_URING_ENTER, 0, 1, 0, 0, 0, 0) < 0 ? 1 : 0;
}
