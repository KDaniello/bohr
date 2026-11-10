#!/usr/bin/env bash
# Prints every claim the tool makes as held, failed or not measured, with its deciding figure.
#
# Figures come from benches/baseline.toml and the documents a claim is about. The script measures
# nothing itself. A claim without a recorded figure is "not measured".
#
# Exits 0 only when every claim holds.
set -u
cd "$(dirname "$0")/.."
exec python3 - "$@" <<'PY'
import pathlib, re, sys, tomllib

base = tomllib.loads(pathlib.Path("benches/baseline.toml").read_text(encoding="utf-8"))

def get(path):
    node = base
    for part in path.split("."):
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node

HELD, FAILED, UNMEASURED = "held", "FAILED", "not measured"
rows = []

def claim(number, name, status, figure, key):
    rows.append((number, name, status, figure, key))

# 1. Clients run unmodified: refused by the model on at most 5% of seeds, each target.
hunts = get("v0_9.hunts") or {}
targets = {k: v for k, v in hunts.items() if isinstance(v, dict) and "refused" in v}
if not targets and hunts:
    claim(1, "Clients run unmodified", UNMEASURED, "campaigns recorded, refusals not counted", "v0_9.hunts")
elif targets:
    worst = max(targets.items(), key=lambda kv: kv[1].get("refused", 0) / max(kv[1].get("runs", 1), 1))
    share = worst[1].get("refused", 0) / max(worst[1].get("runs", 1), 1)
    expected = {"reth", "op-reth", "geth", "cometbft", "lighthouse", "redb", "solana"}
    missing = sorted(expected - set(targets))
    status = HELD if share <= 0.05 and not missing else FAILED
    figure = f"worst {worst[0]} {share:.1%} refused"
    if missing:
        figure += "; not run: " + ", ".join(missing)
    claim(1, "Clients run unmodified", status, figure, "v0_9.hunts")
else:
    claim(1, "Clients run unmodified", UNMEASURED, "no campaign recorded", "v0_9.hunts")

# 2. The fault catalogue is real: the lint rule that holds the two lists to one.
rule = pathlib.Path("scripts/lint-rules.sh").read_text(encoding="utf-8")
claim(2, "The fault catalogue is real", HELD if "14m" in rule else FAILED,
      "rule 14m in make pr" if "14m" in rule else "rule 14m is gone", "v0_6.gates.fault_lists_agree")

# 3. Blockchain oracles: each seeded defect on 30 of 60 or more, nothing over 200 clean seeds.
caught = get("v0_4.gates.oracle_catches_its_defect") or {}
clean = str(get("v0_4.gates.no_oracle_cries_wolf") or "")
if caught:
    low = min(caught.items(), key=lambda kv: int(kv[1].split()[0]))
    ok = int(low[1].split()[0]) >= 30 and clean.startswith("0 findings")
    claim(3, "Blockchain oracles", HELD if ok else FAILED,
          f"lowest {low[0]} {low[1]}; clean seeds: {clean}", "v0_4.gates")
else:
    claim(3, "Blockchain oracles", UNMEASURED, "no oracle figure", "v0_4.gates")

# 4. Findings: five defects in three projects, confirmed by their maintainers and disclosed.
confirmed = get("v0_9.findings.confirmed")
projects = get("v0_9.findings.projects")
if confirmed is None:
    claim(4, "Findings", UNMEASURED, "no disclosure recorded", "v0_9.findings")
else:
    claim(4, "Findings", HELD if confirmed >= 5 and (projects or 0) >= 3 else FAILED,
          f"{confirmed} confirmed in {projects or 0} projects", "v0_9.findings")

# 5. A run repeats and travels: 1000 of 1000, and 100 of 100 across vendors.
repeat = get("v0_6.gates.repeat_1000_every_fault")
vendor = get("v0_7.gates.cross_vendor")
travels = isinstance(vendor, str) and vendor.startswith("100 of 100")
unmeasured = not isinstance(vendor, str) or vendor.startswith("not measured")
status = HELD if repeat == "one digest" and travels else FAILED
if repeat == "one digest" and unmeasured:
    status = UNMEASURED
claim(5, "A run repeats and travels", status,
      f"repeats: {repeat}; across vendors: {vendor}", "v0_6.gates, v0_7.gates")

# 6. Usable speed: at most 5x native on every pack's workload, startup at most 5 ms.
startup = get("v0_8.perf.startup_ms") or {}
latest = startup.get("v0_8")
packs = get("v0_9.speed") or {}
if latest is None:
    claim(6, "Usable speed", UNMEASURED, "no startup figure", "v0_8.perf")
else:
    ratios = {k: v for k, v in packs.items() if isinstance(v, (int, float))}
    worst = max(ratios.items(), key=lambda kv: kv[1]) if ratios else None
    ok = latest <= 5 and worst is not None and worst[1] <= 5
    figure = f"startup {latest} ms; " + (f"worst {worst[0]} {worst[1]}x" if worst else "no pack's workload measured")
    status = HELD if ok else FAILED
    if worst is None and latest <= 5:
        status = UNMEASURED
    claim(6, "Usable speed", status, figure, "v0_8.perf, v0_9.speed")

# 7. Reduction at real scale: at least tenfold within 200 replays.
reduction = get("v0_7.reduction") or {}
kept = [v.get("kept") for v in reduction.values() if isinstance(v, dict) and "kept" in v]
if kept:
    best = min(kept)
    claim(7, "Reduction at real scale", HELD if best <= 0.10 else FAILED,
          f"best kept {best:.1%} of events", "v0_7.reduction")
else:
    claim(7, "Reduction at real scale", UNMEASURED, "no real reduction", "v0_7.reduction")

# 8. One command, in CI: a campaign from an installed pack, and an action on a pull request.
gates = get("v0_8.gates") or {}
on_github = gates.get("action_on_pull_request")
status = HELD if on_github and gates.get("install_to_report") else FAILED
if not on_github and gates.get("install_to_report"):
    status = UNMEASURED
claim(8, "One command, in CI", status,
      f"installed hunt: {gates.get('install_to_report')}; on a pull request: {on_github or 'not run'}",
      "v0_8.gates")

# 9. Limits are stated: every source of nondeterminism has a row and a treatment.
det = pathlib.Path("docs/DETERMINISM.md").read_text(encoding="utf-8")
rows_det = [line for line in det.splitlines() if line.startswith("| ") and "---" not in line]
untreated = [line for line in rows_det if line.rstrip().endswith("|  |") or "TODO" in line]
claim(9, "Limits are stated", HELD if rows_det and not untreated else FAILED,
      f"{len(rows_det)} rows, {len(untreated)} without a treatment", "docs/DETERMINISM.md")

width = max(len(r[1]) for r in rows)
for number, name, status, figure, key in rows:
    print(f"{number}  {name:<{width}}  {status:<12}  {figure}   [{key}]")
held = sum(1 for r in rows if r[2] == HELD)
print(f"{held} of {len(rows)} claims hold")
sys.exit(0 if held == len(rows) else 1)
PY
