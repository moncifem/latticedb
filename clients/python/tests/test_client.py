"""The client state machine, case by case: the same scenarios as the tests of
``lattice_sdk::client`` in the Rust SDK."""

from __future__ import annotations

import dataclasses
import unittest

from lattice_client import (
    Ack,
    Action,
    ByApplication,
    ClientConfig,
    ClientError,
    Close,
    Deliver,
    Error,
    Events,
    HandshakeTimeout,
    Hello,
    Incompatible,
    Message,
    Ping,
    PingTimeout,
    Pong,
    ProtocolViolation,
    Ready,
    Send,
    ServerError,
    Subscribe,
    SubscriptionFailed,
    SyncClient,
    SyncEvent,
    Welcome,
    default_config,
)


def cur(n: int) -> bytes:
    return n.to_bytes(8, "big")


def ev(n: int) -> SyncEvent:
    return SyncEvent(cur(n), bytes([n & 0xFF]))


def events(sub: int, first: int, last: int) -> Events:
    return Events(sub, tuple(ev(n) for n in range(first, last + 1)))


def welcome(version: int) -> Welcome:
    return Welcome(version, 1 << 20, "srv")


def delivered(actions: list[Action]) -> list[int]:
    return [int.from_bytes(a.cursor, "big") for a in actions if isinstance(a, Deliver)]


def client(batch: int) -> SyncClient:
    return SyncClient(dataclasses.replace(default_config("sdk-py"), ack_batch=batch))


def ready(c: SyncClient) -> None:
    c.connect(0)
    c.handle(welcome(1), 0)


def send(frame: Message) -> Action:
    return Send(frame)


class ClientScenarios(unittest.TestCase):
    def test_happy_path(self) -> None:
        c = client(1)
        self.assertEqual(c.connect(0), [send(Hello(1, 1, "sdk-py"))])
        self.assertEqual(c.phase(), "hello_sent")
        self.assertEqual(c.handle(welcome(1), 1), [Ready(1, 1 << 20)])
        self.assertEqual(c.phase(), "ready")
        sub, actions = c.subscribe("ws/main", b"")
        self.assertEqual(sub, 1)
        self.assertEqual(actions, [send(Subscribe(1, "ws/main", b""))])
        self.assertEqual(c.phase(), "subscribed")
        self.assertEqual(delivered(c.handle(events(1, 1, 3), 2)), [1, 2, 3])
        self.assertEqual(c.ack(1, cur(3)), [send(Ack(1, cur(3)))])
        self.assertEqual(c.resume_cursor(1), cur(3))

    def test_subscribing_before_the_welcome_is_refused(self) -> None:
        c = client(1)
        with self.assertRaises(ClientError) as cm:
            c.subscribe("s", b"")
        self.assertEqual(cm.exception, ClientError("wrong_phase", "disconnected"))
        c.connect(0)
        with self.assertRaises(ClientError) as cm2:
            c.subscribe("s", b"")
        self.assertEqual(cm2.exception, ClientError("wrong_phase", "hello_sent"))

    def test_a_client_newer_than_the_server_negotiates_down(self) -> None:
        c = SyncClient(dataclasses.replace(default_config("a"), max_version=3))
        self.assertEqual(c.connect(0), [send(Hello(1, 3, "a"))])
        c.handle(welcome(1), 0)
        self.assertEqual(c.phase(), "ready")
        self.assertEqual(c.version(), 1)

    def test_a_welcome_outside_the_offered_range_is_incompatible_and_terminal(self) -> None:
        c = client(1)
        c.connect(0)
        self.assertEqual(c.handle(welcome(2), 0), [Close(Incompatible())])
        self.assertEqual(c.phase(), "closed")
        with self.assertRaises(ClientError) as cm:
            c.reconnect(1)
        self.assertEqual(cm.exception, ClientError("wrong_phase", "closed"))

    def test_an_unsupported_version_error_is_terminal(self) -> None:
        c = client(1)
        c.connect(0)
        out = c.handle(Error("unsupported_version", 0, "version"), 0)
        self.assertEqual(out, [Close(Incompatible())])
        self.assertEqual(c.phase(), "closed")

    def test_a_reconnect_resumes_from_the_last_acked_cursor_and_drops_replayed_duplicates(self) -> None:
        c = client(1)
        ready(c)
        sub, _ = c.subscribe("ws/main", b"")
        self.assertEqual(delivered(c.handle(events(sub, 1, 5), 1)), [1, 2, 3, 4, 5])
        c.ack(sub, cur(3))
        c.disconnected()
        self.assertEqual(c.phase(), "disconnected")
        c.reconnect(10)
        a = c.handle(welcome(1), 10)
        self.assertEqual(a[1], send(Subscribe(sub, "ws/main", cur(3))))
        self.assertEqual(c.phase(), "subscribed")
        # The server replays 4 and 5 (already delivered) and then continues.
        self.assertEqual(delivered(c.handle(events(sub, 4, 7), 11)), [6, 7])
        # A full duplicate batch yields nothing.
        self.assertEqual(delivered(c.handle(events(sub, 1, 7), 12)), [])

    def test_ack_batching_and_flushing(self) -> None:
        c = client(3)
        ready(c)
        sub, _ = c.subscribe("s", b"")
        c.handle(events(sub, 1, 4), 0)
        self.assertEqual(c.ack(sub, cur(1)), [])
        self.assertEqual(c.ack(sub, cur(2)), [])
        self.assertEqual(c.ack(sub, cur(3)), [send(Ack(sub, cur(3)))])
        self.assertEqual(c.ack(sub, cur(4)), [])
        self.assertEqual(c.resume_cursor(sub), cur(3))
        self.assertEqual(c.flush_acks(), [send(Ack(sub, cur(4)))])
        self.assertEqual(c.flush_acks(), [])
        with self.assertRaises(ClientError) as cm:
            c.ack(sub, cur(9))
        self.assertEqual(cm.exception, ClientError("ack_beyond_delivered"))
        with self.assertRaises(ClientError) as cm2:
            c.ack(9, cur(1))
        self.assertEqual(cm2.exception, ClientError("unknown_subscription", 9))

    def test_an_older_ack_does_not_move_the_resume_point_back(self) -> None:
        c = client(1)
        ready(c)
        sub, _ = c.subscribe("s", b"")
        c.handle(events(sub, 1, 4), 0)
        c.ack(sub, cur(3))
        self.assertEqual(c.ack(sub, cur(2)), [])
        self.assertEqual(c.resume_cursor(sub), cur(3))

    def test_an_acknowledgement_given_while_disconnected_still_moves_the_resume_point(self) -> None:
        c = client(1)
        ready(c)
        sub, _ = c.subscribe("s", b"")
        c.handle(events(sub, 1, 3), 0)
        c.disconnected()
        self.assertEqual(c.ack(sub, cur(2)), [])
        self.assertEqual(c.resume_cursor(sub), cur(2))

    def test_a_subscription_error_removes_only_that_subscription(self) -> None:
        c = client(1)
        ready(c)
        a, _ = c.subscribe("a", b"")
        b, _ = c.subscribe("b", b"")
        out = c.handle(Error("invalid_scope", a, "no"), 0)
        self.assertEqual(out, [SubscriptionFailed(a, "invalid_scope", "no")])
        self.assertEqual(c.subscriptions(), [b])
        self.assertEqual(c.phase(), "subscribed")
        c.handle(Error("invalid_cursor", b, ""), 0)
        self.assertEqual(c.phase(), "ready")
        # An error for a subscription that is gone is ignored.
        self.assertEqual(c.handle(Error("internal", 77, ""), 0), [])

    def test_a_connection_error_drops_the_connection_but_allows_a_reconnect(self) -> None:
        c = client(1)
        ready(c)
        out = c.handle(Error("internal", 0, "boom"), 0)
        self.assertEqual(out, [Close(ServerError("internal", "boom"))])
        self.assertEqual(c.phase(), "disconnected")
        c.reconnect(1)

    def test_protocol_violations_send_malformed_and_drop(self) -> None:
        c = client(1)
        ready(c)
        out = c.handle(welcome(1), 0)
        self.assertEqual(out[0], send(Error("malformed", 0, "welcome outside handshake")))
        self.assertEqual(out[1], Close(ProtocolViolation("welcome outside handshake")))
        self.assertEqual(c.phase(), "disconnected")
        with self.assertRaises(ClientError):
            c.handle(welcome(1), 0)

    def test_events_before_the_welcome_and_client_to_server_frames_are_violations(self) -> None:
        c = client(1)
        c.connect(0)
        self.assertIsInstance(c.handle(events(1, 1, 1), 0)[1], Close)
        c.reconnect(1)
        out = c.handle(Ack(1, b"\x01"), 1)
        self.assertEqual(out[1], Close(ProtocolViolation("client-to-server frame received")))
        for frame in (Hello(1, 1, "x"), Subscribe(1, "s", b"")):
            c.reconnect(2)
            out = c.handle(frame, 2)
            self.assertEqual(out[1], Close(ProtocolViolation("client-to-server frame received")))

    def test_a_server_ping_is_answered_with_the_same_nonce(self) -> None:
        c = client(1)
        ready(c)
        self.assertEqual(c.handle(Ping(77), 1), [send(Pong(77))])

    def test_a_ping_goes_out_when_idle_and_an_unanswered_one_times_out(self) -> None:
        c = client(1)
        ready(c)
        self.assertEqual(c.tick(29), [])
        self.assertEqual(c.tick(30), [send(Ping(1))])
        self.assertEqual(c.tick(39), [])
        self.assertEqual(c.tick(40), [Close(PingTimeout())])
        self.assertEqual(c.phase(), "disconnected")

    def test_a_matching_pong_clears_the_ping_and_a_wrong_nonce_does_not(self) -> None:
        c = client(1)
        ready(c)
        c.tick(30)
        c.handle(Pong(99), 31)
        c.handle(Pong(1), 32)
        self.assertEqual(c.tick(40), [])
        self.assertEqual(c.phase(), "ready")
        # The next ping is due 30 ticks after the last frame.
        self.assertEqual(c.tick(62), [send(Ping(2))])

    def test_a_wrong_pong_nonce_leaves_the_timeout_running(self) -> None:
        c = client(1)
        ready(c)
        c.tick(30)
        c.handle(Pong(99), 31)
        self.assertEqual(c.tick(40), [Close(PingTimeout())])

    def test_the_handshake_times_out(self) -> None:
        c = client(1)
        c.connect(5)
        self.assertEqual(c.tick(14), [])
        self.assertEqual(c.tick(15), [Close(HandshakeTimeout())])
        self.assertEqual(c.phase(), "disconnected")

    def test_a_clock_that_runs_backwards_does_not_underflow(self) -> None:
        c = client(1)
        c.connect(100)
        self.assertEqual(c.tick(0), [])

    def test_the_nonce_wraps_at_u64(self) -> None:
        c = client(1)
        ready(c)
        c._next_nonce = (1 << 64) - 1  # the only private access: reaching 2**64 pings is not feasible
        self.assertEqual(c.tick(30), [send(Ping((1 << 64) - 1))])
        c.handle(Pong((1 << 64) - 1), 31)
        self.assertEqual(c.tick(61), [send(Ping(0))])

    def test_close_is_terminal_and_idempotent(self) -> None:
        c = client(1)
        ready(c)
        self.assertEqual(c.close(), [Close(ByApplication())])
        self.assertEqual(c.close(), [])
        self.assertEqual(c.phase(), "closed")
        c.disconnected()
        self.assertEqual(c.phase(), "closed")

    def test_invalid_inputs_are_errors(self) -> None:
        base = default_config("a")
        for patch in (
            {"agent": ""},
            {"agent": "a" * 65},
            {"agent": "a\nb"},
            {"agent": "a\ud800"},
            {"agent": 5},
            {"min_version": 0},
            {"min_version": 2, "max_version": 1},
            {"max_version": 65_536},
            {"max_version": True},
            {"ack_batch": 0},
            {"ack_batch": 1 << 32},
            {"ping_timeout": 0},
            {"ping_timeout": -1},
            {"ping_interval": 1 << 64},
            {"ping_interval": 1.5},
        ):
            with self.assertRaises(ClientError, msg=repr(patch)):
                SyncClient(dataclasses.replace(base, **patch))
        c = client(1)
        ready(c)
        for scope, cursor, kind in (
            ("", b"", "invalid_scope"),
            ("a\nb", b"", "invalid_scope"),
            ("a" * 513, b"", "invalid_scope"),
            ("a", bytes(65), "invalid_cursor"),
        ):
            with self.assertRaises(ClientError) as cm:
                c.subscribe(scope, cursor)
            self.assertEqual(cm.exception.kind, kind)
        self.assertEqual(c.subscribe("a" * 512, bytes(64))[0], 1)

    def test_times_outside_u64_are_refused_and_change_nothing(self) -> None:
        c = client(1)
        for bad in (-1, 1 << 64, 1.5, True):
            with self.assertRaises(ClientError) as cm:
                c.connect(bad)  # type: ignore[arg-type]
            self.assertEqual(cm.exception.kind, "invalid_time")
        self.assertEqual(c.phase(), "disconnected")
        c.connect(0)
        with self.assertRaises(ClientError):
            c.tick(1 << 64)
        with self.assertRaises(ClientError):
            c.handle(welcome(1), -1)
        self.assertEqual(c.phase(), "hello_sent")

    def test_the_client_keeps_its_own_copy_of_what_it_is_given(self) -> None:
        c = client(1)
        ready(c)
        start = bytearray(b"\x00\x00\x00\x00\x00\x00\x00\x01")
        sub, _ = c.subscribe("s", start)  # type: ignore[arg-type]
        start[:] = b"\x09" * 8
        self.assertEqual(c.resume_cursor(sub), b"\x00\x00\x00\x00\x00\x00\x00\x01")

    def test_cursors_compare_by_length_first_then_by_bytes(self) -> None:
        c = client(1)
        ready(c)
        sub, _ = c.subscribe("s", b"")
        out = c.handle(
            Events(
                sub,
                (
                    SyncEvent(b"\xff", b""),
                    SyncEvent(b"\x00\x00", b""),  # longer, so after
                    SyncEvent(b"\xff", b""),  # shorter again, dropped
                    SyncEvent(b"\x00\x01", b""),
                ),
            ),
            0,
        )
        self.assertEqual(len([a for a in out if isinstance(a, Deliver)]), 3)

    def test_a_scope_or_agent_with_a_lone_surrogate_is_refused_before_any_state_changes(self) -> None:
        c = client(1)
        ready(c)
        for scope in ("\ud800", "a\udc00b", "ok\ud83d"):
            with self.assertRaises(ClientError) as cm:
                c.subscribe(scope, b"")
            self.assertEqual(cm.exception, ClientError("invalid_scope"))
        self.assertEqual(c.subscriptions(), [])
        self.assertEqual(c.phase(), "ready")
        # The next valid subscription still gets id 1.
        self.assertEqual(c.subscribe("ws/\U0001f600", b"")[0], 1)
        with self.assertRaises(ClientError) as cm2:
            SyncClient(ClientConfig(agent="a\ud800"))
        self.assertEqual(cm2.exception, ClientError("invalid_config", "agent name"))

    def test_subscription_ids_run_out_at_u32_max(self) -> None:
        c = client(1)
        ready(c)
        c._next_sub = (1 << 32) - 1
        with self.assertRaises(ClientError) as cm:
            c.subscribe("s", b"")
        self.assertEqual(cm.exception, ClientError("subscription_ids_exhausted"))
        self.assertEqual(c.subscriptions(), [])

    def test_an_error_message_longer_than_the_limit_is_clipped_on_a_character_boundary(self) -> None:
        from lattice_client.client import _clip

        self.assertEqual(len(_clip("a" * 300)), 256)
        clipped = _clip("é" * 200)  # two bytes each
        self.assertEqual(len(clipped.encode("utf-8")), 256)
        self.assertEqual(len(_clip("a" * 255 + "é")), 255)


if __name__ == "__main__":
    unittest.main()
