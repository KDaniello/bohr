/* The records a replica of a consensus log writes for the laws of agreement (docs/API.md §6.4):
 * what it committed, which term it claimed, how far it applied.
 *
 * Header only, so each guest is still one file to build.
 */
#ifndef BOHR_LOG_H
#define BOHR_LOG_H

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int bohr_log_out(void) {
    static int fd = -2;
    if (fd == -2) {
        const char *path = getenv("BOHR_SDK_OUTPUT");
        fd = path ? open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644) : -1;
    }
    return fd;
}

static void bohr_log_event(const char *name, const char *body) {
    char line[512];
    int len = snprintf(line, sizeof line, "{\"antithesis_event\":{\"%s\":{%s}}}\n", name, body);
    int fd = bohr_log_out();
    if (fd >= 0 && len > 0 && (size_t)len < sizeof line && write(fd, line, (size_t)len) < 0) {
        /* A record lost is the run's degradation to report, not the guest's to handle. */
    }
}

/* This node claims to lead `term`. */
static void bohr_log_leader(unsigned long term) {
    char body[64];
    snprintf(body, sizeof body, "\"term\":%lu", term);
    bohr_log_event("log.leader", body);
}

/* This node committed `entry` at `index` in `term`. */
static void bohr_log_committed(unsigned long index, const char *entry, unsigned long term) {
    char body[256];
    snprintf(body, sizeof body, "\"index\":%lu,\"entry\":\"%s\",\"term\":%lu", index, entry, term);
    bohr_log_event("log.committed", body);
}

/* This node applied up to `index`. */
static void bohr_log_applied(unsigned long index) {
    char body[64];
    snprintf(body, sizeof body, "\"index\":%lu", index);
    bohr_log_event("log.applied", body);
}

#endif
