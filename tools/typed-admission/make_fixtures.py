#!/usr/bin/env python3
"""Independent proposed typed-capability framing; no Rust encoder or executor is called."""
from pathlib import Path
import importlib.util
import struct

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('independent_wal_vectors', ROOT / 'tools/wal-admission/make_fixtures.py')
wal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wal)


def vectors():
    catalog = (ROOT / 'formats/corpus/catalog-v2-ticket.golden').read_bytes()
    typed = (ROOT / 'formats/corpus/typed-envelope-v1-ticket.golden').read_bytes()
    block = lambda b: struct.pack('<I', len(b)) + b
    envelope = lambda number, payload: (b'\x01' + number.to_bytes(16, 'big')
        + struct.pack('<QII', number, 0, 1) + bytes(16) + b'\x00\x00\x00' + block(payload))
    rows = b'LROW\x01' + struct.pack('<II', 1, 42) + block(b'key') + b'\x01' + block(typed)
    compound = b'LCIX\x02' + block(catalog) + struct.pack('<I', 0) + block(b'')
    return {
        'typed-admission-cap4-header.golden': wal.header(mask=4),
        'typed-admission-cap5-header.golden': wal.header(mask=5),
        'typed-admission-cap4-schema-rows.golden': wal.header(mask=4)
            + wal.record(1, 1, envelope(1, catalog)) + wal.record(2, 1, envelope(2, rows)),
        'typed-admission-cap5-schema-rows.golden': wal.header(mask=5)
            + wal.record(1, 1, envelope(1, compound)) + wal.record(2, 1, envelope(2, rows)),
    }


if __name__ == '__main__':
    for filename, body in vectors().items():
        path = ROOT / 'formats/corpus' / filename
        path.write_bytes(body)
        path.with_suffix('.golden.toml').write_text(
            'format = "typed-admission"\nversion = 2\nstatus = "proposed"\n'
            'description = "Independent Python typed capability 4/5; RFC 0001 and ADR 0015 remain proposed"\n')
