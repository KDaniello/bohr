/* The names and version a vDSO exports on x86-64, which glibc, musl and the Go runtime look up. */
LINUX_2.6 {
    global:
        __vdso_clock_gettime;
        __vdso_gettimeofday;
        __vdso_time;
        __vdso_getcpu;
    local: *;
};
