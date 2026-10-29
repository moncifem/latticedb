#!/usr/bin/env python3
"""Independent RFC 0008 vectors. No Rust codec is called."""
from pathlib import Path
import struct


def crc(data):
    value = 0xffffffff
    for byte in data:
        value ^= byte
        for _ in range(8):
            value = (value >> 1) ^ (0x82f63b78 if value & 1 else 0)
    return value ^ 0xffffffff


def u32(value):
    return struct.pack('<I', value)


def name(value):
    return u32(len(value)) + value


def header(mask=1, version=2):
    prefix = b'LATCAP02' + u32(version) + struct.pack('<HHQI', 32, 0, mask, 0)
    return prefix + u32(crc(prefix))


def record(lsn, kind, body):
    checked = u32(len(body)) + struct.pack('<QB', lsn, kind) + body
    return u32(len(body)) + u32(crc(checked)) + struct.pack('<QB', lsn, kind) + body


def vectors():
    tables = b'LCAT\x01' + u32(1) + b'\x01' + u32(7) + name(b'items')
    rows = b'LROW\x01' + u32(2) + u32(7) + name(b'a') + b'\x01' + name(b'x') + u32(7) + name(b'b') + b'\x00'
    compound = b'LCIX\x01' + name(tables) + u32(1) + b'\x01' + u32(9) + u32(7) + name(b'by_value') + b'\x00' + name(rows)
    # Explicit ChangeSet id 1, HLC (1,0,1), author 0, no turn, no parents.
    envelope = lambda payload: b'\x01' + (1).to_bytes(16, 'big') + struct.pack('<QII', 1, 0, 1) + bytes(16) + b'\x00\x00\x00' + name(payload)
    changeset = envelope(compound)
    legacy = b'LATLOG01' + struct.pack('<II', 1, 0)
    result = {
        'wal-admission-v2-header.golden': header(),
        'wal-admission-v2-compound.golden': header() + record(1, 1, changeset),
        'wal-admission-v2-unknown-capability.golden': header(mask=2),
        'wal-admission-v2-unknown-version.golden': header(version=3),
        'wal-admission-v1-legacy.golden': legacy,
        'wal-admission-v1-rows.golden': legacy + record(1, 1, envelope(rows)),
        'wal-admission-v2-dropped-capability.golden': header() + record(1, 1, changeset) + legacy,
    }
    for suffix, offset, value in [('wrong-length', 12, 16), ('reserved', 14, 1), ('bad-crc', 28, 0)]:
        data = bytearray(header())
        data[offset] = value if suffix != 'bad-crc' else data[offset] ^ 1
        if suffix != 'bad-crc':
            data[28:] = u32(crc(data[:28]))
        result[f'wal-admission-v2-{suffix}.golden'] = bytes(data)
    for length in range(32):
        result[f'wal-admission-v2-prefix-{length:02}.golden'] = header()[:length]
    return result


if __name__ == '__main__':
    root = Path(__file__).resolve().parents[2] / 'formats' / 'corpus'
    for filename, data in vectors().items():
        (root / filename).write_bytes(data)
        (root / (filename + '.toml')).write_text(
            'format = "wal-admission"\nversion = '+('1' if 'v1-' in filename else '2')+'\nstatus = "accepted"\n'
            'description = "Independent Python framing/CRC32C; RFC 0008 accepted; illustrative nested LCIX remains draft RFC 0005"\n')
