"""Sans-I/O client for the sync protocol v1: the state machine of ``lattice_sdk::client``.

The caller owns the socket, the framing and the clock. It decodes frames with
:func:`lattice_client.sync.decode`, passes them to :meth:`SyncClient.handle` with a logical time,
writes every :class:`Send` action it gets back and calls :meth:`SyncClient.tick` now and then.

Delivery is at least once. After a reconnect the client resubscribes with the last cursor it
acknowledged, so the server may replay events the application already saw. The client drops every
event whose cursor is not strictly greater than the last one it delivered (by length first, then
byte by byte), so the application sees each event once and in order. Times and nonces are ``u64``
in the Rust client; here they are ``int`` and are range checked.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Final, Literal, Union

from ._bytes import U16_MAX, U32_MAX, U64_MAX, is_bytes_like, is_uint
from .sync import (
    MAX_CURSOR_LEN,
    MAX_ERROR_MESSAGE_LEN,
    MAX_NAME_LEN,
    MAX_SCOPE_LEN,
    Ack,
    ErrorCode,
    Events,
    Hello,
    Message,
    Ping,
    Pong,
    Subscribe,
    SyncEvent,
    Welcome,
    has_control_char,
)
from .sync import Error as ErrorFrame

SUPPORTED_MIN_VERSION: Final = 1
"""Lowest protocol version this client can speak."""
SUPPORTED_MAX_VERSION: Final = 1
"""Highest protocol version this client can speak."""

Phase = Literal["disconnected", "hello_sent", "ready", "subscribed", "closed"]
"""Connection phase."""


@dataclass(frozen=True, slots=True)
class Incompatible:
    """The server speaks no version the client offered."""


@dataclass(frozen=True, slots=True)
class ServerError:
    """The server sent a connection-level ``Error``."""

    code: ErrorCode
    message: str


@dataclass(frozen=True, slots=True)
class ProtocolViolation:
    """The server broke the protocol; ``rule`` names the rule."""

    rule: str


@dataclass(frozen=True, slots=True)
class PingTimeout:
    """A ``Ping`` was not answered in time."""


@dataclass(frozen=True, slots=True)
class HandshakeTimeout:
    """No ``Welcome`` arrived in time."""


@dataclass(frozen=True, slots=True)
class ByApplication:
    """The application called :meth:`SyncClient.close`."""


CloseReason = Union[
    Incompatible, ServerError, ProtocolViolation, PingTimeout, HandshakeTimeout, ByApplication
]
"""Why a connection ended."""


@dataclass(frozen=True, slots=True)
class Send:
    """Write ``frame`` to the transport."""

    frame: Message


@dataclass(frozen=True, slots=True)
class Ready:
    """The handshake finished."""

    version: int
    max_payload: int


@dataclass(frozen=True, slots=True)
class Deliver:
    """Hand one event to the application, once and in order."""

    sub: int
    cursor: bytes
    data: bytes


@dataclass(frozen=True, slots=True)
class SubscriptionFailed:
    """The server refused or ended one subscription; the connection stays up."""

    sub: int
    code: ErrorCode
    message: str


@dataclass(frozen=True, slots=True)
class Close:
    """Close the transport."""

    reason: CloseReason


Action = Union[Send, Ready, Deliver, SubscriptionFailed, Close]
"""What the caller must do after an input."""

ClientErrorKind = Literal[
    "invalid_config",
    "wrong_phase",
    "invalid_scope",
    "invalid_cursor",
    "unknown_subscription",
    "ack_beyond_delivered",
    "subscription_ids_exhausted",
    "invalid_time",
]


class ClientError(Exception):
    """A call that does not fit the client's state or limits.

    ``detail`` is the rejected setting for ``invalid_config``, the phase for ``wrong_phase`` and the
    subscription id for ``unknown_subscription``. ``invalid_time`` exists only here: Rust types a
    time as ``u64``, Python must check it. A call that raises leaves the client unchanged.
    """

    def __init__(self, kind: ClientErrorKind, detail: str | int | None = None) -> None:
        super().__init__(kind if detail is None else f"{kind}: {detail}")
        self.kind: ClientErrorKind = kind
        self.detail: str | int | None = detail

    def __eq__(self, other: object) -> bool:
        return (
            isinstance(other, ClientError)
            and self.kind == other.kind
            and self.detail == other.detail
        )

    def __hash__(self) -> int:
        return hash((self.kind, self.detail))


@dataclass(frozen=True, slots=True)
class ClientConfig:
    """Client settings. Times are in the caller's logical ticks."""

    agent: str
    """Agent name, 1 to 64 bytes of UTF-8 without control characters."""
    min_version: int = SUPPORTED_MIN_VERSION
    """Lowest version offered in ``Hello``, at least 1."""
    max_version: int = SUPPORTED_MAX_VERSION
    """Highest version offered in ``Hello``."""
    ping_interval: int = 30
    """Idle ticks after which the client sends a ``Ping``."""
    ping_timeout: int = 10
    """Ticks without an answer after which the connection is dropped, at least 1."""
    ack_batch: int = 1
    """Number of ``ack`` calls merged into one wire ``Ack``, at least 1."""


def default_config(agent: str) -> ClientConfig:
    """The supported version range, a 30 tick interval, a 10 tick timeout and no batching."""
    return ClientConfig(agent=agent)


def _utf8_len(text: str) -> int | None:
    """Length in UTF-8 bytes, or ``None`` if ``text`` is not a ``str`` or has a lone surrogate."""
    if not isinstance(text, str):
        return None
    try:
        return len(text.encode("utf-8"))
    except UnicodeEncodeError:
        return None


def _validate(cfg: ClientConfig) -> None:
    if (
        not is_uint(cfg.min_version, U16_MAX)
        or not is_uint(cfg.max_version, U16_MAX)
        or cfg.min_version == 0
        or cfg.min_version > cfg.max_version
    ):
        raise ClientError("invalid_config", "version range")
    name_len = _utf8_len(cfg.agent)
    if name_len is None or name_len == 0 or name_len > MAX_NAME_LEN or has_control_char(cfg.agent):
        raise ClientError("invalid_config", "agent name")
    if not is_uint(cfg.ack_batch, U32_MAX) or cfg.ack_batch < 1:
        raise ClientError("invalid_config", "ack batch")
    if not is_uint(cfg.ping_interval, U64_MAX):
        raise ClientError("invalid_config", "ping interval")
    if not is_uint(cfg.ping_timeout, U64_MAX) or cfg.ping_timeout == 0:
        raise ClientError("invalid_config", "ping timeout")


def _check_time(now: int) -> None:
    if not is_uint(now, U64_MAX):
        raise ClientError("invalid_time")


class _Sub:
    __slots__ = ("scope", "delivered", "acked", "processed", "pending_acks")

    def __init__(self, scope: str, cursor: bytes) -> None:
        self.scope = scope
        # Last cursor handed to the application (or the starting cursor).
        self.delivered = cursor
        # Last cursor sent in an `Ack` (or the starting cursor); the resume point.
        self.acked = cursor
        # Highest cursor the application acknowledged but that is not yet on the wire.
        self.processed: bytes | None = None
        # `ack` calls merged into `processed`.
        self.pending_acks = 0


def cursor_after(a: bytes, b: bytes) -> bool:
    """``a`` is strictly after ``b``: longer, or equal length and bytewise greater."""
    return (len(a), a) > (len(b), b)


def _clip(text: str) -> str:
    """At most 256 bytes of ``text``, cut on a character boundary."""
    out: list[str] = []
    used = 0
    for ch in text:
        n = len(ch.encode("utf-8"))
        if used + n > MAX_ERROR_MESSAGE_LEN:
            break
        out.append(ch)
        used += n
    return "".join(out)


class SyncClient:
    """The sync client state machine.

    Every method that the Rust client reports with ``Err`` raises a :class:`ClientError` here and
    leaves the client unchanged.
    """

    def __init__(self, cfg: ClientConfig) -> None:
        """Builds a client; refuses a configuration that would produce an invalid ``Hello``."""
        _validate(cfg)
        self._cfg = cfg
        self._phase: Phase = "disconnected"
        self._last_rx = 0
        self._ping: tuple[int, int] | None = None  # (nonce, sent at)
        self._next_nonce = 1
        # Subscription ids only grow and a removal never reorders, so insertion order is id order,
        # the order in which the Rust client (a BTreeMap) resubscribes.
        self._subs: dict[int, _Sub] = {}
        self._next_sub = 1
        self._version: int | None = None

    def phase(self) -> Phase:
        return self._phase

    def version(self) -> int | None:
        """The negotiated version, once the handshake finished on the current connection."""
        return self._version

    def subscriptions(self) -> list[int]:
        """Ids of the live subscriptions, ascending."""
        return list(self._subs)

    def resume_cursor(self, sub: int) -> bytes | None:
        """The cursor a reconnect would resume ``sub`` from (the last acknowledged one)."""
        s = self._subs.get(sub)
        return None if s is None else s.acked

    def connect(self, now: int) -> list[Action]:
        """Starts the first connection: returns the ``Hello`` to send."""
        _check_time(now)
        if self._phase != "disconnected":
            raise ClientError("wrong_phase", self._phase)
        return self._begin(now)

    def reconnect(self, now: int) -> list[Action]:
        """Starts a new connection after the old one ended; subscriptions resume on ``Welcome``."""
        return self.connect(now)

    def disconnected(self) -> None:
        """The transport dropped without a protocol-level reason. Subscriptions are kept."""
        if self._phase != "closed":
            self._phase = "disconnected"
            self._ping = None
            self._version = None

    def close(self) -> list[Action]:
        """The application is done. Terminal."""
        if self._phase == "closed":
            return []
        self._phase = "closed"
        return [Close(ByApplication())]

    def _begin(self, now: int) -> list[Action]:
        self._phase = "hello_sent"
        self._last_rx = now
        self._ping = None
        self._version = None
        cfg = self._cfg
        return [Send(Hello(cfg.min_version, cfg.max_version, cfg.agent))]

    def subscribe(self, scope: str, cursor: bytes = b"") -> tuple[int, list[Action]]:
        """Starts a subscription and returns its id and the actions.

        ``cursor`` is empty to start from the beginning. Allowed once the handshake finished.
        """
        if self._phase not in ("ready", "subscribed"):
            raise ClientError("wrong_phase", self._phase)
        scope_len = _utf8_len(scope)
        if (
            scope_len is None
            or scope_len == 0
            or scope_len > MAX_SCOPE_LEN
            or has_control_char(scope)
        ):
            raise ClientError("invalid_scope")
        if not is_bytes_like(cursor) or len(cursor) > MAX_CURSOR_LEN:
            raise ClientError("invalid_cursor")
        sub = self._next_sub
        if sub == U32_MAX:
            raise ClientError("subscription_ids_exhausted")
        start = bytes(cursor)
        self._next_sub = sub + 1
        self._subs[sub] = _Sub(scope, start)
        self._phase = "subscribed"
        return sub, [Send(Subscribe(sub, scope, start))]

    def ack(self, sub: int, cursor: bytes) -> list[Action]:
        """The application finished processing everything up to ``cursor`` on ``sub``.

        Calls are batched: a wire ``Ack`` is produced every ``ack_batch`` calls, or by
        :meth:`flush_acks`.
        """
        s = self._subs.get(sub)
        if s is None:
            raise ClientError("unknown_subscription", sub)
        if not is_bytes_like(cursor) or len(cursor) == 0 or len(cursor) > MAX_CURSOR_LEN:
            raise ClientError("invalid_cursor")
        cur = bytes(cursor)
        if cursor_after(cur, s.delivered):
            raise ClientError("ack_beyond_delivered")
        newest = (
            cursor_after(cur, s.processed)
            if s.processed is not None
            else cursor_after(cur, s.acked)
        )
        if newest:
            s.processed = cur
        s.pending_acks = min(s.pending_acks + 1, U32_MAX)
        if s.pending_acks >= self._cfg.ack_batch:
            return self._take_ack(sub, s)
        return []

    def flush_acks(self) -> list[Action]:
        """Sends the pending acknowledgement of every subscription now."""
        out: list[Action] = []
        for sub_id, s in self._subs.items():
            out.extend(self._take_ack(sub_id, s))
        return out

    def _take_ack(self, sub_id: int, s: _Sub) -> list[Action]:
        s.pending_acks = 0
        cursor = s.processed
        s.processed = None
        if cursor is None:
            return []
        # The acknowledgement counts as given even if the connection is down: the resume point
        # moves, and the server learns it from the next Subscribe.
        s.acked = cursor
        if self._phase in ("ready", "subscribed"):
            return [Send(Ack(sub_id, cursor))]
        return []

    def tick(self, now: int) -> list[Action]:
        """Advances the logical clock.

        Sends a ``Ping`` after ``ping_interval`` idle ticks and drops the connection when an answer
        is overdue by ``ping_timeout``.
        """
        _check_time(now)
        idle = max(now - self._last_rx, 0)
        if self._phase in ("disconnected", "closed"):
            return []
        if self._phase == "hello_sent":
            if idle >= self._cfg.ping_timeout:
                return self._drop_connection(HandshakeTimeout())
            return []
        if self._ping is not None:
            _, sent = self._ping
            if max(now - sent, 0) >= self._cfg.ping_timeout:
                return self._drop_connection(PingTimeout())
            return []
        if idle >= self._cfg.ping_interval:
            nonce = self._next_nonce
            self._next_nonce = (nonce + 1) & U64_MAX
            self._ping = (nonce, now)
            return [Send(Ping(nonce))]
        return []

    def _drop_connection(self, reason: CloseReason) -> list[Action]:
        self._phase = "disconnected"
        self._ping = None
        self._version = None
        return [Close(reason)]

    def _violation(self, rule: str) -> list[Action]:
        out: list[Action] = [Send(ErrorFrame("malformed", 0, _clip(rule)))]
        out.extend(self._drop_connection(ProtocolViolation(rule)))
        return out

    def handle(self, frame: Message, now: int) -> list[Action]:
        """Feeds one decoded frame from the server."""
        _check_time(now)
        if self._phase in ("disconnected", "closed"):
            raise ClientError("wrong_phase", self._phase)
        if not isinstance(frame, (Hello, Welcome, Subscribe, Events, Ack, ErrorFrame, Ping, Pong)):
            raise TypeError("handle expects a sync message")
        self._last_rx = now
        if isinstance(frame, Welcome):
            return self._on_welcome(frame.version, frame.max_payload)
        if isinstance(frame, Events):
            if self._phase == "hello_sent":
                return self._violation("events before welcome")
            return self._on_events(frame.sub, frame.events)
        if isinstance(frame, ErrorFrame):
            return self._on_error(frame.code, frame.sub, frame.message)
        if isinstance(frame, Ping):
            return [Send(Pong(frame.nonce))]
        if isinstance(frame, Pong):
            if self._ping is not None and self._ping[0] == frame.nonce:
                self._ping = None
            return []
        return self._violation("client-to-server frame received")

    def _on_welcome(self, version: int, max_payload: int) -> list[Action]:
        if self._phase != "hello_sent":
            return self._violation("welcome outside handshake")
        if version < self._cfg.min_version or version > self._cfg.max_version:
            self._phase = "closed"
            self._ping = None
            return [Close(Incompatible())]
        self._version = version
        out: list[Action] = [Ready(version, max_payload)]
        # Resume: every live subscription restarts from its last acknowledged cursor. Events the
        # application already saw are filtered out by `_on_events`.
        for sub_id, s in self._subs.items():
            out.append(Send(Subscribe(sub_id, s.scope, s.acked)))
        self._phase = "ready" if not self._subs else "subscribed"
        return out

    def _on_events(self, sub: int, events: tuple[SyncEvent, ...]) -> list[Action]:
        s = self._subs.get(sub)
        if s is None:
            # A late frame for a subscription that failed or never existed: nothing to deliver.
            return []
        out: list[Action] = []
        for ev in events:
            if len(ev.cursor) > MAX_CURSOR_LEN or not cursor_after(ev.cursor, s.delivered):
                continue
            s.delivered = bytes(ev.cursor)
            out.append(Deliver(sub, bytes(ev.cursor), bytes(ev.data)))
        return out

    def _on_error(self, code: ErrorCode, sub: int, message: str) -> list[Action]:
        if sub != 0:
            if self._subs.pop(sub, None) is not None:
                if not self._subs and self._phase == "subscribed":
                    self._phase = "ready"
                return [SubscriptionFailed(sub, code, message)]
            return []
        if code == "unsupported_version":
            self._phase = "closed"
            self._ping = None
            self._version = None
            return [Close(Incompatible())]
        return self._drop_connection(ServerError(code, message))
