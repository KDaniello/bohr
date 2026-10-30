# split-brain

A five-node [`raft-rs`](https://github.com/tikv/raft-rs) cluster driven through a one-directional
partition and back. This scenario is the M2 gate: one seed must produce a byte-identical run, a
hundred times over.

```sh
make example-split-brain          # static build, into examples/split-brain/build
bohr run --seed 42 --repeat 100 --config examples/split-brain/scenario.toml -- \
    examples/split-brain/build/split-brain-node
```

The node checks Raft's own promise and exits 1 when it breaks: no term may have two leaders, and a
committed index may never go backwards. Nothing about the defect is known to the tool — the guest
catches itself out, which is what makes a finding believable.

`raft-rs` rather than an asynchronous library because it is a state machine the caller drives:
`tick` and `step` are ordinary function calls, so a node's whole syscall surface is a datagram
socket, `nanosleep` and its own memory. The transport is deliberately the plainest one that works.

The binary is linked statically, like every guest in `testdata/`: the virtual filesystem holds what
the world model puts there, and a dynamic loader looking for `libc.so.6` finds nothing.
