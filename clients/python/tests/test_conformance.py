"""Every vector the Rust side wrote must decode, encode and fail exactly the same way here."""

from __future__ import annotations

import unittest
from typing import Any

from lattice_client import (
    MAX_PAYLOAD,
    Decoded,
    FeedDecodeError,
    FeedError,
    SyncError,
    compare_cursors,
    cursor_from_bytes,
    cursor_to_bytes,
    decode,
    decode_event,
    decode_with_limit,
    encode,
    encode_event,
    from_hex,
    negotiate,
    to_hex,
)

from .support import (
    Base,
    big,
    bytes_of,
    event_from_json,
    load_vectors,
    message_from_json,
)

VECTORS = load_vectors()
SYNC = VECTORS["sync"]
FEED = VECTORS["feed"]


def rows(parent: dict[str, Any], key: str) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = parent[key]
    assert len(out) > 0, f"no vectors under {key}"
    return out


class SyncVectors(Base):
    def test_vector_file_shape(self) -> None:
        self.assertEqual(VECTORS["format"], "lattice-conformance-vectors")
        self.assertEqual(VECTORS["version"], 1)

    def test_valid_frames_decode_and_encode_byte_for_byte(self) -> None:
        for v in rows(SYNC, "valid"):
            name = v["name"]
            raw = from_hex(v["bytes"])
            message = message_from_json(v["message"])
            decoded = decode(raw)
            self.assertEqual(decoded, Decoded(message, len(raw)), f"{name}: decode")
            self.assertEqual(to_hex(encode(message)), to_hex(raw), f"{name}: encode")
            assert decoded is not None
            self.assertEqual(encode(decoded.message), raw, f"{name}: encode(decode(bytes))")

    def test_a_frame_followed_by_more_bytes_decodes_alone(self) -> None:
        for v in rows(SYNC, "valid"):
            raw = from_hex(v["bytes"])
            decoded = decode(raw + b"\xff\x00\x01")
            assert decoded is not None
            self.assertEqual(decoded.length, len(raw), v["name"])

    def test_every_proper_prefix_of_a_valid_frame_is_incomplete(self) -> None:
        for v in rows(SYNC, "valid"):
            raw = from_hex(v["bytes"])
            for cut in range(len(raw)):
                self.assertIsNone(decode(raw[:cut]), f"{v['name']}: prefix {cut}")

    def test_invalid_frames_fail_with_the_error_rust_reports(self) -> None:
        for v in rows(SYNC, "invalid"):
            limit = v.get("limit", MAX_PAYLOAD)
            with self.assertRaises(SyncError, msg=v["name"]) as cm:
                decode_with_limit(from_hex(v["bytes"]), limit)
            self.assert_error_matches(cm.exception, v["error"], v["name"])

    def test_incomplete_input_returns_none(self) -> None:
        for v in rows(SYNC, "incomplete"):
            self.assertIsNone(decode(from_hex(v["bytes"])), v["name"])

    def test_the_encoder_refuses_what_rust_refuses(self) -> None:
        for v in rows(SYNC, "encode_errors"):
            with self.assertRaises(SyncError, msg=v["name"]) as cm:
                encode(message_from_json(v["message"]))
            self.assert_error_matches(cm.exception, v["error"], v["name"])

    def test_version_negotiation_gives_rusts_answer(self) -> None:
        for v in rows(SYNC, "negotiate"):
            got = negotiate(v["min"], v["max"])
            want = v["result"]
            label = f"{v['min']}..={v['max']}"
            if "version" in want:
                self.assertEqual((got.version, got.error), (want["version"], None), label)
            else:
                self.assertEqual((got.version, got.error), (None, want["error"]), label)

    def test_the_vectors_cover_every_message_type_and_error_kind(self) -> None:
        types = {v["message"]["type"] for v in rows(SYNC, "valid")}
        self.assertEqual(
            sorted(types), ["ack", "error", "events", "hello", "ping", "pong", "subscribe", "welcome"]
        )
        kinds = {v["error"]["kind"] for v in rows(SYNC, "invalid")}
        self.assertEqual(
            sorted(kinds),
            ["bad_length", "invalid_field", "not_utf8", "trailing", "truncated", "unknown_type"],
        )
        encode_kinds = {v["error"]["kind"] for v in rows(SYNC, "encode_errors")}
        self.assertTrue({"too_large", "invalid_field"} <= encode_kinds)

    def test_an_events_frame_can_carry_a_feed_event(self) -> None:
        v = next(r for r in rows(SYNC, "valid") if r["name"] == "events_carrying_a_feed_event")
        decoded = decode(from_hex(v["bytes"]))
        assert decoded is not None and hasattr(decoded.message, "events")
        event = decoded.message.events[0]
        inner = decode_event(event.data)
        self.assertEqual(cursor_to_bytes(inner.cursor), event.cursor)


class FeedVectors(Base):
    def test_cursors_encode_and_decode_as_rust_does_and_bytes_order_like_cursors(self) -> None:
        listing = rows(FEED, "cursors")
        for v in listing:
            name = v["name"]
            cursor = cursor_from_bytes(bytes_of(v["bytes"]))
            self.assertEqual(
                (cursor.lsn, cursor.index), (big(v["cursor"]["lsn"]), v["cursor"]["index"]), name
            )
            self.assertEqual(cursor_to_bytes(cursor), bytes_of(v["bytes"]), f"{name}: encode")
        for a in listing:
            for b in listing:
                ba, bb = bytes_of(a["bytes"]), bytes_of(b["bytes"])
                by_bytes = (ba > bb) - (ba < bb)
                self.assertEqual(
                    by_bytes,
                    compare_cursors(cursor_from_bytes(ba), cursor_from_bytes(bb)),
                    f"{a['name']} vs {b['name']}",
                )

    def test_invalid_cursors_fail_with_rusts_error(self) -> None:
        for v in rows(FEED, "cursors_invalid"):
            with self.assertRaises(FeedDecodeError, msg=v["name"]) as cm:
                cursor_from_bytes(from_hex(v["bytes"]))
            self.assert_error_matches(cm.exception, v["error"], v["name"])

    def test_valid_events_decode_and_encode_byte_for_byte(self) -> None:
        for v in rows(FEED, "events"):
            name = v["name"]
            raw = from_hex(v["bytes"])
            event = event_from_json(v["event"])
            self.assertEqual(decode_event(raw), event, f"{name}: decode")
            self.assertEqual(encode_event(event), raw, f"{name}: encode")
            self.assertEqual(encode_event(decode_event(raw)), raw, f"{name}: round trip")

    def test_every_proper_prefix_of_a_valid_event_is_truncated(self) -> None:
        for v in rows(FEED, "events"):
            raw = from_hex(v["bytes"])
            for cut in range(len(raw)):
                with self.assertRaises(FeedDecodeError, msg=f"{v['name']}: prefix {cut}") as cm:
                    decode_event(raw[:cut])
                self.assertEqual(cm.exception.kind, "truncated", f"{v['name']}: prefix {cut}")

    def test_invalid_events_fail_with_rusts_error(self) -> None:
        for v in rows(FEED, "events_invalid"):
            with self.assertRaises(FeedDecodeError, msg=v["name"]) as cm:
                decode_event(from_hex(v["bytes"]))
            self.assert_error_matches(cm.exception, v["error"], v["name"])

    def test_the_encoder_refuses_what_rust_refuses(self) -> None:
        for v in rows(FEED, "encode_errors"):
            with self.assertRaises(FeedError, msg=v["name"]) as cm:
                encode_event(event_from_json(v["event"]))
            self.assert_error_matches(cm.exception, v["error"], v["name"])


if __name__ == "__main__":
    unittest.main()
