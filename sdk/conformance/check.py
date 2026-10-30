#!/usr/bin/env python3
"""Runs every conformance case against every SDK that has a driver here.

One checker for all languages, because the point is that they agree: a case is a list of calls and
the records those calls must produce, and a driver's only job is to make the calls in its own
language. A driver that is not built is skipped and said so — a language that cannot be checked
must not look like one that passed.

Fields that depend on where the driver lives are compared loosely: `location.file` names the
driver's own source, and `bohr.seq` counts records within one process. Everything else must match
exactly, because everything else is the contract.

Usage: check.py [--all] [--driver NAME]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from typing import Any

HERE = os.path.dirname(os.path.abspath(__file__))
CASES = os.path.join(HERE, "cases")
DRIVERS = os.path.join(HERE, "drivers")

# Checked for shape, never for value. A location names the driver's own file, function and line,
# which differ by language on purpose; the sequence counts records within one process; and
# display_type and id are presentations of fields already compared.
SHAPED = ("location",)
DROPPED = ("display_type", "id")


def cases() -> list[str]:
    return sorted(
        name for name in os.listdir(CASES) if os.path.isdir(os.path.join(CASES, name))
    )


def first_built(*paths: str) -> list[str] | None:
    """The first of these that exists, as a command; None where a driver is not built."""
    for path in paths:
        if os.path.isfile(path):
            return [os.path.abspath(path)]
    return None


def python_driver() -> list[str] | None:
    return [sys.executable, os.path.join(DRIVERS, "python", "driver.py")]


#: The executable suffixes to look for, the platform's own first.
#:
#: On Windows a build leaves an extension-less file beside the real one, and it is not something
#: the operating system will start -- so `.exe` comes first there. Preferring it everywhere was
#: worse than useless in a checkout that has been built on both: on Linux the check picked up a
#: Windows binary, which starts through the interop layer, writes its records somewhere the
#: check cannot see, and reports success having done nothing.
SUFFIXES = (".exe", "") if os.name == "nt" else ("", ".exe")


def rust_driver() -> list[str] | None:
    return first_built(
        *(
            os.path.join(HERE, "..", "..", "target", profile, "conformance-driver" + suffix)
            for profile in ("release", "debug")
            for suffix in SUFFIXES
        )
    )


def csharp_driver() -> list[str] | None:
    # Run through the framework host rather than an apphost: the apphost is a native executable
    # for whichever machine built it, and the assembly is the thing the build produces everywhere.
    path = os.path.join(DRIVERS, "csharp", "out", "conformance-driver.dll")
    if not os.path.isfile(path):
        return None
    host = shutil.which("dotnet") or os.path.expanduser("~/.dotnet/dotnet")
    return [host, os.path.abspath(path)] if os.path.isfile(host) else None


def go_driver() -> list[str] | None:
    return first_built(
        *(
            os.path.join(DRIVERS, "go", "out", "conformance-driver" + suffix)
            for suffix in SUFFIXES
        )
    )


#: Every driver this repository has, by the name the command line takes.
#:
#: The list the sweep is built from rather than one written beside it: `--all` used to name a
#: driver called "c" that matches nothing here, so it was reported skipped on every run for ever
#: and nobody was any the wiser.
BY_NAME = {
    "python": python_driver,
    "rust": rust_driver,
    "csharp": csharp_driver,
    "go": go_driver,
}

#: Drivers whose SDKs ship, and which must therefore run.
#:
#: A driver that fails to build used to be skipped and the run passed as long as one other had
#: run, so a shipped SDK could stop conforming and the gate would go on saying yes. Widening the
#: sweep with `--all` is a different question: those are drivers for SDKs that are not shipped yet,
#: and one that is not built is not yet a promise to anybody.
SHIPPED = ("rust", "python", "csharp")


def driver_command(name: str) -> list[str] | None:
    """How to run a driver, or None where it is not built."""
    build = BY_NAME.get(name)
    return build() if build else None


def normalise(record: dict[str, Any]) -> dict[str, Any]:
    """Reduces a record to the part every language must agree on.

    A location is replaced by whether it is there and complete, so a Rust driver and a Python one
    can both be right about where they are while still being required to say.
    """
    out: dict[str, Any] = json.loads(json.dumps(record))
    # Our own namespace, compared field by field rather than dropped whole. `schema` and `node` are
    # contract fields every language must write the same, and popping the object took them out of
    # the comparison along with the one field that legitimately differs.
    meta = out.pop("bohr", None)
    if isinstance(meta, dict):
        # The sequence number is each writer's own count and is not compared; what it must do is
        # be there and be a number, which is a different claim and is made here.
        out["bohr"] = {
            "schema": meta.get("schema"),
            "node": meta.get("node"),
            "seq_is_a_number": isinstance(meta.get("seq"), int),
        }
    for value in out.values():
        if not isinstance(value, dict):
            continue
        for key in DROPPED:
            value.pop(key, None)
        for key in SHAPED:
            if key in value:
                value[key] = shape_of(value[key])
    return out


def shape_of(location: object) -> str:
    """What a location must have, without saying what it must be.

    An expectation file writes the answer directly — `"a location"` — because that is what it is
    claiming; a record from a driver carries the real thing and is reduced to the same word.
    """
    if isinstance(location, str):
        return location
    if not isinstance(location, dict):
        return "not an object"
    if not location.get("file"):
        return "no file"
    if not location.get("function"):
        return "no function"
    if not isinstance(location.get("begin_line"), int) or location["begin_line"] <= 0:
        return "no line"
    return "a location"


def run_case(command: list[str], case: str) -> tuple[bool, str]:
    with open(os.path.join(CASES, case, "case.json"), encoding="utf-8") as f:
        spec = json.load(f)
    with open(os.path.join(CASES, case, "expected.jsonl"), encoding="utf-8") as f:
        expected = [normalise(json.loads(line)) for line in f if line.strip()]

    with tempfile.TemporaryDirectory() as tmp:
        output = os.path.join(tmp, "records.jsonl")
        env = dict(os.environ, BOHR_SDK_OUTPUT=output)
        result = subprocess.run(
            [*command, json.dumps(spec)], env=env, capture_output=True, text=True, check=False
        )
        if result.returncode != 0:
            return False, f"driver exited {result.returncode}: {result.stderr.strip()}"
        if not os.path.exists(output):
            return False, "the driver wrote no records at all"
        with open(output, encoding="utf-8") as f:
            actual = [normalise(json.loads(line)) for line in f if line.strip()]

    if len(actual) != len(expected):
        return False, f"expected {len(expected)} records, got {len(actual)}"
    for i, (want, got) in enumerate(zip(expected, actual)):
        if want != got:
            wanted, gotten = json.dumps(want, sort_keys=True), json.dumps(got, sort_keys=True)
            return False, f"record {i}:\n    expected {wanted}\n    got      {gotten}"
    return True, ""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--all", action="store_true", help="every driver, not only the shipped two")
    parser.add_argument("--driver", help="one driver by name")
    args = parser.parse_args()

    # The shipped SDKs are the default and must be present; --all widens the sweep to every
    # language a driver exists for, skipping the ones not built. The list tightens as SDKs ship.
    if args.driver:
        names = [args.driver]
    elif args.all:
        names = list(BY_NAME)
    else:
        names = list(SHIPPED)
    failures = 0
    checked = 0

    for name in names:
        command = driver_command(name)
        if command is None:
            # A shipped SDK that cannot be run is a shipped SDK nothing checked, which is the one
            # thing this gate exists to prevent. Anything else is a driver for an SDK that has not
            # shipped, and skipping it says so.
            if name in SHIPPED:
                print(f"{name:8} NOT BUILT — a shipped SDK must be checked, not skipped")
                failures += 1
                checked += 1
            else:
                print(f"{name:8} not built, skipped")
            continue
        for case in cases():
            ok, detail = run_case(command, case)
            checked += 1
            if ok:
                print(f"{name:8} {case:24} ok")
            else:
                failures += 1
                print(f"{name:8} {case:24} FAILED\n    {detail}")

    if checked == 0:
        print("no driver was built; nothing was checked")
        return 1
    print(f"\n{checked - failures}/{checked} checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
