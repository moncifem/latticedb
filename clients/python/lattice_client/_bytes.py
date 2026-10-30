"""Byte helpers shared by the codecs: range checks, a bounded reader and a writer.

Python integers have no width, so every field is range checked explicitly. ``bool`` is an ``int``
in Python and is never accepted as a number here.
"""

from __future__ import annotations

import struct

U16_MAX = 0xFFFF
U32_MAX = 0xFFFF_FFFF
U64_MAX = 0xFFFF_FFFF_FFFF_FFFF
U128_MAX = (1 << 128) - 1


def is_uint(n: object, maximum: int) -> bool:
    """True for an ``int`` (not a ``bool``) ``n`` with ``0 <= n <= maximum``."""
    return isinstance(n, int) and not isinstance(n, bool) and 0 <= n <= maximum


def is_bytes_like(b: object) -> bool:
    """True for ``bytes`` and ``bytearray``, the only byte containers the codecs accept."""
    return isinstance(b, (bytes, bytearray))


def to_hex(data: bytes | bytearray) -> str:
    """Lowercase hexadecimal, two digits per byte."""
    return bytes(data).hex()


def from_hex(text: str) -> bytes:
    """Parses :func:`to_hex` output. Raises ``ValueError`` on odd length or a non-hex digit."""
    if len(text) % 2 != 0:
        raise ValueError("hex text has an odd length")
    if any(c not in "0123456789abcdefABCDEF" for c in text):
        raise ValueError("hex text holds a non-hex digit")
    return bytes.fromhex(text)


def compare_bytes(a: bytes, b: bytes) -> int:
    """Unsigned lexicographic comparison, shorter prefix first. Returns -1, 0 or 1."""
    if a == b:
        return 0
    return -1 if a < b else 1


class Writer:
    """Growable output buffer."""

    def __init__(self) -> None:
        self._buf = bytearray()

    def __len__(self) -> int:
        return len(self._buf)

    def u8(self, n: int) -> None:
        self._buf += struct.pack(">B", n)

    def u16be(self, n: int) -> None:
        self._buf += struct.pack(">H", n)

    def u16le(self, n: int) -> None:
        self._buf += struct.pack("<H", n)

    def u32be(self, n: int) -> None:
        self._buf += struct.pack(">I", n)

    def u32le(self, n: int) -> None:
        self._buf += struct.pack("<I", n)

    def u64be(self, n: int) -> None:
        self._buf += struct.pack(">Q", n)

    def u64le(self, n: int) -> None:
        self._buf += struct.pack("<Q", n)

    def u128be(self, n: int) -> None:
        self.u64be(n >> 64)
        self.u64be(n & U64_MAX)

    def raw(self, b: bytes | bytearray) -> None:
        self._buf += b

    def finish(self) -> bytes:
        return bytes(self._buf)


class Reader:
    """Bounds-checked input cursor. Every read returns ``None`` instead of passing the end."""

    def __init__(self, buf: bytes | bytearray | memoryview) -> None:
        self._buf = bytes(buf)
        self._pos = 0

    @property
    def remaining(self) -> int:
        return len(self._buf) - self._pos

    def take(self, n: int) -> bytes | None:
        if n > self.remaining:
            return None
        out = self._buf[self._pos : self._pos + n]
        self._pos += n
        return out

    def _unpack(self, fmt: str, size: int) -> int | None:
        raw = self.take(size)
        if raw is None:
            return None
        (value,) = struct.unpack(fmt, raw)
        return int(value)

    def u8(self) -> int | None:
        return self._unpack(">B", 1)

    def u16be(self) -> int | None:
        return self._unpack(">H", 2)

    def u16le(self) -> int | None:
        return self._unpack("<H", 2)

    def u32be(self) -> int | None:
        return self._unpack(">I", 4)

    def u32le(self) -> int | None:
        return self._unpack("<I", 4)

    def u64be(self) -> int | None:
        return self._unpack(">Q", 8)

    def u64le(self) -> int | None:
        return self._unpack("<Q", 8)

    def u128be(self) -> int | None:
        hi = self.u64be()
        lo = self.u64be()
        if hi is None or lo is None:
            return None
        return (hi << 64) | lo
