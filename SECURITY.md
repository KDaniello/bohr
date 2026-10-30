# Security policy

## What Bohr is not

> **Bohr is not a security sandbox.**

It provides reproducibility, not containment. A program running under Bohr is supervised through
`ptrace` and confined with namespaces, and neither is a security boundary against code that is
actively trying to escape. Run untrusted code in a virtual machine, whether or not Bohr is involved.

This is stated first because a tool that isolates a process *looks* like a sandbox, and a user who
assumes it is one has made a decision on a false premise. Hermit carried the same caveat, for the
same reason.

## What we do treat as a vulnerability

| Class | Example |
|---|---|
| **Parser memory safety** | a crafted `.bohr` bundle, `.tape` file or SDK record that causes memory unsafety, unbounded allocation or a hang in the supervisor. These parsers read untrusted input by design |
| **False assurance** | a bug that makes Bohr report a run as deterministic when it was not, or report a bundle as reproducing when it did not. This is the one that matters most to us |
| **Supervisor privilege** | any path where Bohr requires, acquires or grants more privilege than an ordinary user, or where a guest gains privilege it did not start with |
| **Data leakage** | Bohr writing guest data somewhere the user did not ask for, or `--redact` failing to remove what it documents as removed |
| **Supply chain** | a compromised or malicious dependency in the tool or in any SDK |

**False assurance is a security issue here.** A team ships a release because Bohr said the scenario
was clean. If "clean" was wrong because of a defect in our reporting, we caused that. It is triaged
at the same severity as memory unsafety.

## What is not a vulnerability

- A guest escaping isolation. See above — that is not a boundary we claim.
- A guest crashing, hanging or corrupting its own data. That is a finding, and it is the product
  working.
- An unsupported syscall aborting a run (`BOHR-2001`). That is a deliberate design decision
  ([`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) D7); file an ordinary issue.
- A degraded-determinism warning. Reporting our limits loudly is the intended behaviour
  ([`docs/DETERMINISM.md`](./docs/DETERMINISM.md)).

## Reporting

**Do not open a public issue.**

Use GitHub's private vulnerability reporting on this repository: **Security → Report a vulnerability**.
That channel is private, is read only by the maintainers, and needs no third-party account.

Include: the version (`bohr version --verbose` — all four numbers), what was observed, and a
reproduction. For a parser issue, the crafted file. For a false-assurance issue, the bundle and the
seed — those are, conveniently, exactly what this tool exists to produce.

Where the reproduction contains data that cannot be shared, say so and we will work from a description.

## What happens next

| When | What |
|---|---|
| within 3 days | acknowledgement, and whether we consider it in scope |
| within 14 days | an assessment: severity, affected versions, planned fix |
| on release | a fix, advisory, and credit under a name of the reporter's choosing — or none |

We follow coordinated disclosure and agree a date with the reporter. If a fix takes longer than 90 days
we will say why rather than let the clock run out quietly.

## Supported versions

Pre-1.0: only the latest release receives fixes. After 1.0, the current minor and the one before it.

## Our own supply chain

`cargo deny` runs on every commit: licences, advisories, duplicate versions, and an explicit
allow-list of sources. Dependencies are kept few and are reviewed on addition; the SDKs have
**zero** third-party dependencies by rule ([`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §9 S3),
because they are linked into other people's builds.

Release artefacts are published with `SHA256SUMS`, and builds are reproducible from M2 onward — a
tool that promises reproducibility and cannot reproduce its own build has an obvious credibility
problem.

Bohr transmits nothing. No telemetry, no crash reporting, no update check, no analytics. Bohr making
an unconfigured network connection is a vulnerability report and a
serious one.
