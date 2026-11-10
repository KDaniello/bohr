// Prints a line of progress, then the kind of line a program logs as it gives up, and exits 1.
// The run is meant to keep both lines and to read the second as where the program fell.
#include <unistd.h>

int main(void) {
    static const char progress[] = "I[0] starting the store\n";
    static const char fatal[] = "ERROR: failed to open the store at height 41: entry point missing\n";
    if (write(1, progress, sizeof progress - 1) < 0) {
        return 2;
    }
    if (write(2, fatal, sizeof fatal - 1) < 0) {
        return 2;
    }
    return 1;
}
