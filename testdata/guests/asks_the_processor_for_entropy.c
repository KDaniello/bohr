// Takes a number from the processor, which is the one source no supervisor below a hypervisor can
// intercept.
//
// Built for the diagnosis rather than for the run: what matters is that the instruction is in the
// object where a reader can be told about it. A scan that looks for two bytes finds this and finds
// a dozen displacements besides; a scan that measures instructions finds this and nothing else.
// Getting that wrong in the generous direction wastes a reader's afternoon, and in the strict
// direction it reports a hole as solid ground -- which is what happened to `libstdc++`, whose five
// sites went unreported until the rule was that only a judged candidate may be dropped.

#include <immintrin.h>
#include <stdint.h>

int main(void) {
    unsigned int taken = 0;
    // The value is not used for anything: the point is the instruction, not the number.
    if (!_rdrand32_step(&taken)) {
        return 1;
    }
    return 0;
}
