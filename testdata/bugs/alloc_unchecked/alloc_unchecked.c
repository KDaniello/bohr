// A store that keeps each record in a mapping of its own and never checks that the mapping was
// made. The seeded defect, stated plainly: `mmap` can fail, and when it does the store writes the
// record through MAP_FAILED, which is address -1, and dies of the fault. Correct code checks the
// result and refuses the record.
//
// Nothing goes wrong while memory lasts, which is every run on a machine that does not run out:
// the defect needs an allocation refused, and the world refuses one only where a scenario asks it
// to.
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define RECORDS 600
#define RECORD_BYTES 4096

int main(void) {
    for (int i = 0; i < RECORDS; i++) {
        char *record = mmap(0, RECORD_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        // The defect: no check against MAP_FAILED before the write.
        memset(record, 'r', RECORD_BYTES);
        record[0] = (char)i;
    }
    if (write(1, "stored\n", 7) != 7) {
        return 2;
    }
    return 0;
}
