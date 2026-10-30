"""Change feed codec behaviour beyond the shared vectors."""

from __future__ import annotations

import dataclasses
import struct
import unittest

from lattice_client import (
    CURSOR_LEN,
    MAX_KEY_LEN,
    MAX_VALUE_LEN,
    ORIGIN,
    ChangeEvent,
    Cursor,
    Delete,
    FeedDecodeError,
    FeedError,
    Hlc,
    TurnRef,
    Upsert,
    compare_cursors,
    cursor_from_bytes,
    cursor_to_bytes,
    decode_event,
    encode_event,
    from_hex,
    new_cursor,
    to_hex,
)


def hx(text: str) -> bytes:
    return from_hex("".join(c for c in text if c not in " |\n"))


SAMPLE = ChangeEvent(
    cursor=Cursor(258, 1),
    set_len=3,
    change_set=b"\xa0" * 16,
    hlc=Hlc(5, 6, 7),
    author=9,
    turn=None,
    kind=2,
    key=b"k",
    op=Upsert(b"vv"),
)

# Copied from the worked example in formats/change-feed-v1.md.
SAMPLE_BYTES = hx(
    "01 | 0000000000000102 00000001 | 03000000 | a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0 | "
    "0500000000000000 06000000 07000000 | 00000000000000000000000000000009 | 00 | 02000000 | "
    "0100 6b | 01 02000000 7676"
)


class FeedCodec(unittest.TestCase):
    def test_the_upsert_example_of_the_format_document(self) -> None:
        self.assertEqual(encode_event(SAMPLE), SAMPLE_BYTES)
        self.assertEqual(decode_event(SAMPLE_BYTES), SAMPLE)

    def test_a_cursor_is_12_bytes_big_endian_and_the_origin_is_all_zero(self) -> None:
        self.assertEqual(CURSOR_LEN, 12)
        self.assertEqual(to_hex(cursor_to_bytes(ORIGIN)), "00" * 12)
        self.assertEqual(cursor_from_bytes(bytes(12)), ORIGIN)

    def test_new_cursor_refuses_lsn_zero_and_out_of_range_parts(self) -> None:
        for args, kind in [
            ((0, 0), "zero_lsn"),
            ((1 << 64, 0), "invalid_value"),
            ((1, 2**32), "invalid_value"),
            ((-1, 0), "invalid_value"),
            ((True, 0), "invalid_value"),
        ]:
            with self.assertRaises(FeedError, msg=repr(args)) as cm:
                new_cursor(*args)
            self.assertEqual(cm.exception.kind, kind)
        self.assertEqual(new_cursor(1, 0), Cursor(1, 0))

    def test_cursors_order_by_lsn_then_index_with_no_precision_loss_above_2_53(self) -> None:
        a = Cursor((1 << 53) + 1, 0)
        b = Cursor((1 << 53) + 2, 0)
        self.assertEqual(compare_cursors(a, b), -1)
        self.assertEqual(compare_cursors(b, a), 1)
        self.assertEqual(compare_cursors(a, a), 0)
        self.assertEqual(compare_cursors(Cursor(7, 1), Cursor(7, 0)), 1)
        self.assertEqual(cursor_from_bytes(cursor_to_bytes(a)), a)

    def test_key_and_value_limits_are_exact(self) -> None:
        def with_key(n: int) -> ChangeEvent:
            return dataclasses.replace(SAMPLE, key=bytes(n))

        self.assertEqual(len(decode_event(encode_event(with_key(MAX_KEY_LEN))).key), MAX_KEY_LEN)
        with self.assertRaises(FeedError) as cm:
            encode_event(with_key(MAX_KEY_LEN + 1))
        self.assertEqual(cm.exception.kind, "key_too_large")

        def with_value(n: int) -> ChangeEvent:
            return dataclasses.replace(SAMPLE, op=Upsert(bytes(n)))

        back = decode_event(encode_event(with_value(MAX_VALUE_LEN)))
        assert isinstance(back.op, Upsert)
        self.assertEqual(len(back.op.value), MAX_VALUE_LEN)
        with self.assertRaises(FeedError) as cm2:
            encode_event(with_value(MAX_VALUE_LEN + 1))
        self.assertEqual(cm2.exception.kind, "value_too_large")

    def test_a_huge_declared_value_length_is_refused_before_anything_is_copied(self) -> None:
        buf = bytearray(SAMPLE_BYTES)
        buf[74:78] = struct.pack("<I", 0xFFFF_FFFF)
        with self.assertRaises(FeedDecodeError) as cm:
            decode_event(bytes(buf))
        self.assertEqual((cm.exception.kind, cm.exception.detail), ("invalid", "value length"))

    def test_values_above_2_53_and_at_the_top_of_their_range_are_exact(self) -> None:
        e = dataclasses.replace(
            SAMPLE,
            cursor=Cursor((1 << 63) + 12345, 4_000_000_000),
            set_len=4_294_967_295,
            hlc=Hlc((1 << 64) - 1, 4_294_967_295, 1),
            author=(1 << 128) - 3,
            turn=TurnRef((1 << 60) + 1, (1 << 64) - 1),
        )
        self.assertEqual(decode_event(encode_event(e)), e)

    def test_values_outside_their_field_are_refused_instead_of_wrapping(self) -> None:
        bad = [
            dataclasses.replace(SAMPLE, author=1 << 128),
            dataclasses.replace(SAMPLE, author=-1),
            dataclasses.replace(SAMPLE, kind=-1),
            dataclasses.replace(SAMPLE, kind=2**32),
            dataclasses.replace(SAMPLE, kind=True),
            dataclasses.replace(SAMPLE, hlc=Hlc(1 << 64, 0, 0)),
            dataclasses.replace(SAMPLE, hlc=Hlc(0, 1.5, 0)),  # type: ignore[arg-type]
            dataclasses.replace(SAMPLE, change_set=bytes(15)),
            dataclasses.replace(SAMPLE, turn=TurnRef(-1, 0)),
            dataclasses.replace(SAMPLE, turn=TurnRef(0, 1 << 64)),
            dataclasses.replace(SAMPLE, set_len=2**32),
        ]
        for e in bad:
            with self.assertRaises(FeedError, msg=repr(e)) as cm:
                encode_event(e)
            self.assertEqual(cm.exception.kind, "invalid_value", repr(e))

    def test_a_decoded_event_does_not_alias_the_input(self) -> None:
        buf = bytearray(SAMPLE_BYTES)
        ev = decode_event(buf)
        buf[:] = bytes(len(buf))
        self.assertEqual(ev, SAMPLE)

    def test_decoding_accepts_a_memoryview_into_a_larger_buffer(self) -> None:
        pool = bytearray(b"\xee" * 4) + bytearray(SAMPLE_BYTES) + bytearray(b"\xee" * 5)
        view = memoryview(pool)[4 : 4 + len(SAMPLE_BYTES)]
        self.assertEqual(decode_event(view), SAMPLE)

    def test_a_delete_with_a_turn_round_trips(self) -> None:
        e = dataclasses.replace(SAMPLE, op=Delete(), turn=TurnRef(3, 4))
        self.assertEqual(decode_event(encode_event(e)), e)


if __name__ == "__main__":
    unittest.main()
