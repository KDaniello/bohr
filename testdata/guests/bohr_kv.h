/* The records a store's harness writes for the laws of durability (docs/API.md §6.5).
 *
 * Header only, so each guest is still one file to build. Each record names the line of the
 * acknowledgement, so a finding is placed at it. No `bohr` metadata is written: a restarted node
 * counts its records from one again, which the world would read as records lost.
 */
#ifndef BOHR_KV_H
#define BOHR_KV_H

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int bohr_kv_out(void) {
    static int fd = -2;
    if (fd == -2) {
        const char *path = getenv("BOHR_SDK_OUTPUT");
        fd = path ? open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644) : -1;
    }
    return fd;
}

static void bohr_kv_event(const char *name, const char *body) {
    char line[2048];
    int len = snprintf(line, sizeof line, "{\"antithesis_event\":{\"%s\":{%s}}}\n", name, body);
    int fd = bohr_kv_out();
    if (fd >= 0 && len > 0 && (size_t)len < sizeof line && write(fd, line, (size_t)len) < 0) {
        /* A record lost is the run's degradation to report, not the guest's to handle. */
    }
}

/* One operation begun (`kv.begun`) or acknowledged (`kv.acked`): `op` is put, delete or batch;
 * `values[i]` is null for a removal. */
static void bohr_kv_op(const char *stage, const char *op, const char *const *keys,
                       const char *const *values, int n, unsigned long seq, const char *file,
                       int line) {
    char keys_text[512] = "", values_text[512] = "", body[1400];
    for (int i = 0; i < n; i++) {
        size_t k = strlen(keys_text), v = strlen(values_text);
        snprintf(keys_text + k, sizeof keys_text - k, "%s\"%s\"", i ? "," : "", keys[i]);
        if (values && values[i])
            snprintf(values_text + v, sizeof values_text - v, "%s\"%s\"", i ? "," : "", values[i]);
        else
            snprintf(values_text + v, sizeof values_text - v, "%snull", i ? "," : "");
    }
    snprintf(body, sizeof body,
             "\"op\":\"%s\",\"keys\":[%s],\"values\":[%s],\"seq\":%lu,"
             "\"at\":{\"file\":\"%s\",\"line\":%d}",
             op, keys_text, values_text, seq, file, line);
    bohr_kv_event(stage, body);
}

#define BOHR_KV_PUT(stage, key, value, seq)                                                    \
    do {                                                                                       \
        const char *bohr_k[1] = {(key)}, *bohr_v[1] = {(value)};                                \
        bohr_kv_op((stage), "put", bohr_k, bohr_v, 1, (seq), __FILE__, __LINE__);               \
    } while (0)

#define BOHR_KV_DELETE(stage, key, seq)                                                        \
    do {                                                                                       \
        const char *bohr_k[1] = {(key)};                                                       \
        bohr_kv_op((stage), "delete", bohr_k, 0, 1, (seq), __FILE__, __LINE__);                 \
    } while (0)

#define BOHR_KV_BATCH(stage, keys, values, n, seq)                                             \
    bohr_kv_op((stage), "batch", (keys), (values), (n), (seq), __FILE__, __LINE__)

/* A key read back: its value, or null where it is absent. */
static void bohr_kv_read(const char *key, const char *value) {
    char body[512];
    if (value)
        snprintf(body, sizeof body, "\"key\":\"%s\",\"value\":\"%s\"", key, value);
    else
        snprintf(body, sizeof body, "\"key\":\"%s\",\"value\":null", key);
    bohr_kv_event("kv.read", body);
}

/* The store answered its first question since this start. */
static void bohr_kv_ready(void) { bohr_kv_event("kv.ready", ""); }

#endif
