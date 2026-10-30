// A guest whose control flow depends on what it draws, built with coverage instrumentation.
//
// The one guest that gives a search something to follow: every other program here walks the same
// path whatever the tape says, so its coverage map is the same on every seed. Four paths through
// two coin flips is the smallest thing that is not that.
//
// Built with -fsanitize-coverage=trace-pc-guard and linked against sdk/c/bohr-cov-rt.c, which is
// how a target under search is meant to be built. See docs/API.md 5.5.

#include <stdio.h>
#include <sys/random.h>

static int left(unsigned char x) { return x + 1; }
static int right(unsigned char x) { return x * 3; }
static int deeper(unsigned char x) { return x ^ 0x5A; }
static int shallower(unsigned char x) { return x & 0x0F; }

int main(void) {
    unsigned char coins[2] = {0, 0};
    if (getrandom(coins, sizeof coins, 0) != (ssize_t)sizeof coins) {
        return 1;
    }

    int acc = (coins[0] & 1) ? left(coins[0]) : right(coins[0]);
    acc += (coins[1] & 1) ? deeper(coins[1]) : shallower(coins[1]);

    printf("%d\n", acc);
    return 0;
}
