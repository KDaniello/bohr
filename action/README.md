# bohr hunt, as a GitHub Action

The action lives in the bohr repository, `KDaniello/bohr/action`, at the ref of the release it
installs. Hunts a [bohr](https://github.com/KDaniello/bohr) target pack in a GitHub Actions job and fails the
job on a finding. Not a test runner: a finding is a run that reproduces, and what the job leaves
behind is the bundle that replays it, a page about it, a disclosure draft and SARIF.

```yaml
jobs:
  hunt:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v5
      - uses: KDaniello/bohr/action@v1.0.0
        id: bohr
        with:
          pack: targets/my-service      # a pack name, or a pack's directory or file
          time: 5m                      # or seeds: 1..60, the budget that repeats
          binary: build/my-service      # optional: hunt this build instead of building the pack's
      - uses: github/codeql-action/upload-sarif@v4
        if: failure() && steps.bohr.outputs.sarif != ''
        with: { sarif_file: "${{ steps.bohr.outputs.sarif }}" }
```

| Input | Meaning | Default |
|---|---|---|
| `pack` | the pack to hunt | required |
| `time` | wall-clock budget, ignored where `seeds` is given | `5m` |
| `seeds` | run these seeds once each, `a..b` | — |
| `binary` | a built binary with the pack's digest, so the job does not build it | — |
| `version` | the bohr release, `v1.0.0` | the latest |
| `source` | where `bohrup` fetches the release from: a URL or a directory | the releases page |
| `out` | where findings, the summary, the report and the SARIF go | `bohr-hunt` |

Outputs: `findings`, how many runs found something, and `sarif`, the path of the SARIF file or
empty. On a finding the directory `out` is attached as the artefact `bohr-findings`, the groups are
written to the job summary, and the last step fails.

What it does not do: upload SARIF itself, which needs a token with `security-events: write` that a
job should grant deliberately; or decide that a finding is a bug in the program rather than in the
model it ran under — the disclosure draft says what a run under a model cannot claim.
