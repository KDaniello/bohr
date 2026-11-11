"""Tell Bohr what your program promises, and let it try to break the promise.

    import bohr_sdk as bohr

    bohr.always(log.is_monotonic(), "log is monotonic")
    bohr.sometimes(leader_changed, "a leader change happened")
    with bohr.fault_point("before_commit_ack") as armed:
        if armed:
            ...  # this run wants the fault here

Nothing here has any logic of its own: it checks whether a channel is open, serialises a record
and writes a line. Everything that decides anything is the supervisor's, which is what keeps this
module something anyone could rewrite in an afternoon from ``docs/API.md`` section 6.

Outside Bohr every call is one branch on a cached value. The standard library only — a program
that installs this must not have to resolve a dependency of ours.
"""

from __future__ import annotations

import contextlib
import inspect
import json
import os
import threading
from collections.abc import Iterator, Sequence
from contextlib import contextmanager
from typing import Any, TextIO

__all__ = [
    "always",
    "always_or_unreachable",
    "fault_point",
    "kv_acked",
    "kv_begun",
    "kv_read",
    "kv_ready",
    "random_bytes",
    "random_choice",
    "reachable",
    "send_event",
    "setup_complete",
    "sometimes",
    "unreachable",
]

#: Version of the record schema this module writes.
SCHEMA = 1

_OUTPUT_VAR = "BOHR_SDK_OUTPUT"
_ANTITHESIS_OUTPUT_VAR = "ANTITHESIS_SDK_LOCAL_OUTPUT"
_NODE_VAR = "BOHR_SDK_NODE"
_ARMED_VAR = "BOHR_ARMED_FAULT_POINTS"


class _Channel:
    """The open output, its sequence counter and what this run armed."""

    def __init__(self, handle: TextIO, node: int, armed: frozenset[str]) -> None:
        self.handle = handle
        self.node = node
        self.armed = armed
        self.seq = 0
        self.lock = threading.Lock()


class _State:
    """The module's one piece of mutable state: whether the channel was resolved, and to what.

    A class rather than a pair of module globals so that mutation is an attribute assignment —
    the singleton is the module itself, and this is its private field.
    """

    channel: _Channel | None = None
    resolved = False
    lock = threading.Lock()


def _open() -> _Channel | None:
    """Resolves the channel once.

    Both variables set is not an error: ours wins, and the supervisor records the ambiguity. An
    SDK that refused to run would break a program whose author did nothing wrong.
    """
    if _State.resolved:
        return _State.channel
    with _State.lock:
        if _State.resolved:
            return _State.channel
        _State.resolved = True
        target = os.environ.get(_OUTPUT_VAR) or os.environ.get(_ANTITHESIS_OUTPUT_VAR)
        if not target:
            return None
        try:
            # A bare number names an already-open descriptor, which is how a supervisor hands
            # one over without a path existing anywhere. Deliberately not a context manager:
            # the channel stays open for the life of the process.
            handle = (
                os.fdopen(int(target), "a", buffering=1)
                if target.isdigit()
                else open(target, "a", buffering=1, encoding="utf-8")  # noqa: SIM115
            )
        except OSError:
            return None
        armed = frozenset(a for a in os.environ.get(_ARMED_VAR, "").split(",") if a)
        node = int(os.environ.get(_NODE_VAR, "0") or 0)
        _State.channel = _Channel(handle, node, armed)
        return _State.channel


def _emit(key: str, body: dict[str, Any]) -> None:
    """Writes one record, if anything is listening."""
    channel = _open()
    if channel is None:
        return
    with channel.lock:
        channel.seq += 1
        record = {
            key: body,
            "bohr": {"schema": SCHEMA, "seq": channel.seq, "node": channel.node},
        }
        # A record that cannot be written is dropped rather than raised: this module is a
        # bystander in someone else's program, and the supervisor notices the gap anyway.
        with contextlib.suppress(OSError):
            channel.handle.write(json.dumps(record, separators=(",", ":")) + "\n")


def _here() -> dict[str, Any]:
    """Where in the guest's own code the call was made.

    Found by walking out of this module rather than by counting frames: a count is right until
    someone adds a helper, and then every report quietly points at the SDK instead of at the
    program it is reporting on.
    """
    mine = os.path.abspath(__file__)
    frame = inspect.currentframe()
    while frame is not None and os.path.abspath(frame.f_code.co_filename) == mine:
        frame = frame.f_back
    if frame is None:
        return {"file": "", "function": "", "begin_line": 0, "begin_column": 0}
    return {
        "file": frame.f_code.co_filename,
        "function": frame.f_code.co_name,
        "begin_line": frame.f_lineno,
        "begin_column": 0,
    }


def _assert(kind: str, display: str, condition: bool, message: str) -> None:
    if _open() is None:
        return
    _emit(
        "antithesis_assert",
        {
            "hit": True,
            "must_hit": kind != "unreachable",
            "assert_type": kind,
            "display_type": display,
            "message": message,
            "condition": bool(condition),
            "id": message,
            "location": _here(),
        },
    )


def always(condition: bool, message: str) -> None:
    """Claims that ``condition`` holds every time this line is reached."""
    _assert("always", "Always", condition, message)


def always_or_unreachable(condition: bool, message: str) -> None:
    """Claims that ``condition`` holds whenever reached, and that never reaching it is fine."""
    _assert("always_or_unreachable", "AlwaysOrUnreachable", condition, message)


def sometimes(condition: bool, message: str) -> None:
    """Claims that ``condition`` holds at least once across a campaign."""
    _assert("sometimes", "Sometimes", condition, message)


def reachable(message: str) -> None:
    """Records that this line was reached."""
    _assert("reachable", "Reachable", True, message)


def unreachable(message: str) -> None:
    """Records that a line that should never be reached was."""
    _assert("unreachable", "Unreachable", False, message)


@contextmanager
def fault_point(name: str) -> Iterator[bool]:
    """Whether this run armed the named fault point.

    A context manager rather than a function because that is how the surrounding code reads: the
    block is the window the fault belongs in. Outside Bohr it yields ``False`` and the program
    takes its ordinary path.
    """
    channel = _open()
    armed = bool(channel and name in channel.armed)
    _emit("bohr_fault_point", {"name": name, "armed": armed})
    yield armed


def setup_complete(details: Any = None) -> None:
    """Announces that the program has finished setting itself up."""
    _emit("antithesis_setup", {"status": "complete", "details": details})


def send_event(name: str, details: Any = None) -> None:
    """Puts a named event on the timeline."""
    _emit("antithesis_event", {name: details})


def _kv_op(stage: str, op: str, keys: list[str], values: list[str | None], seq: int) -> None:
    frame = inspect.currentframe()
    caller = frame.f_back.f_back if frame and frame.f_back else None
    at = {"file": caller.f_code.co_filename, "line": caller.f_lineno} if caller else None
    send_event(stage, {"op": op, "keys": keys, "values": values, "seq": seq, "at": at})


def kv_begun(op: str, keys: list[str], values: list[str | None], seq: int) -> None:
    """Records an operation a store's harness began: put, delete or batch; None removes a key."""
    _kv_op("kv.begun", op, keys, values, seq)


def kv_acked(op: str, keys: list[str], values: list[str | None], seq: int) -> None:
    """Records an operation the store acknowledged, at the line that calls this."""
    _kv_op("kv.acked", op, keys, values, seq)


def kv_read(key: str, value: str | None) -> None:
    """Records a key read back: its value, or None where it is absent."""
    send_event("kv.read", {"key": key, "value": value})


def kv_ready() -> None:
    """Records that the store answered its first question since this start."""
    send_event("kv.ready", {})


def random_bytes(n: int) -> bytes:
    """Random bytes that belong to the run rather than to the machine.

    Under Bohr these come from the entropy tape by way of the world model, so a seed reproduces
    them. Outside it they come from the platform, and the supervisor reports a run whose
    randomness escaped it rather than pretending the run was reproducible.
    """
    if _open() is not None:
        send_event("bohr_random", {"n": n})
    with open("/dev/urandom", "rb") as source:
        return source.read(n)


def random_choice(choices: Sequence[Any]) -> Any:
    """One of ``choices``, or ``None`` where there is nothing to choose from."""
    if not choices:
        return None
    index = int.from_bytes(random_bytes(8), "little") % len(choices)
    return choices[index]
