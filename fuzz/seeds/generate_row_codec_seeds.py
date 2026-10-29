#!/usr/bin/env python3
"""Small codec seeds encoded independently with Python struct, not the Rust codecs.

Run from any directory; --check verifies tracked bytes without writing them. Typed-row seeds carry
one layout-selector byte consumed by typed_row_decode.rs before the actual versioned row payload.
"""
import argparse
from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parent

def u32(n):
    return struct.pack('<I', n)

def sized(value):
    return u32(len(value)) + value

def seeds():
    rows = b'LROW\x01' + u32(2) + u32(7) + sized(b'k') + b'\x01' + sized(b'value')
    rows += u32(7) + sized(b'deleted') + b'\x00'
    catalog = b'LCAT\x01' + u32(3)
    catalog += b'\x01' + u32(7) + sized(b'Issue')
    catalog += b'\x02' + u32(7) + sized(b'Ticket')
    catalog += b'\x03' + u32(7)
    # Optional cells: bool, int, text, bytes, ref, enum and list<bool> in layout order.
    rich = b'\x01\x7f\x01' + struct.pack('<q', -7) + sized(b'hello')
    rich += sized(b'\x00\xff') + sized(b'issue:1') + struct.pack('<H', 1)
    rich += u32(2) + b'\x00\x01' + u32(0)
    # The empty layout stores only tagged overflow; name 'extra', nested object 'flag'=true.
    overflow = b'\x01' + u32(1) + struct.pack('<H', 5) + b'extra'
    overflow += b'\x07' + u32(1) + struct.pack('<H', 4) + b'flag' + b'\x00\x01'
    return {
        'row_ops/put-delete': rows,
        'row_ops/empty': b'LROW\x01' + u32(0),
        'row_ops/truncated-count': b'LROW\x01' + u32(0xffffffff),
        'catalog_decode/create-rename-drop': catalog,
        'catalog_decode/unknown-version': b'LCAT\xff' + u32(0),
        'catalog_decode/truncated-name': b'LCAT\x01' + u32(1) + b'\x01' + u32(7) + u32(0xffffffff),
        'typed_row_decode/all-cell-types': b'\x01' + rich,
        'typed_row_decode/absent-optional': b'\x01\x01\x00' + u32(0),
        'typed_row_decode/required-bool': b'\x02\x01\x01\x01' + u32(0),
        'typed_row_decode/nested-overflow': b'\x00' + overflow,
        'typed_row_decode/empty-list': b'\x03\x01\x01' + u32(0) + u32(0),
        'typed_row_decode/depth-boundary': b'\x12\x01\x01' + (u32(1) * 16) + b'\x01' + u32(0),
        'typed_row_decode/depth-exceeded': b'\x13\x01\x01' + (u32(1) * 17) + b'\x01' + u32(0),
    }

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    expected = seeds()
    for name, value in expected.items():
        path = ROOT / name
        if args.check:
            if not path.is_file() or path.read_bytes() != value:
                raise SystemExit(f'seed differs: {name}')
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(value)
    print(f'{len(expected)} independent row codec seeds ' + ('verified' if args.check else 'written'))

if __name__ == '__main__':
    main()
