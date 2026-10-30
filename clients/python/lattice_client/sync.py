"""Sync protocol v1 codec (formats/sync-protocol-v1.md).

A port of ``lattice_api::sync``: the same bounds, the same canonical encodings and the same order
of checks, so the same input fails with the same error. Pure functions over bytes: no I/O, no
clock, no session state.
"""

from __future__ import annotations

import re
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Final, Literal, Union

from ._bytes import (
    U16_MAX,
    U32_MAX,
    U64_MAX,
    Reader,
    Writer,
    is_bytes_like,
    is_uint,
)

VERSION_MIN: Final = 1
"""The lowest protocol version this implementation speaks."""
VERSION_MAX: Final = 1
"""The highest protocol version this implementation speaks."""
HEADER_LEN: Final = 5
"""Bytes before the payload: a type byte and a four-byte payload length."""
MAX_PAYLOAD: Final = 1 << 20
"""The largest payload the codec accepts or produces, in bytes."""
MIN_NEGOTIATED_PAYLOAD: Final = 1024
"""The smallest payload limit a ``Welcome`` may announce."""
MAX_CURSOR_LEN: Final = 64
"""The longest cursor, in bytes."""
MAX_SCOPE_LEN: Final = 512
"""The longest subscription scope, in bytes of UTF-8."""
MAX_NAME_LEN: Final = 64
"""The longest agent or server name, in bytes of UTF-8."""
MAX_ERROR_MESSAGE_LEN: Final = 256
"""The longest error message, in bytes of UTF-8."""
MAX_EVENTS_PER_FRAME: Final = 256
"""The most events in one ``Events`` frame."""

_TYPE_HELLO = 0x01
_TYPE_WELCOME = 0x02
_TYPE_SUBSCRIBE = 0x03
_TYPE_EVENTS = 0x04
_TYPE_ACK = 0x05
_TYPE_ERROR = 0x06
_TYPE_PING = 0x07
_TYPE_PONG = 0x08

SyncErrorKind = Literal[
    "bad_length",
    "unknown_type",
    "truncated",
    "trailing",
    "not_utf8",
    "too_large",
    "invalid_field",
]


class SyncError(Exception):
    """Why a frame could not be decoded or encoded. Mirrors ``lattice_api::sync::SyncError``.

    ``detail`` is the offending length for ``bad_length``, the type byte for ``unknown_type`` and
    the field name for ``invalid_field``; it is ``None`` for the other kinds.
    """

    def __init__(self, kind: SyncErrorKind, detail: int | str | None = None) -> None:
        super().__init__(kind if detail is None else f"{kind}: {detail}")
        self.kind: SyncErrorKind = kind
        self.detail: int | str | None = detail


ErrorCode = Literal[
    "unsupported_version",
    "malformed",
    "unknown_subscription",
    "invalid_scope",
    "invalid_cursor",
    "too_many_subscriptions",
    "internal",
]

ERROR_CODE_NAMES: Final[tuple[ErrorCode, ...]] = (
    "unsupported_version",
    "malformed",
    "unknown_subscription",
    "invalid_scope",
    "invalid_cursor",
    "too_many_subscriptions",
    "internal",
)
"""The names of the error codes in an ``Error`` message, by wire number 1 to 7."""


def error_code_number(code: str) -> int:
    """The number on the wire for an error code. Raises ``SyncError`` for an unknown name."""
    try:
        return ERROR_CODE_NAMES.index(code) + 1
    except ValueError:
        raise SyncError("invalid_field", "error code") from None


def error_code_from_number(n: int) -> ErrorCode | None:
    """The error code for a number on the wire, or ``None`` if the protocol does not define it."""
    if 1 <= n <= len(ERROR_CODE_NAMES):
        return ERROR_CODE_NAMES[n - 1]
    return None


@dataclass(frozen=True, slots=True)
class SyncEvent:
    """One change delivered to a subscription."""

    cursor: bytes
    """The position after this event, 1 to 64 opaque bytes."""
    data: bytes
    """The event body, opaque to this codec."""


@dataclass(frozen=True, slots=True)
class Hello:
    min_version: int
    max_version: int
    agent: str


@dataclass(frozen=True, slots=True)
class Welcome:
    version: int
    max_payload: int
    server: str


@dataclass(frozen=True, slots=True)
class Subscribe:
    sub: int
    scope: str
    cursor: bytes


@dataclass(frozen=True, slots=True)
class Events:
    sub: int
    events: tuple[SyncEvent, ...]


@dataclass(frozen=True, slots=True)
class Ack:
    sub: int
    cursor: bytes


@dataclass(frozen=True, slots=True)
class Error:
    code: ErrorCode
    sub: int
    message: str


@dataclass(frozen=True, slots=True)
class Ping:
    nonce: int


@dataclass(frozen=True, slots=True)
class Pong:
    nonce: int


Message = Union[Hello, Welcome, Subscribe, Events, Ack, Error, Ping, Pong]
"""A sync protocol message."""


@dataclass(frozen=True, slots=True)
class Negotiation:
    """The result of :func:`negotiate`: a ``version``, or an ``error`` name."""

    version: int | None = None
    error: Literal["malformed", "unsupported_version"] | None = None


def negotiate(minimum: int, maximum: int) -> Negotiation:
    """Picks the version for a connection: the highest one both sides speak.

    The client offers ``minimum..=maximum``; this implementation speaks
    ``VERSION_MIN..=VERSION_MAX``.
    """
    if (
        not is_uint(minimum, U16_MAX)
        or not is_uint(maximum, U16_MAX)
        or minimum < VERSION_MIN
        or minimum > maximum
    ):
        return Negotiation(error="malformed")
    if minimum > VERSION_MAX or maximum < VERSION_MIN:
        return Negotiation(error="unsupported_version")
    return Negotiation(version=min(maximum, VERSION_MAX))


# Unicode category Cc, the same set as Rust's `char::is_control`.
_CONTROL = re.compile("[\u0000-\u001f\u007f-\u009f]")


def has_control_char(s: str) -> bool:
    """True if ``s`` holds a Unicode control character (category Cc)."""
    return _CONTROL.search(s) is not None


def has_lone_surrogate(s: str) -> bool:
    """True if ``s`` holds a surrogate code point, which has no UTF-8 form."""
    return any(0xD800 <= ord(c) <= 0xDFFF for c in s)


def _check_text(byte_len: int, s: str, minimum: int, maximum: int, what: str) -> None:
    if byte_len < minimum or byte_len > maximum or has_control_char(s):
        raise SyncError("invalid_field", what)


def _check_versions(minimum: object, maximum: object) -> None:
    if (
        not is_uint(minimum, U16_MAX)
        or not is_uint(maximum, U16_MAX)
        or minimum < VERSION_MIN  # type: ignore[operator]
        or minimum > maximum  # type: ignore[operator]
    ):
        raise SyncError("invalid_field", "version range")


# A Welcome names the one version this implementation speaks; an unknown version is refused
# rather than guessed at.
def _check_welcome(version: object, max_payload: object) -> None:
    if not is_uint(version, U16_MAX) or version < VERSION_MIN or version > VERSION_MAX:  # type: ignore[operator]
        raise SyncError("invalid_field", "version")
    if (
        not is_uint(max_payload, U32_MAX)
        or max_payload < MIN_NEGOTIATED_PAYLOAD  # type: ignore[operator]
        or max_payload > MAX_PAYLOAD  # type: ignore[operator]
    ):
        raise SyncError("invalid_field", "max payload")


def _check_sub(sub: object) -> None:
    if not is_uint(sub, U32_MAX) or sub == 0:
        raise SyncError("invalid_field", "subscription id")


def _put_text(w: Writer, s: object, minimum: int, maximum: int, what: str) -> None:
    if not isinstance(s, str):
        raise SyncError("not_utf8")
    try:
        raw = s.encode("utf-8")
    except UnicodeEncodeError:
        raise SyncError("not_utf8") from None
    _check_text(len(raw), s, minimum, maximum, what)
    w.u16be(len(raw))
    w.raw(raw)


def _put_cursor(w: Writer, c: object, minimum: int) -> None:
    if not is_bytes_like(c) or len(c) < minimum or len(c) > MAX_CURSOR_LEN:  # type: ignore[arg-type]
        raise SyncError("invalid_field", "cursor")
    w.u16be(len(c))  # type: ignore[arg-type]
    w.raw(c)  # type: ignore[arg-type]


def encode(msg: Message) -> bytes:
    """The frame for ``msg``.

    Refuses any message the decoder would refuse, so a message that encodes always decodes to
    itself. Raises :class:`SyncError`.
    """
    w = Writer()
    if isinstance(msg, Hello):
        _check_versions(msg.min_version, msg.max_version)
        w.u16be(msg.min_version)
        w.u16be(msg.max_version)
        _put_text(w, msg.agent, 1, MAX_NAME_LEN, "agent")
        ty = _TYPE_HELLO
    elif isinstance(msg, Welcome):
        _check_welcome(msg.version, msg.max_payload)
        w.u16be(msg.version)
        w.u32be(msg.max_payload)
        _put_text(w, msg.server, 1, MAX_NAME_LEN, "server")
        ty = _TYPE_WELCOME
    elif isinstance(msg, Subscribe):
        _check_sub(msg.sub)
        w.u32be(msg.sub)
        _put_text(w, msg.scope, 1, MAX_SCOPE_LEN, "scope")
        _put_cursor(w, msg.cursor, 0)
        ty = _TYPE_SUBSCRIBE
    elif isinstance(msg, Events):
        _check_sub(msg.sub)
        if not isinstance(msg.events, Sequence) or not (
            1 <= len(msg.events) <= MAX_EVENTS_PER_FRAME
        ):
            raise SyncError("invalid_field", "event count")
        w.u32be(msg.sub)
        w.u16be(len(msg.events))
        for e in msg.events:
            if not isinstance(e, SyncEvent):
                raise SyncError("invalid_field", "event")
            _put_cursor(w, e.cursor, 1)
            if not is_bytes_like(e.data):
                raise SyncError("invalid_field", "data")
            if len(w) + 4 + len(e.data) > MAX_PAYLOAD:
                raise SyncError("too_large")
            w.u32be(len(e.data))
            w.raw(e.data)
        ty = _TYPE_EVENTS
    elif isinstance(msg, Ack):
        _check_sub(msg.sub)
        w.u32be(msg.sub)
        _put_cursor(w, msg.cursor, 1)
        ty = _TYPE_ACK
    elif isinstance(msg, Error):
        w.u16be(error_code_number(msg.code))
        if not is_uint(msg.sub, U32_MAX):
            raise SyncError("invalid_field", "subscription id")
        w.u32be(msg.sub)
        _put_text(w, msg.message, 0, MAX_ERROR_MESSAGE_LEN, "message")
        ty = _TYPE_ERROR
    elif isinstance(msg, (Ping, Pong)):
        if not is_uint(msg.nonce, U64_MAX):
            raise SyncError("invalid_field", "nonce")
        w.u64be(msg.nonce)
        ty = _TYPE_PING if isinstance(msg, Ping) else _TYPE_PONG
    else:
        raise SyncError("unknown_type")
    if len(w) > MAX_PAYLOAD:
        raise SyncError("too_large")
    payload = w.finish()
    out = Writer()
    out.u8(ty)
    out.u32be(len(payload))
    out.raw(payload)
    return out.finish()


@dataclass(frozen=True, slots=True)
class Decoded:
    """A decoded frame and the number of bytes it used."""

    message: Message
    length: int


def decode(buf: bytes | bytearray | memoryview) -> Decoded | None:
    """Decodes one frame from the front of ``buf`` with the default limit, ``MAX_PAYLOAD``.

    Returns ``None`` when ``buf`` holds only part of a frame. Raises :class:`SyncError`.
    """
    return decode_with_limit(buf, MAX_PAYLOAD)


def decode_with_limit(buf: bytes | bytearray | memoryview, limit: int) -> Decoded | None:
    """Like :func:`decode`, with a payload limit negotiated lower than ``MAX_PAYLOAD``.

    A higher limit is treated as ``MAX_PAYLOAD``. The type and the length are judged as soon as the
    five header bytes are present, so a hostile peer cannot make the reader wait for a payload it
    will refuse.
    """
    if not isinstance(limit, int) or isinstance(limit, bool) or limit < 0:
        raise SyncError("invalid_field", "limit")
    view = memoryview(buf)
    if len(view) == 0:
        return None
    ty = view[0]
    if ty < _TYPE_HELLO or ty > _TYPE_PONG:
        raise SyncError("unknown_type", ty)
    if len(view) < HEADER_LEN:
        return None
    length = int.from_bytes(view[1:HEADER_LEN], "big")
    if length > min(limit, MAX_PAYLOAD):
        raise SyncError("bad_length", length)
    end = HEADER_LEN + length
    if len(view) < end:
        return None
    return Decoded(_decode_payload(ty, bytes(view[HEADER_LEN:end])), end)


def _need_int(v: int | None) -> int:
    if v is None:
        raise SyncError("truncated")
    return v


def _need_bytes(v: bytes | None) -> bytes:
    if v is None:
        raise SyncError("truncated")
    return v


def _read_text(r: Reader, minimum: int, maximum: int, what: str) -> str:
    n = _need_int(r.u16be())
    if n < minimum or n > maximum:
        raise SyncError("invalid_field", what)
    raw = _need_bytes(r.take(n))
    try:
        s = raw.decode("utf-8")
    except UnicodeDecodeError:
        raise SyncError("not_utf8") from None
    _check_text(n, s, minimum, maximum, what)
    return s


def _read_cursor(r: Reader, minimum: int) -> bytes:
    n = _need_int(r.u16be())
    if n < minimum or n > MAX_CURSOR_LEN:
        raise SyncError("invalid_field", "cursor")
    return _need_bytes(r.take(n))


def _decode_payload(ty: int, payload: bytes) -> Message:
    r = Reader(payload)
    msg: Message
    if ty == _TYPE_HELLO:
        min_version = _need_int(r.u16be())
        max_version = _need_int(r.u16be())
        _check_versions(min_version, max_version)
        msg = Hello(min_version, max_version, _read_text(r, 1, MAX_NAME_LEN, "agent"))
    elif ty == _TYPE_WELCOME:
        version = _need_int(r.u16be())
        max_payload = _need_int(r.u32be())
        _check_welcome(version, max_payload)
        msg = Welcome(version, max_payload, _read_text(r, 1, MAX_NAME_LEN, "server"))
    elif ty == _TYPE_SUBSCRIBE:
        sub = _need_int(r.u32be())
        _check_sub(sub)
        scope = _read_text(r, 1, MAX_SCOPE_LEN, "scope")
        msg = Subscribe(sub, scope, _read_cursor(r, 0))
    elif ty == _TYPE_EVENTS:
        sub = _need_int(r.u32be())
        _check_sub(sub)
        count = _need_int(r.u16be())
        if count == 0 or count > MAX_EVENTS_PER_FRAME:
            raise SyncError("invalid_field", "event count")
        events: list[SyncEvent] = []
        for _ in range(count):
            cursor = _read_cursor(r, 1)
            n = _need_int(r.u32be())
            events.append(SyncEvent(cursor, _need_bytes(r.take(n))))
        msg = Events(sub, tuple(events))
    elif ty == _TYPE_ACK:
        sub = _need_int(r.u32be())
        _check_sub(sub)
        msg = Ack(sub, _read_cursor(r, 1))
    elif ty == _TYPE_ERROR:
        code = error_code_from_number(_need_int(r.u16be()))
        if code is None:
            raise SyncError("invalid_field", "error code")
        sub = _need_int(r.u32be())
        msg = Error(code, sub, _read_text(r, 0, MAX_ERROR_MESSAGE_LEN, "message"))
    elif ty == _TYPE_PING:
        msg = Ping(_need_int(r.u64be()))
    elif ty == _TYPE_PONG:
        msg = Pong(_need_int(r.u64be()))
    else:
        raise SyncError("unknown_type", ty)
    if r.remaining != 0:
        raise SyncError("trailing")
    return msg
