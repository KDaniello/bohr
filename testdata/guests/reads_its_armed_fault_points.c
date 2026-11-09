// A guest that asks whether its fault point is armed, the way every SDK does: by reading the names
// the supervisor put in BOHR_ARMED_FAULT_POINTS. Exits 7 when `skip_sync` is among them, 0 when it
// is not, so a run's status says what the run armed.

#include <stdlib.h>
#include <string.h>

int main(void) {
    const char *armed = getenv("BOHR_ARMED_FAULT_POINTS");
    if (!armed) return 0;
    const char *at = strstr(armed, "skip_sync");
    if (!at) return 0;
    char after = at[strlen("skip_sync")];
    int whole = (at == armed || at[-1] == ',') && (after == 0 || after == ',');
    return whole ? 7 : 0;
}
