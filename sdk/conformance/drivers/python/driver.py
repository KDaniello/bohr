#!/usr/bin/env python3
"""Makes the calls a conformance case asks for, in Python.

A driver knows how to call its own SDK and nothing else: the case says what to call, the checker
says whether the records were right, and this file is the only part that is language-specific.
"""

from __future__ import annotations

import json
import os
import sys

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "python", "src")
)

import bohr_sdk as bohr  # the path above has to be set first


def main() -> int:
    spec = json.loads(sys.argv[1])
    for call in spec["calls"]:
        fn, args = call["fn"], call["args"]
        if fn == "always":
            bohr.always(args[0], args[1])
        elif fn == "always_or_unreachable":
            bohr.always_or_unreachable(args[0], args[1])
        elif fn == "sometimes":
            bohr.sometimes(args[0], args[1])
        elif fn == "reachable":
            bohr.reachable(args[0])
        elif fn == "unreachable":
            bohr.unreachable(args[0])
        elif fn == "fault_point":
            with bohr.fault_point(args[0]):
                pass
        elif fn == "setup_complete":
            bohr.setup_complete(args[0])
        elif fn == "send_event":
            bohr.send_event(args[0], args[1])
        elif fn == "random_bytes":
            bohr.random_bytes(args[0])
        else:
            print(f"this driver has no call named {fn}", file=sys.stderr)
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
