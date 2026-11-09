# Guests are built from committed source with pinned flags, never committed as binaries: a
# committed binary makes a golden test depend on somebody's toolchain from years ago, and the
# dependency is invisible until it stops reproducing.
#
# Static, because dynamic loading is a syscall path of its own and these guests exist to exercise
# what comes after it. -O1 keeps the compiler from eliding the calls under test.

CC      ?= gcc
CFLAGS  ?= -O1 -static -fno-stack-protector -no-pie -Wall -Wextra
LDFLAGS ?= -pthread
OUT     ?= build

SOURCES := $(wildcard *.c) $(wildcard ../bugs/*/*.c) $(wildcard ../golden/*.c)
BINARIES := $(patsubst %.c,$(OUT)/%,$(notdir $(SOURCES)))
VPATH   := $(sort $(dir $(SOURCES)))

# One guest is built the way a target under search is built: instrumented, and linked against the
# runtime that fills the map. It is the only program here that exercises the coverage channel end
# to end, which is why it is built differently rather than being a second copy of an ordinary one.
# Clang names the edge with a guard variable; GCC offers only the plain form and the runtime
# derives the edge from the return address instead. Both are deterministic here, because the guest
# is static, position-dependent and run with address randomisation off.
COV_RT   := ../../sdk/c/bohr-cov-rt.c
COVFLAGS := $(if $(findstring clang,$(shell $(CC) --version 2>/dev/null)),              -fsanitize-coverage=trace-pc-guard,-fsanitize-coverage=trace-pc)

.PHONY: all clean
all: $(BINARIES)

# The runtime is compiled without the instrumentation it implements. Built with it, the function
# that records an edge records the edges inside itself, and the first edge of the program becomes
# an unbounded recursion — a guest that segfaults before it reaches its own main.
$(OUT)/bohr-cov-rt.o: $(COV_RT) | $(OUT)
	$(CC) $(CFLAGS) -c -o $@ $<

$(OUT)/covered_branches: covered_branches.c $(OUT)/bohr-cov-rt.o | $(OUT)
	$(CC) $(CFLAGS) $(COVFLAGS) -o $@ $< $(OUT)/bohr-cov-rt.o $(LDFLAGS)

# The one guest built for what it contains rather than for what it does: the instruction has to be
# in the object for the diagnosis that finds it to have anything to find.
$(OUT)/asks_the_processor_for_entropy: asks_the_processor_for_entropy.c | $(OUT)
	$(CC) $(CFLAGS) -mrdrnd -o $@ $< $(LDFLAGS)

$(OUT)/%: %.c | $(OUT)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

$(OUT):
	mkdir -p $(OUT)

clean:
	rm -rf $(OUT)
