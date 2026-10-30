#!/usr/bin/env python3
"""Derives the frozen fixed-point artifacts of draw_exponential (API.md 2.4).

Two outputs, both committed:

  --table    the Q32 ln table pasted into crates/bohr-tape/src/fixed.rs
  --golden   the vectors in testdata/tape/exponential.golden

The golden vectors are an independent derivation of the normative integer
algorithm, not of the mathematical function: the Rust implementation must match
them bit for bit, interpolation error and all. Regenerating with this script
must therefore be byte-stable forever; the table is computed with 50 decimal
digits so that rounding to the nearest integer is unambiguous.

Usage: python3 scripts/experiments/exponential-table.py --table|--golden
"""

import sys
from decimal import Decimal, getcontext

getcontext().prec = 50

Q32 = 1 << 32


def ln_q32(x: Decimal) -> int:
    # Round half to even; ties cannot occur for these transcendental values,
    # the mode is stated only so the artifact is reproducible by definition.
    return int((x.ln() * Q32).to_integral_value(rounding="ROUND_HALF_EVEN"))


TABLE = [ln_q32(Decimal(1) + Decimal(k) / 256) for k in range(257)]


def exp_inverse_cdf(mean: int, u: int) -> int:
    """The normative integer algorithm, mirrored by fixed::exp_inverse_cdf."""
    if u == 0 or mean == 0:
        return 0
    v = Q32 - u
    bitlen = v.bit_length()
    m32 = v << (32 - bitlen)
    frac31 = m32 - (1 << 31)
    k = frac31 >> 23
    rem = frac31 & ((1 << 23) - 1)
    ln_m = TABLE[k] + ((TABLE[k + 1] - TABLE[k]) * rem >> 23)
    neg_ln = (33 - bitlen) * TABLE[256] - ln_m
    return min((mean * neg_ln) >> 32, (1 << 64) - 1)


def emit_table() -> None:
    print("pub(crate) const LN_TABLE_Q32: [u64; 257] = [")
    for row in range(0, 257, 4):
        chunk = ", ".join(f"{v}" for v in TABLE[row : row + 4])
        print(f"    {chunk},")
    print("];")


MEANS = [0, 1, 3, 1_000, 5_000_000, 1 << 32, (1 << 64) - 1]
US = [
    0, 1, 2, 0x1234_5678, 0x7FFF_FFFF, 0x8000_0000,
    0xCAFE_BABE, 0xDEAD_BEEF, 0xFFFF_FFFE, 0xFFFF_FFFF,
]


def emit_golden() -> None:
    print("# draw_exponential golden vectors: mean u expected")
    print("# Derived by scripts/experiments/exponential-table.py from the normative")
    print("# integer algorithm in API.md 2.4. Frozen; a mismatch is a tape ABI break.")
    for mean in MEANS:
        for u in US:
            print(f"{mean} {u} {exp_inverse_cdf(mean, u)}")


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "--table":
        emit_table()
    elif mode == "--golden":
        emit_golden()
    else:
        sys.exit("usage: exponential-table.py --table|--golden")
