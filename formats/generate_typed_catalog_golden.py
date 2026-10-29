#!/usr/bin/env python3
"""Independent stdlib-only examples of LCAT v2 and LTYP v1; never runs Rust."""
from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parent / "corpus"
SOURCE = b"type Ticket {\n  points: int in 0..=9\n  title: text max 20\n}\n"
SCHEMA_VERSION = 7
TABLE_ID = 42

def u32(n):
    return struct.pack("<I", n)

def u64(n):
    return struct.pack("<Q", n)

def write(name, data, format_name, version):
    path = ROOT / (name + ".golden")
    path.write_bytes(data)
    path.with_suffix(".golden.toml").write_text(
        f'format = "{format_name}"\nversion = {version}\n'
        'generator = "formats/generate_typed_catalog_golden.py (Python stdlib)"\n'
        'schema_version = 7\ntable_id = 42\n'
        'description = "Independent byte-exact typed catalog/row fixture"\n'
    )

# LCAT v2: one schema operation, one type and its stable physical table mapping.
name = b"Ticket"
catalog = b"LCAT" + bytes([2]) + u32(1) + bytes([4]) + u64(SCHEMA_VERSION)
catalog += u32(len(SOURCE)) + SOURCE + u32(1) + u32(TABLE_ID) + u32(len(name)) + name
write("catalog-v2-ticket", catalog, "catalog", 2)

# LTYP envelope: schema 7, then typed cells v1, both columns present, points before title.
title = b"hello"
cells = bytes([1, 3]) + struct.pack("<q", 3) + u32(len(title)) + title + u32(0)
row = b"LTYP" + bytes([1]) + u64(SCHEMA_VERSION) + cells
write("typed-envelope-v1-ticket", row, "typed-envelope", 1)
