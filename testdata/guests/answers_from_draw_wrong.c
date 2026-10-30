/* The same program, answering with one byte flipped.
 *
 * The seeded disagreement: handed the same bytes as its twin, it answers differently — which is
 * exactly what a differential run exists to find, and what nothing but a comparison could find,
 * since neither program crashes, asserts, or exits badly.
 *
 * Nothing about the defect is known to the tool. What the tool is told is that these two implement
 * one specification and that their answers must match.
 */
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

enum { DRAWN = 32 };

int main(void) {
    unsigned char drawn[DRAWN];
    int source = open("/dev/urandom", O_RDONLY);
    if (source < 0 || read(source, drawn, sizeof drawn) != (ssize_t)sizeof drawn)
        return 2;

    char answer[17];
    for (int i = 0; i < 8; i++)
        snprintf(answer + i * 2, 3, "%02x", drawn[i] ^ (i == 0 ? 1 : 0));

    int out = open("/answer", O_CREAT | O_WRONLY, 0644);
    if (out < 0 || write(out, answer, 16) != 16)
        return 2;
    return 0;
}
