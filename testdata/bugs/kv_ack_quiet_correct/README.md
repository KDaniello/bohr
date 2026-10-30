# kv_ack_quiet_correct

The twin of [`kv_ack_quiet`](../kv_ack_quiet/): the same store, with nothing acknowledged before it
is durable.

It exists to make the claim falsifiable. "Caught on a seeded defect" is half a claim, because an
oracle that fired on every run would satisfy it; the gate is the pair, on the same scenario and the
same seeds — **found on the defective twin and never on this one**.

Identical in every other respect on purpose: same records, same answers, same pause, same kill in
the same place. The difference is the order of two calls.
