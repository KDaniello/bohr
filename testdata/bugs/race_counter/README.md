# `race_counter`

**Defect.** A lost update. Two threads increment one counter with a read, a yield and a write, and
no lock. The write of the second thread overwrites the read of the first whenever the two interleave
between the read and the write.

**Depth.** 1 — one ordering has to fall in one place.

**How it reports.** The program prints the counter and the total it expected, and exits non-zero
when they differ. The exit status is what the oracle turns into a finding; nothing about the defect
is known to the tool in advance.

**Budget.** Under 100 runs.
