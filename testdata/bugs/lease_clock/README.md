# lease_clock

A lease measured on the wall clock, which outlives its term when the wall clock is set back.

| | |
|---|---|
| Defect | a ten-second lease held while `CLOCK_REALTIME` is short of its end; exits 1 once `CLOCK_MONOTONIC` shows the term has passed with the lease still held |
| Reached by | a `clock_step` back during the lease, and by nothing else |
| Not reached by | initial skew or drift (both clocks move together), a step forward (the lease ends early) |

| Scenario | What it shows |
|---|---|
| `scenario.toml` | the wall clock set back three seconds, five seconds into the lease: a finding on every seed |
| `scenario-steady.toml` | the same world with no clock fault: clean on every seed |
| `scenario-drawn.toml` | clock steps and drift drawn from the tape: the finding on the seeds that draw a step back |

```bash
make guests
bohr run --seed 1 --config testdata/bugs/lease_clock/scenario.toml -- testdata/guests/build/lease_clock
```
