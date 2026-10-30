# `race_init`

**Defect.** Double initialisation. Two threads test a flag, build a resource if it is clear, and set
the flag. An interleaving that places both tests before either set has both threads build it.

**Depth.** 2 — the ordering has to fall in two places: both tests before both sets, within one
round.

**How it reports.** The program prints how many times it built the resource and exits non-zero when
that is not exactly once.

**Budget.** Under 1 000 runs.
