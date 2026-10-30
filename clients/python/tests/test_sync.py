"""Sync codec behaviour that does not depend on the shared vectors: the worked examples of the
format document (copied, not generated), limits, and input handling specific to Python."""

from __future__ import annotations

import unittest

from lattice_client import (
    MAX_PAYLOAD,
    Ack,
    Decoded,
    Error,
    Events,
    Hello,
    Message,
    Ping,
    Pong,
    Subscribe,
    SyncError,
    SyncEvent,
    Welcome,
    decode,
    decode_with_limit,
    encode,
    from_hex,
    to_hex,
)


def hx(text: str) -> bytes:
    return from_hex("".join(c for c in text if c not in " |\n"))


class SyncCodec(unittest.TestCase):
    def test_worked_examples_of_the_format_document(self) -> None:
        examples: list[tuple[Message, str]] = [
            (Hello(1, 1, "sdk-rs"), "01 0000000c | 0001 0001 0006 73646b2d7273"),
            (
                Subscribe(2, "ws/main", hx("0102030405060708")),
                "03 00000017 | 00000002 0007 77732f6d61696e 0008 0102030405060708",
            ),
            (
                Events(1, (SyncEvent(hx("0001"), hx("61")), SyncEvent(hx("0002"), b""))),
                "04 00000017 | 00000001 0002 | 0002 0001 00000001 61 | 0002 0002 00000000",
            ),
            (Pong(0x0102030405060708), "08 00000008 | 0102030405060708"),
        ]
        for message, text in examples:
            self.assertEqual(to_hex(encode(message)), to_hex(hx(text)))
            self.assertEqual(decode(hx(text)), Decoded(message, len(hx(text))))

    def test_a_frame_of_exactly_the_maximum_payload_round_trips_and_one_more_is_refused(self) -> None:
        # Overhead of an Events frame with one event and a one-byte cursor: 4 + 2 + (2 + 1) + 4.
        room = MAX_PAYLOAD - 13

        def at(n: int) -> Events:
            return Events(1, (SyncEvent(b"\x01", b"\x07" * n),))

        frame = encode(at(room))
        self.assertEqual(len(frame), MAX_PAYLOAD + 5)
        self.assertEqual(decode(frame), Decoded(at(room), len(frame)))
        with self.assertRaises(SyncError) as cm:
            encode(at(room + 1))
        self.assertEqual(cm.exception.kind, "too_large")

    def test_a_lowered_payload_limit_applies_and_a_higher_one_is_clamped(self) -> None:
        ping = encode(Ping(1))
        with self.assertRaises(SyncError) as cm:
            decode_with_limit(ping, 7)
        self.assertEqual(cm.exception.kind, "bad_length")
        self.assertIsNotNone(decode_with_limit(ping, 8))
        with self.assertRaises(SyncError) as cm2:
            decode_with_limit(hx("07 00100001"), 1 << 80)
        self.assertEqual(cm2.exception.detail, 1_048_577)

    def test_the_type_and_the_length_are_judged_from_the_header_alone(self) -> None:
        with self.assertRaises(SyncError) as cm:
            decode(hx("09"))
        self.assertEqual(cm.exception.kind, "unknown_type")
        with self.assertRaises(SyncError) as cm2:
            decode(hx("07 ffffffff"))
        self.assertEqual(cm2.exception.kind, "bad_length")
        self.assertIsNone(decode(hx("07 000000")))

    def test_decoding_accepts_bytearray_and_memoryview_slices(self) -> None:
        frame = encode(Ack(3, b"\x09\x09"))
        pool = bytearray(b"\xee" * 7) + bytearray(frame) + bytearray(b"\xee" * 3)
        view = memoryview(pool)[7 : 7 + len(frame)]
        self.assertEqual(decode(view), Decoded(Ack(3, b"\x09\x09"), len(frame)))
        self.assertEqual(decode(bytearray(frame)), Decoded(Ack(3, b"\x09\x09"), len(frame)))

    def test_a_decoded_message_does_not_alias_the_input(self) -> None:
        buf = bytearray(encode(Ack(1, b"\x01\x02")))
        decoded = decode(buf)
        assert decoded is not None
        buf[:] = bytes(len(buf))
        self.assertEqual(decoded.message, Ack(1, b"\x01\x02"))

    def test_encoding_does_not_keep_a_reference_to_the_message(self) -> None:
        cursor = bytearray(b"\x01\x02")
        frame = encode(Ack(1, cursor))  # type: ignore[arg-type]
        cursor[:] = b"\x00\x00"
        decoded = decode(frame)
        assert decoded is not None
        self.assertEqual(decoded.message, Ack(1, b"\x01\x02"))

    def test_text_with_a_lone_surrogate_cannot_be_encoded(self) -> None:
        for agent in ["\ud800", "a\udc00b", "\ud83d"]:
            with self.assertRaises(SyncError, msg=agent) as cm:
                encode(Hello(1, 1, agent))
            self.assertEqual(cm.exception.kind, "not_utf8")
        decoded = decode(encode(Hello(1, 1, "\U0001f600")))
        assert decoded is not None
        self.assertEqual(decoded.message, Hello(1, 1, "\U0001f600"))

    def test_a_byte_order_mark_is_kept_as_data(self) -> None:
        decoded = decode(encode(Hello(1, 1, "﻿a")))
        assert decoded is not None
        self.assertEqual(decoded.message, Hello(1, 1, "﻿a"))

    def test_numbers_outside_the_field_are_refused_instead_of_wrapping(self) -> None:
        bad: list[Message] = [
            Hello(1, 65_536, "a"),
            Hello(1.5, 2, "a"),  # type: ignore[arg-type]
            Hello(float("nan"), 2, "a"),  # type: ignore[arg-type]
            Hello(True, 2, "a"),
            Hello(1, 1 << 70, "a"),
            Subscribe(2**32, "s", b""),
            Subscribe(-1, "s", b""),
            Subscribe(True, "s", b""),
            Ack(1.5, b"\x01"),  # type: ignore[arg-type]
            Error("internal", 2**32, ""),
            Ping(1 << 64),
            Pong(-1),
            Pong(1 << 200),
            Welcome(1, 2**32, "s"),
        ]
        for m in bad:
            with self.assertRaises(SyncError, msg=repr(m)) as cm:
                encode(m)
            self.assertEqual(cm.exception.kind, "invalid_field", repr(m))

    def test_a_code_outside_the_defined_set_cannot_be_encoded(self) -> None:
        with self.assertRaises(SyncError) as cm:
            encode(Error("nope", 0, ""))  # type: ignore[arg-type]
        self.assertEqual(cm.exception.kind, "invalid_field")

    def test_a_stream_of_frames_decodes_one_at_a_time(self) -> None:
        stream = encode(Ping(1)) + encode(Pong(2)) + encode(Ack(1, b"\x05"))
        kinds = []
        while (d := decode(stream)) is not None:
            kinds.append(type(d.message).__name__)
            stream = stream[d.length :]
        self.assertEqual(kinds, ["Ping", "Pong", "Ack"])
        self.assertEqual(stream, b"")

    def test_a_payload_limit_that_is_not_a_non_negative_int_is_refused(self) -> None:
        ping = encode(Ping(1))
        for limit in (-1, 1.5, float("inf"), True, None, "8"):
            with self.assertRaises(SyncError, msg=repr(limit)) as cm:
                decode_with_limit(ping, limit)  # type: ignore[arg-type]
            self.assertEqual((cm.exception.kind, cm.exception.detail), ("invalid_field", "limit"))
        self.assertIsNotNone(decode_with_limit(ping, 8))

    def test_an_unknown_message_object_is_refused(self) -> None:
        with self.assertRaises(SyncError) as cm:
            encode("ping")  # type: ignore[arg-type]
        self.assertEqual(cm.exception.kind, "unknown_type")

    def test_control_characters_follow_unicode_category_cc(self) -> None:
        for ch in ("\x00", "\x1f", "\x7f", "\x80", "\x9f"):
            with self.assertRaises(SyncError, msg=repr(ch)):
                encode(Hello(1, 1, f"a{ch}b"))
        # U+00A0 and U+2028 are not Cc.
        for ch in ("\xa0", " "):
            self.assertIsNotNone(decode(encode(Hello(1, 1, f"a{ch}b"))))


if __name__ == "__main__":
    unittest.main()
