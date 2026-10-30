"""Helpers shared by the tests: the vector file, JSON to value conversion and case counts."""

from __future__ import annotations

import json
import os
import random
import unittest
from pathlib import Path
from typing import Any, Callable

from lattice_client import (
    ChangeEvent,
    Cursor,
    Delete,
    Hlc,
    TurnRef,
    Upsert,
    from_hex,
)
from lattice_client import sync as s

Json = Any

# clients/python/tests -> repository root is three levels up.
VECTORS_PATH = Path(__file__).resolve().parents[3] / "conformance" / "vectors.json"


def load_vectors() -> dict[str, Any]:
    """The vectors the Rust side wrote: ``conformance/vectors.json`` at the repository root."""
    with VECTORS_PATH.open(encoding="utf-8") as f:
        data: dict[str, Any] = json.load(f)
    return data


def bytes_of(j: Json) -> bytes:
    """Hex text, or ``{"fill":"ab","len":N}``."""
    if isinstance(j, str):
        return from_hex(j)
    assert isinstance(j, dict), j
    return bytes(from_hex(j["fill"]) * j["len"])


def big(j: Json) -> int:
    """A decimal string holding a 64 or 128 bit integer."""
    assert isinstance(j, str) and j.isascii() and j.isdigit(), j
    return int(j)


def message_from_json(j: Json) -> s.Message:
    kind = j["type"]
    if kind == "hello":
        return s.Hello(j["min_version"], j["max_version"], j["agent"])
    if kind == "welcome":
        return s.Welcome(j["version"], j["max_payload"], j["server"])
    if kind == "subscribe":
        return s.Subscribe(j["sub"], j["scope"], bytes_of(j["cursor"]))
    if kind == "events":
        return s.Events(
            j["sub"],
            tuple(s.SyncEvent(bytes_of(e["cursor"]), bytes_of(e["data"])) for e in j["events"]),
        )
    if kind == "ack":
        return s.Ack(j["sub"], bytes_of(j["cursor"]))
    if kind == "error":
        assert j["code"] in s.ERROR_CODE_NAMES, j["code"]
        return s.Error(j["code"], j["sub"], j["message"])
    if kind == "ping":
        return s.Ping(big(j["nonce"]))
    if kind == "pong":
        return s.Pong(big(j["nonce"]))
    raise AssertionError(f"unknown message type in vectors: {kind}")


def event_from_json(j: Json) -> ChangeEvent:
    turn = j["turn"]
    op = j["op"]
    return ChangeEvent(
        cursor=Cursor(big(j["cursor"]["lsn"]), j["cursor"]["index"]),
        set_len=j["set_len"],
        change_set=bytes_of(j["change_set"]),
        hlc=Hlc(big(j["hlc"]["wall_ms"]), j["hlc"]["counter"], j["hlc"]["node"]),
        author=big(j["author"]),
        turn=None if turn is None else TurnRef(big(turn["session"]), big(turn["turn"])),
        kind=j["kind"],
        key=bytes_of(j["key"]),
        op=Delete() if op["type"] == "delete" else Upsert(bytes_of(op["value"])),
    )


class Base(unittest.TestCase):
    def assert_error_matches(self, err: BaseException, expected: Json, label: str) -> None:
        """The error must carry the kind and the detail Rust's has."""
        kind = getattr(err, "kind", None)
        detail = getattr(err, "detail", None)
        self.assertEqual(kind, expected["kind"], f"{label}: kind ({err})")
        want = expected.get("value", expected.get("field"))
        self.assertEqual(detail, want, f"{label}: detail ({err})")
        # `1 == True`: make sure a bool never stands in for a number.
        self.assertIs(type(detail), type(want), f"{label}: detail type")


def cases(fallback: int) -> int:
    """The number of random cases per test; override with ``LATTICE_PY_CASES``."""
    raw = os.environ.get("LATTICE_PY_CASES", "")
    return int(raw) if raw.isdigit() and int(raw) > 0 else fallback


def for_each_case(count: int, fn: Callable[[random.Random], None]) -> None:
    """Runs ``fn`` with a generator seeded ``i + 1`` for case ``i``; a failure names its seed."""
    for i in range(count):
        seed = i + 1
        try:
            fn(random.Random(seed))
        except Exception as e:
            raise AssertionError(f"seed {seed}: {type(e).__name__}: {e}") from e


def rand_u64(rng: random.Random) -> int:
    """A random u64, biased to include the edges."""
    edges = [0, 1, (1 << 64) - 1, 1 << 53, (1 << 53) + 1, (1 << 63) - 1]
    if rng.randrange(4) == 0:
        return rng.choice(edges)
    return rng.getrandbits(64)


def rand_u32(rng: random.Random) -> int:
    edges = [0, 1, 0xFFFF_FFFF, 0x8000_0000]
    return rng.choice(edges) if rng.randrange(4) == 0 else rng.getrandbits(32)
