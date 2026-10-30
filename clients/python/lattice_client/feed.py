"""Change feed v1: cursor and event codecs (formats/change-feed-v1.md).

A port of ``lattice_core::feed`` with the same bounds, canonical form and order of checks. Python
integers have no width, so each field is range checked: the encoder refuses a value outside its
field instead of wrapping.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Final, Literal, Union

from ._bytes import (
    U32_MAX,
    U64_MAX,
    U128_MAX,
    Reader,
    Writer,
    compare_bytes,
    is_bytes_like,
    is_uint,
)

MAX_KEY_LEN: Final = 4096
"""Largest entity key an event may carry, in bytes."""
MAX_VALUE_LEN: Final = 16 * 1024 * 1024
"""Largest value an upsert may carry, in bytes."""
CURSOR_LEN: Final = 12
"""Encoded cursor size in bytes."""

_VERSION = 1

FeedErrorKind = Literal[
    "zero_lsn",
    "index_out_of_range",
    "key_too_large",
    "value_too_large",
    "invalid_value",
]


class FeedError(Exception):
    """Why a feed value could not be built or encoded. Mirrors ``FeedError``."""

    def __init__(self, kind: FeedErrorKind) -> None:
        super().__init__(kind)
        self.kind: FeedErrorKind = kind
        self.detail: None = None


FeedDecodeErrorKind = Literal["truncated", "version", "trailing", "invalid"]


class FeedDecodeError(Exception):
    """Why bytes are not a canonical feed value. Mirrors ``FeedDecodeError``.

    ``detail`` is the version byte for ``version`` and the field name for ``invalid``.
    """

    def __init__(self, kind: FeedDecodeErrorKind, detail: int | str | None = None) -> None:
        super().__init__(kind if detail is None else f"{kind}: {detail}")
        self.kind: FeedDecodeErrorKind = kind
        self.detail: int | str | None = detail


@dataclass(frozen=True, slots=True)
class Cursor:
    """A position in the feed: ordered by ``lsn``, then ``index``."""

    lsn: int
    index: int


ORIGIN: Final = Cursor(0, 0)
"""Before every event."""


def new_cursor(lsn: int, index: int) -> Cursor:
    """A cursor at event ``index`` of the change set logged at ``lsn``. LSNs start at 1."""
    if not is_uint(lsn, U64_MAX) or not is_uint(index, U32_MAX):
        raise FeedError("invalid_value")
    if lsn == 0:
        raise FeedError("zero_lsn")
    return Cursor(lsn, index)


def compare_cursors(a: Cursor, b: Cursor) -> int:
    """-1, 0 or 1 by ``(lsn, index)``."""
    if a.lsn != b.lsn:
        return -1 if a.lsn < b.lsn else 1
    if a.index != b.index:
        return -1 if a.index < b.index else 1
    return 0


def _write_cursor(w: Writer, c: Cursor) -> None:
    if not is_uint(c.lsn, U64_MAX) or not is_uint(c.index, U32_MAX):
        raise FeedError("invalid_value")
    w.u64be(c.lsn)
    w.u32be(c.index)


def cursor_to_bytes(c: Cursor) -> bytes:
    """Encodes as ``lsn u64 | index u32``, both big endian: byte order is cursor order."""
    w = Writer()
    _write_cursor(w, c)
    return w.finish()


def _need_int(v: int | None) -> int:
    if v is None:
        raise FeedDecodeError("truncated")
    return v


def _need_bytes(v: bytes | None) -> bytes:
    if v is None:
        raise FeedDecodeError("truncated")
    return v


def _read_cursor(r: Reader) -> Cursor:
    lsn = _need_int(r.u64be())
    index = _need_int(r.u32be())
    if lsn == 0 and index != 0:
        raise FeedDecodeError("invalid", "cursor")
    return Cursor(lsn, index)


def cursor_from_bytes(buf: bytes | bytearray | memoryview) -> Cursor:
    """Decodes exactly 12 bytes. The origin is the only cursor with LSN 0 and it has index 0."""
    r = Reader(buf)
    c = _read_cursor(r)
    if r.remaining != 0:
        raise FeedDecodeError("trailing")
    return c


@dataclass(frozen=True, slots=True)
class Hlc:
    """Commit timestamp of a change set."""

    wall_ms: int
    counter: int
    node: int


@dataclass(frozen=True, slots=True)
class TurnRef:
    """The turn a write belonged to."""

    session: int
    turn: int


@dataclass(frozen=True, slots=True)
class Delete:
    """The entity was deleted."""


@dataclass(frozen=True, slots=True)
class Upsert:
    """The entity was created or replaced with ``value``."""

    value: bytes


ChangeOp = Union[Delete, Upsert]
"""What happened to the entity."""


@dataclass(frozen=True, slots=True)
class ChangeEvent:
    """One entity change of one committed change set, with its commit provenance."""

    cursor: Cursor
    set_len: int
    """Number of events the change set produced."""
    change_set: bytes
    """The change set id, 16 bytes (big endian ULID)."""
    hlc: Hlc
    author: int
    """Who made the change, a u128."""
    turn: TurnRef | None
    kind: int
    """Table or collection of the entity."""
    key: bytes
    op: ChangeOp


def _check_change(ev: ChangeEvent) -> None:
    if not is_bytes_like(ev.key) or len(ev.key) > MAX_KEY_LEN:
        raise FeedError("key_too_large")
    if isinstance(ev.op, Upsert) and is_bytes_like(ev.op.value) and len(ev.op.value) > MAX_VALUE_LEN:
        raise FeedError("value_too_large")


def encode_event(ev: ChangeEvent) -> bytes:
    """Encodes to the v1 layout. Raises :class:`FeedError`."""
    if ev.cursor.lsn == 0:
        raise FeedError("zero_lsn")
    if not is_uint(ev.set_len, U32_MAX) or not is_uint(ev.cursor.index, U32_MAX):
        raise FeedError("invalid_value")
    if ev.cursor.index >= ev.set_len:
        raise FeedError("index_out_of_range")
    _check_change(ev)
    if (
        not is_bytes_like(ev.change_set)
        or len(ev.change_set) != 16
        or not is_uint(ev.hlc.wall_ms, U64_MAX)
        or not is_uint(ev.hlc.counter, U32_MAX)
        or not is_uint(ev.hlc.node, U32_MAX)
        or not is_uint(ev.author, U128_MAX)
        or not is_uint(ev.kind, U32_MAX)
        or not (
            isinstance(ev.op, Delete) or (isinstance(ev.op, Upsert) and is_bytes_like(ev.op.value))
        )
    ):
        raise FeedError("invalid_value")
    w = Writer()
    w.u8(_VERSION)
    _write_cursor(w, ev.cursor)
    w.u32le(ev.set_len)
    w.raw(ev.change_set)
    w.u64le(ev.hlc.wall_ms)
    w.u32le(ev.hlc.counter)
    w.u32le(ev.hlc.node)
    w.u128be(ev.author)
    if ev.turn is None:
        w.u8(0)
    else:
        if not is_uint(ev.turn.session, U64_MAX) or not is_uint(ev.turn.turn, U64_MAX):
            raise FeedError("invalid_value")
        w.u8(1)
        w.u64le(ev.turn.session)
        w.u64le(ev.turn.turn)
    w.u32le(ev.kind)
    w.u16le(len(ev.key))
    w.raw(ev.key)
    if isinstance(ev.op, Delete):
        w.u8(0)
    else:
        w.u8(1)
        w.u32le(len(ev.op.value))
        w.raw(ev.op.value)
    return w.finish()


def decode_event(buf: bytes | bytearray | memoryview) -> ChangeEvent:
    """Decodes the v1 layout. Only the bytes :func:`encode_event` produces are accepted."""
    r = Reader(buf)
    v = _need_int(r.u8())
    if v != _VERSION:
        raise FeedDecodeError("version", v)
    cursor = _read_cursor(r)
    if cursor.lsn == 0:
        raise FeedDecodeError("invalid", "event lsn")
    set_len = _need_int(r.u32le())
    if cursor.index >= set_len:
        raise FeedDecodeError("invalid", "event index")
    change_set = _need_bytes(r.take(16))
    hlc = Hlc(
        wall_ms=_need_int(r.u64le()),
        counter=_need_int(r.u32le()),
        node=_need_int(r.u32le()),
    )
    author = _need_int(r.u128be())
    flag = _need_int(r.u8())
    turn: TurnRef | None
    if flag == 0:
        turn = None
    elif flag == 1:
        turn = TurnRef(session=_need_int(r.u64le()), turn=_need_int(r.u64le()))
    else:
        raise FeedDecodeError("invalid", "turn flag")
    kind = _need_int(r.u32le())
    key_len = _need_int(r.u16le())
    if key_len > MAX_KEY_LEN:
        raise FeedDecodeError("invalid", "key length")
    key = _need_bytes(r.take(key_len))
    tag = _need_int(r.u8())
    op: ChangeOp
    if tag == 0:
        op = Delete()
    elif tag == 1:
        length = _need_int(r.u32le())
        if length > MAX_VALUE_LEN:
            raise FeedDecodeError("invalid", "value length")
        # Checked against the bytes that remain before anything is copied.
        op = Upsert(_need_bytes(r.take(length)))
    else:
        raise FeedDecodeError("invalid", "operation")
    if r.remaining != 0:
        raise FeedDecodeError("trailing")
    return ChangeEvent(cursor, set_len, change_set, hlc, author, turn, kind, key, op)


def compare_encoded_cursors(a: bytes, b: bytes) -> int:
    """Compares two encoded cursors as unsigned strings; equals :func:`compare_cursors`."""
    return compare_bytes(bytes(a), bytes(b))
