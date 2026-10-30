// Reads through a null pointer. The guest is meant to die, and the run is meant to report that it
// did: a fault the supervisor swallows leaves the guest faulting on the same instruction forever.
int main(void) {
    volatile int *nowhere = 0;
    return *nowhere;
}
