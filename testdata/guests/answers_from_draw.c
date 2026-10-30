/* Answers with bytes the run gave it, and writes the answer where a projection can read it.
 *
 * The shape of every differential target in miniature: two implementations handed the same source
 * of randomness must produce the same answer, and one that does not has disagreed with the other
 * about something the specification says they cannot disagree about.
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

    /* The answer: the first eight bytes, as hex. Where an implementation is free to differ is in
     * how it gets here, and this one adds nothing to what it was given. */
    char answer[17];
    for (int i = 0; i < 8; i++)
        snprintf(answer + i * 2, 3, "%02x", drawn[i]);

    int out = open("/answer", O_CREAT | O_WRONLY, 0644);
    if (out < 0 || write(out, answer, 16) != 16)
        return 2;
    return 0;
}
