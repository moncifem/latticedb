"""Randomised properties with a seeded generator. Case ``i`` uses seed ``i + 1``, so a failure
message names the seed and the case replays exactly. ``LATTICE_PY_CASES`` sets the count."""

from __future__ import annotations

import dataclasses
import random
import unittest
from collections.abc import Callable

from lattice_client import (
    ERROR_CODE_NAMES,
    MAX_CURSOR_LEN,
    MAX_EVENTS_PER_FRAME,
    Ack,
    Action,
    ChangeEvent,
    ClientError,
    Cursor,
    Delete,
    Deliver,
    Error,
    Events,
    FeedDecodeError,
    Hello,
    Hlc,
    Message,
    Ping,
    Pong,
    Send,
    Subscribe,
    SyncClient,
    SyncError,
    SyncEvent,
    TurnRef,
    Upsert,
    Welcome,
    compare_cursors,
    cursor_from_bytes,
    cursor_to_bytes,
    decode,
    decode_event,
    default_config,
    encode,
    encode_event,
)

from .support import cases, for_each_case, rand_u32, rand_u64

POOL = ["a", "b", "z", "0", "/", "-", "_", " ", "~", "é", "漢", "\U0001f600", " ", " ", "﻿", "￾"]


def utf8_len(s: str) -> int:
    return len(s.encode("utf-8"))


def text(rng: random.Random, lo: int, hi: int) -> str:
    """Valid text of ``lo..=hi`` UTF-8 bytes, no control characters."""
    want = rng.randint(lo, min(hi, lo + 12))
    s = ""
    while utf8_len(s) < want:
        nxt = s + rng.choice(POOL)
        if utf8_len(nxt) > hi:
            break
        s = nxt
    while utf8_len(s) < lo:
        s += "a"
    return s


def rand_bytes(rng: random.Random, n: int) -> bytes:
    return bytes(rng.randrange(256) for _ in range(n))


def message(rng: random.Random) -> Message:
    def sub() -> int:
        return rng.randint(1, 0xFFFF_FFFF)

    kind = rng.randrange(8)
    if kind == 0:
        lo = rng.randint(1, 0xFFFF)
        return Hello(lo, rng.randint(lo, 0xFFFF), text(rng, 1, 64))
    if kind == 1:
        return Welcome(1, rng.randint(1024, 1 << 20), text(rng, 1, 64))
    if kind == 2:
        return Subscribe(sub(), text(rng, 1, 512), rand_bytes(rng, rng.randint(0, MAX_CURSOR_LEN)))
    if kind == 3:
        count = rng.randint(1, MAX_EVENTS_PER_FRAME if rng.randrange(8) == 0 else 6)
        return Events(
            sub(),
            tuple(
                SyncEvent(
                    rand_bytes(rng, rng.randint(1, MAX_CURSOR_LEN)), rand_bytes(rng, rng.randint(0, 40))
                )
                for _ in range(count)
            ),
        )
    if kind == 4:
        return Ack(sub(), rand_bytes(rng, rng.randint(1, MAX_CURSOR_LEN)))
    if kind == 5:
        return Error(rng.choice(ERROR_CODE_NAMES), rand_u32(rng), text(rng, 0, 256))
    if kind == 6:
        return Ping(rand_u64(rng))
    return Pong(rand_u64(rng))


def mutate(rng: random.Random, original: bytes, header_len: int) -> bytes:
    """Flips, inserts, deletes or truncates bytes, and often repairs the length so the payload
    parser runs."""
    b = list(original)
    for _ in range(rng.randint(1, 3)):
        at = rng.randrange(max(1, len(b)))
        op = rng.randrange(6)
        if op == 0 and at < len(b):
            b[at] ^= 1 << rng.randrange(8)
        elif op == 1 and at < len(b):
            b[at] = rng.choice([0, 1, 0x7F, 0x80, 0xC0, 0xED, 0xF4, 0xFF])
        elif op == 2 and at < len(b):
            del b[at]
        elif op == 3:
            b.insert(at, rng.randrange(256))
        elif op == 4:
            b = b[:at]
        else:
            b.append(rng.randrange(256))
    if header_len > 0 and len(b) >= header_len and rng.randrange(10) < 7:
        b[1:5] = (len(b) - header_len).to_bytes(4, "big")
    return bytes(b)


def check_sync_input(test: unittest.TestCase, data: bytes) -> None:
    """Whatever the input, the decoder returns, waits or raises SyncError; what it accepts is
    canonical."""
    try:
        decoded = decode(data)
    except SyncError:
        return
    if decoded is None:
        return
    test.assertLessEqual(decoded.length, len(data))
    test.assertEqual(encode(decoded.message), data[: decoded.length], "not canonical")


def change_event(rng: random.Random) -> ChangeEvent:
    lsn = rand_u64(rng) or 1
    set_len = rand_u32(rng) or 1
    index = set_len - 1 if rng.randrange(4) == 0 else rng.randrange(set_len)
    return ChangeEvent(
        cursor=Cursor(lsn, index),
        set_len=set_len,
        change_set=rand_bytes(rng, 16),
        hlc=Hlc(rand_u64(rng), rand_u32(rng), rand_u32(rng)),
        author=(rand_u64(rng) << 64) | rand_u64(rng),
        turn=None if rng.random() < 0.5 else TurnRef(rand_u64(rng), rand_u64(rng)),
        kind=rand_u32(rng),
        key=rand_bytes(rng, rng.randint(0, 40)),
        op=Delete() if rng.random() < 0.5 else Upsert(rand_bytes(rng, rng.randint(0, 40))),
    )


class SyncProperties(unittest.TestCase):
    def test_encode_then_decode_returns_the_message_and_the_bytes_are_canonical(self) -> None:
        def one(rng: random.Random) -> None:
            m = message(rng)
            data = encode(m)
            d = decode(data)
            assert d is not None
            self.assertEqual((d.message, d.length), (m, len(data)))
            self.assertEqual(encode(d.message), data)

        for_each_case(cases(3000), one)

    def test_a_stream_split_at_random_points_decodes_to_the_same_messages(self) -> None:
        def one(rng: random.Random) -> None:
            msgs = [message(rng) for _ in range(rng.randint(1, 6))]
            stream = b"".join(encode(m) for m in msgs)
            got: list[Message] = []
            pending = b""
            pos = 0
            while pos < len(stream):
                n = rng.randint(1, 64)
                pending += stream[pos : pos + n]
                pos += n
                while (d := decode(pending)) is not None:
                    got.append(d.message)
                    pending = pending[d.length :]
            self.assertEqual(pending, b"")
            self.assertEqual(got, msgs)

        for_each_case(cases(500), one)

    def test_mutated_frames_never_crash_the_decoder_and_what_it_accepts_is_canonical(self) -> None:
        for_each_case(
            cases(6000), lambda rng: check_sync_input(self, mutate(rng, encode(message(rng)), 5))
        )

    def test_random_bytes_never_crash_the_decoder(self) -> None:
        def one(rng: random.Random) -> None:
            body = rand_bytes(rng, rng.randint(0, 40))
            header = bytes([rng.randint(0, 10), 0, 0, 0, len(body)])
            data = rand_bytes(rng, rng.randint(0, 60)) if rng.random() < 0.5 else header + body
            check_sync_input(self, data)

        for_each_case(cases(3000), one)


class FeedProperties(unittest.TestCase):
    def test_encode_then_decode_returns_the_event_and_the_bytes_are_canonical(self) -> None:
        def one(rng: random.Random) -> None:
            e = change_event(rng)
            data = encode_event(e)
            self.assertEqual(decode_event(data), e)
            self.assertEqual(encode_event(decode_event(data)), data)

        for_each_case(cases(3000), one)

    def test_mutated_events_never_crash_the_decoder_and_what_it_accepts_is_canonical(self) -> None:
        def one(rng: random.Random) -> None:
            data = mutate(rng, encode_event(change_event(rng)), 0)
            try:
                e = decode_event(data)
            except FeedDecodeError:
                return
            self.assertEqual(encode_event(e), data, "not canonical")

        for_each_case(cases(6000), one)

    def test_cursor_bytes_order_exactly_like_cursors(self) -> None:
        def one(rng: random.Random) -> None:
            # Few distinct values, so equal lsn and equal index both come up often.
            lsns = [1, 2, 255, 256, (1 << 53) + 1, (1 << 64) - 1]
            idx = [0, 1, 255, 256, 0xFFFF_FFFF]
            a = Cursor(rng.choice(lsns), rng.choice(idx))
            b = Cursor(rng.choice(lsns), rng.choice(idx))
            ba, bb = cursor_to_bytes(a), cursor_to_bytes(b)
            self.assertEqual((ba > bb) - (ba < bb), compare_cursors(a, b))
            self.assertEqual(cursor_from_bytes(ba), a)

        for_each_case(cases(3000), one)


# ---- client against a model server --------------------------------------------------------------


def cur(n: int) -> bytes:
    return n.to_bytes(8, "big")


def pos_of(c: bytes) -> int:
    return int.from_bytes(c, "big")


def run(server: str, script: list[tuple[int, int]]) -> list[int]:
    """Positions the application received, in order. The script drives sends, acks and reconnects."""
    c = SyncClient(dataclasses.replace(default_config("sdk-py"), ack_batch=2))
    c.connect(0)
    c.handle(Welcome(1, 1 << 20, "s"), 0)
    sub, _ = c.subscribe("s", b"")
    seen: list[int] = []
    nxt = 1
    now = 1
    for op, arg in script:
        now += 1
        if op % 4 in (0, 1):
            n = 1 + arg % 4
            out = c.handle(Events(sub, tuple(SyncEvent(cur(nxt + i), bytes([i])) for i in range(n))), now)
            seen.extend(pos_of(a.cursor) for a in out if isinstance(a, Deliver))
            nxt += n
        elif op % 4 == 2:
            if seen:
                c.ack(sub, cur(seen[-1]))
                c.flush_acks()
        else:
            c.disconnected()
            c.reconnect(now)
            out2: list[Action] = c.handle(Welcome(1, 1 << 20, "s"), now)
            resub = next(
                a.frame for a in out2 if isinstance(a, Send) and isinstance(a.frame, Subscribe)
            )
            acked = 0 if len(resub.cursor) == 0 else pos_of(resub.cursor)
            nxt = max(1, acked + 1 - arg % 3) if server == "honest" else acked + 2
    return seen


def exactly_once(seen: list[int]) -> str | None:
    for i, v in enumerate(seen):
        if v != i + 1:
            return f"position {i}: got {v}, want {i + 1}"
    return None


class ClientProperties(unittest.TestCase):
    def test_a_server_that_skips_an_event_makes_the_property_fail(self) -> None:
        # Deliver 1 and 2, ack, reconnect: the buggy server resumes at 4 instead of 3.
        script = [(0, 1), (2, 0), (3, 0), (0, 1)]
        seen = run("skips_one", script)
        self.assertEqual(seen, [1, 2, 4, 5])
        self.assertIsNotNone(exactly_once(seen))
        self.assertIsNone(exactly_once(run("honest", script)))

    def test_with_a_server_that_honours_cursors_the_application_sees_each_event_exactly_once(self) -> None:
        def one(rng: random.Random) -> None:
            script = [(rng.randrange(256), rng.randrange(256)) for _ in range(rng.randint(0, 80))]
            self.assertIsNone(exactly_once(run("honest", script)))

        for_each_case(cases(1500), one)

    def test_arbitrary_frames_never_raise_anything_but_a_client_error(self) -> None:
        def one(rng: random.Random) -> None:
            c = SyncClient(dataclasses.replace(default_config("x"), ack_batch=2))
            c.connect(0)
            for i in range(rng.randrange(40)):
                now = i * 7
                sub = rng.randrange(3)
                first = rng.randrange(5)
                k = rng.randrange(7)
                if k == 0:
                    frame: Message = Welcome(rng.randrange(3), 1 << 20, "s")
                elif k == 1:
                    frame = Events(sub, tuple(SyncEvent(cur(first + j), b"") for j in range(3)))
                elif k == 2:
                    frame = Error(rng.choice(ERROR_CODE_NAMES), sub, "")
                elif k == 3:
                    frame = Ping(rand_u64(rng))
                elif k == 4:
                    frame = Pong(rand_u64(rng))
                elif k == 5:
                    frame = Ack(sub, b"")
                else:
                    frame = Hello(1, 1, "x")
                steps: list[Callable[[], object]] = [
                    lambda: c.handle(frame, now),
                    lambda: c.tick(now),
                    lambda: c.subscribe("s", b""),
                    lambda: c.reconnect(now) if c.phase() == "disconnected" else [],
                ]
                for step in steps:
                    try:
                        step()
                    except ClientError:
                        pass

        for_each_case(cases(1000), one)


if __name__ == "__main__":
    unittest.main()
