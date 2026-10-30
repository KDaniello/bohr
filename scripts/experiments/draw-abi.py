#!/usr/bin/env python3
"""Measures the encoding properties of candidate draw ABIs (open question 2).

The question is how many tape bytes a draw consumes. Three candidates:

  A  fixed byte width   bytes = f(n)            — the current spec, API.md 2.4
  B  minimal width      bytes = f(n, value)     — varint, shorter tapes
  C  bit packed         bits  = ceil(log2 n)    — shortest, still value-independent

What is measured here are properties of the ENCODING, not of a bug model: tape
length for a stated draw mix, and what a single byte flip does. Those are exact
and do not depend on guessing what bugs look like. The search-effectiveness half
of the question still needs a real corpus at M1.

Usage: python3 scripts/experiments/draw-abi.py
"""

import random

# Draw mix for one run. Weights are a stated assumption, not a measurement:
# scheduler picks dominate because there is one per switch point, and network
# draws follow packet volume. Revisit against a real corpus at M1.
DRAW_MIX = [
    # Runnable task count varies run to run and is rarely a power of two.
    # That matters: 256 % n != 0 is what makes the rejection loop fire.
    ("SchedPick", 0.70, ("range_var", 2, 8)),
    ("NetDeliver", 0.08, ("bool", 1, 1000)),  # drop_probability = "1/1000"
    ("NetDelay", 0.08, ("range", 2**32)),  # latency draw
    ("DiskTear", 0.04, ("bool", 1, 50)),
    ("NodeFault", 0.02, ("bool", 1, 10000)),
    ("GuestRandom", 0.08, ("bytes", 8)),
]


def width_bytes(n):
    """Bytes needed to hold n-1. The `w` of API.md 2.4."""
    if n <= 1:
        return 0
    return ((n - 1).bit_length() + 7) // 8


def width_bits(n):
    if n <= 1:
        return 0
    return (n - 1).bit_length()


class TapeA:
    """Fixed byte width: consumption depends only on n."""

    name = "A fixed byte width"

    def __init__(self, data):
        self.data, self.pos = data, 0

    def _uint(self, w):
        v = int.from_bytes(self.data[self.pos : self.pos + w], "little")
        self.pos += w
        return v

    def range(self, n):
        if n <= 1:
            return 0
        w = width_bytes(n)
        span = 1 << (8 * w)
        limit = span - (span % n)
        for _ in range(8):
            v = self._uint(w)
            if v < limit:
                return v % n
        return self._uint(w) % n

    def raw(self, k):
        v = self.data[self.pos : self.pos + k]
        self.pos += k
        return v


class TapeB(TapeA):
    """Minimal width: a continuation-bit varint, so small values cost less."""

    name = "B minimal width"

    # One byte carries seven payload bits; the eighth says whether another follows.
    PAYLOAD_PER_BYTE = 7
    CONTINUATION = 0x80
    SINGLE_BYTE_MAX = 128
    ACC_BITS = 64

    def range(self, n):
        if n <= 1:
            return 0
        if n <= self.SINGLE_BYTE_MAX:  # single byte covers it, same as A
            return self._uint(1) % n
        acc, shift = 0, 0
        while True:
            b = self._uint(1)
            acc |= (b & (self.CONTINUATION - 1)) << shift
            shift += self.PAYLOAD_PER_BYTE
            if not (b & self.CONTINUATION) or shift >= self.ACC_BITS:
                return acc % n


class TapeC:
    """Bit packed: consumption is ceil(log2 n) bits, still value-independent."""

    name = "C bit packed"

    def __init__(self, data):
        self.data, self.bitpos = data, 0

    def _bits(self, k):
        v = 0
        for i in range(k):
            byte = self.data[(self.bitpos + i) >> 3]
            v |= ((byte >> ((self.bitpos + i) & 7)) & 1) << i
        self.bitpos += k
        return v

    def range(self, n):
        if n <= 1:
            return 0
        k = width_bits(n)
        span = 1 << k
        limit = span - (span % n)
        for _ in range(8):
            v = self._bits(k)
            if v < limit:
                return v % n
        return self._bits(k) % n

    def raw(self, k):
        v = self._bits(k * 8)
        return v

    @property
    def pos(self):
        return (self.bitpos + 7) // 8


class TapeA2(TapeA):
    """A, but a boolean costs a fixed 4 bytes instead of draw_range(den).

    Under A the width of a boolean is width_bytes(den), so editing a probability
    literal in the config re-lays-out every tape in the corpus. Scaling a fixed
    32-bit draw makes the layout independent of the literal.
    """

    name = "A2 fixed + scaled bool"

    def boolean(self, num, den):
        return self._uint(4) < (num << 32) // den


class TapeA3(TapeA):
    """A2 with a 2-byte boolean: granularity 1/65536, denominators capped there.

    Cheaper than A2 in tape and in wasted entropy. The price is that the config
    validator must reject a denominator above 65536 rather than silently rounding
    a probability to never.
    """

    name = "A3 fixed + 2-byte bool"

    def boolean(self, num, den):
        return self._uint(2) < (num << 16) // den


class TapeA4(TapeA):
    """Fixed width, plain modulo, no rejection loop.

    Rejection is the ONLY thing that makes consumption depend on the drawn value
    under A, so dropping it makes consumption exactly f(n) by construction rather
    than by luck. The price is modulo bias, bounded by n / 2^(8w) — at most 2.7%
    relative for the small n a scheduler draws.
    """

    name = "A4 no rejection"

    def range(self, n):
        if n <= 1:
            return 0
        return self._uint(width_bytes(n)) % n


class TapeA5(TapeA4):
    """A4 plus the 2-byte scaled boolean. The full proposal."""

    name = "A5 = A4 + 2-byte bool"

    def boolean(self, num, den):
        return self._uint(2) < (num << 16) // den


def make_requests(count, rng):
    """One run's sequence of draw requests, sampled from the mix."""
    names = [m[0] for m in DRAW_MIX]
    weights = [m[1] for m in DRAW_MIX]
    specs = {m[0]: m[2] for m in DRAW_MIX}
    out = []
    for _ in range(count):
        spec = specs[rng.choices(names, weights)[0]]
        if spec[0] == "range_var":
            spec = ("range", rng.randint(spec[1], spec[2]))
        out.append(spec)
    return out


def replay(tape_cls, data, requests):
    """Runs a fixed request sequence against a tape; returns decisions and bytes used."""
    t = tape_cls(data)
    out = []
    for req in requests:
        if req[0] == "range":
            out.append(t.range(req[1]))
        elif req[0] == "bool":
            if hasattr(t, "boolean"):
                out.append(1 if t.boolean(req[1], req[2]) else 0)
            else:
                out.append(1 if t.range(req[2]) < req[1] else 0)
        else:
            out.append(t.raw(req[1]))
    return out, t.pos


# Workload: draws per run, runs per encoding, byte flips per run.
DRAWS, TRIALS, FLIPS = 10_000, 20, 400


def main():
    rng = random.Random(20260809)

    print(f"draw mix: {DRAWS} draws per run, {TRIALS} runs, seed fixed\n")
    print("A realigning flip shifts every later draw, so its decision count is huge and")
    print("swamps the mean. The two populations are reported separately.\n")
    print(f"{'encoding':<22} {'bytes/run':>10} {'vs A':>7} {'flip acts':>10} "
          f"{'realign%':>9} {'aligned: mean':>14} {'median':>7}")
    print("-" * 86)

    baseline = None
    for cls in (TapeA, TapeB, TapeC, TapeA2, TapeA3, TapeA4, TapeA5):
        total_bytes, acted, realigned = 0, 0, 0
        aligned_diffs = []
        for _ in range(TRIALS):
            data = bytes(rng.getrandbits(8) for _ in range(DRAWS * 8))
            requests = make_requests(DRAWS, rng)
            base, used = replay(cls, data, requests)
            total_bytes += used

            for _ in range(FLIPS):
                i = rng.randrange(used)
                mutated = bytearray(data)
                mutated[i] ^= 1 << rng.randrange(8)
                after, used2 = replay(cls, bytes(mutated), requests)
                diff = sum(1 for a, b in zip(base, after) if a != b)
                if diff:
                    acted += 1
                # Same request sequence, different byte consumption => every later
                # draw reads from a shifted offset. Under A this can only come from
                # the rejection loop retrying, which is itself value-dependent.
                if used2 != used:
                    realigned += 1
                elif diff:
                    aligned_diffs.append(diff)

        per_run = total_bytes / TRIALS
        baseline = baseline or per_run
        n_flips = TRIALS * FLIPS
        aligned_diffs.sort()
        mean = sum(aligned_diffs) / max(len(aligned_diffs), 1)
        median = aligned_diffs[len(aligned_diffs) // 2] if aligned_diffs else 0
        print(f"{cls.name:<22} {per_run:>10.0f} {per_run / baseline:>6.2f}x "
              f"{acted / n_flips:>9.1%} {realigned / n_flips:>8.2%} "
              f"{mean:>14.2f} {median:>7}")

    # What dropping the rejection loop costs: a bounded skew, not a lost option.
    print("\nmodulo bias without rejection, worst case per draw:")
    for n in (2, 3, 5, 7, 8, 16, 1000, 65536, 2**32):
        w = width_bytes(n)
        span = 1 << (8 * w)
        print(f"  n={n:<11} w={w}  bias <= {(span % n) / span:.4%}")

    # A probability literal must not change tape layout: a corpus collected with
    # drop_probability = "1/1000" has to stay meaningful after tuning it.
    print("\ndraw_bool width by denominator, current spec (draw_range(den)):")
    for den in (2, 50, 1000, 10_000, 100_000, 1_000_000):
        print(f"  1/{den:<9} -> {width_bytes(den)} bytes")


if __name__ == "__main__":
    main()
