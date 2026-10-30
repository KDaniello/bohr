#!/usr/bin/env python3
"""Every workflow parses, and every gate actually fires.

A workflow that does not run is not a workflow that fails: it is one GitHub never starts, and a
job that never runs looks exactly like a job that passed. The whole point of a gate is that its
silence means something, so the files that define the gates are checked before they are trusted.

There are two ways to never run, and this checks both.

The syntactic one is a GitHub expression left unquoted inside a flow mapping --
``with: { name: x-${{ matrix.thing }} }`` — where ``{`` opens a nested mapping and the parse
stops. Quote the value, or write the mapping in block form.

The other is a trigger that no event in this repository's history matches. `sdk` and `docs` are
blocking gates in the table in docs/TESTING.md 15, gated there on a path and never on an event;
both were written `pull_request:` alone, and a repository whose branches are pushed to directly
opened no pull request that touched those paths — so neither had run once. A gate that does not
fire on push is not a gate. The rule below is that prohibition, mechanically.
"""

from __future__ import annotations

import pathlib
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent


def fires(path: pathlib.Path, doc: object) -> int:
    """Refuses a workflow that reacts to pull requests but not to pushes.

    Also refuses two path filters that have drifted apart: a gate that fires on a pull request
    touching a file and not on a push touching the same file is the same hole, half-closed. Write
    the list once and anchor it (``paths: &name`` / ``paths: *name``) so the two cannot diverge.
    """
    where = path.relative_to(ROOT)
    if not isinstance(doc, dict):
        print(f"{where}: not a mapping")
        return 1
    # PyYAML resolves the bare word `on` to True; a quoted "on" stays a string.
    on = doc.get(True, doc.get("on"))
    if not isinstance(on, dict):
        # A bare `on: push` or a list of events has no path filter and no per-event options, so
        # neither half of the rule below has anything to say about it.
        return 0
    if "pull_request" not in on:
        return 0
    if "push" not in on:
        print(f"{where}: runs on pull_request but not on push, so a direct push never gates on it")
        return 1
    pr, push = on["pull_request"] or {}, on["push"] or {}
    if pr.get("paths") != push.get("paths"):
        print(f"{where}: pull_request and push filter on different paths")
        return 1
    return 0


def main() -> int:
    files = sorted(
        [*ROOT.glob(".github/workflows/*.yml"), *ROOT.glob(".github/actions/*/action.yml")]
    )
    if not files:
        print("no workflow files found; this check is looking in the wrong place")
        return 1

    failed = 0
    for path in files:
        try:
            doc = yaml.safe_load(path.read_text(encoding="utf-8"))
        except yaml.YAMLError as e:
            failed += 1
            print(f"{path.relative_to(ROOT)}: {e}")
            continue
        if path.parent.name == "workflows":
            failed += fires(path, doc)

    if failed:
        print(f"\n{failed} of {len(files)} workflow files do not gate what they claim to")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
