#!/usr/bin/env python3
"""Small independent structural checks; no benchmark workload runs locally."""
from pathlib import Path
import struct
import tempfile
import unittest

from measure import dataset
from prepare_measurement import install


class DatasetTests(unittest.TestCase):
    def test_literal_field_offsets_lengths_and_increasing_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dataset(root, 3, 64, False)
            for lsn in [1, 2, 3]:
                data = (root / 'wal' / f'{lsn:020}.seg').read_bytes()
                self.assertEqual(data[:16], b'LATLOG01\x01\x00\x00\x00\x00\x00\x00\x00')
                self.assertEqual(len(data), 178)
                self.assertEqual(struct.unpack('<I', data[16:20])[0], 145)
                self.assertEqual(struct.unpack('<Q', data[24:32])[0], lsn)
                self.assertEqual(data[32], 1)
                cs = data[33:]
                self.assertEqual(int.from_bytes(cs[1:17], 'big'), lsn)
                self.assertEqual(struct.unpack('<QII', cs[17:33]), (lsn, 0, 1))
                self.assertEqual(cs[56:69], b'LROW\x01\x01\x00\x00\x00\x07\x00\x00\x00')
                self.assertEqual(cs[-64:], b'x' * 64)

    def test_guarded_body_identical_to_legacy(self):
        with tempfile.TemporaryDirectory() as tmp:
            legacy, guarded = Path(tmp) / 'legacy', Path(tmp) / 'guarded'
            dataset(legacy, 2, 7, False)
            dataset(guarded, 2, 7, True)
            for lsn in [1, 2]:
                filename = f'{lsn:020}.seg'
                a = (legacy / 'wal' / filename).read_bytes()
                b = (guarded / 'wal' / filename).read_bytes()
                self.assertEqual(a[16:], b[32:])
                self.assertEqual(b[:8], b'LATCAP02')

    def test_frozen_harness_uses_only_actual_old_api(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            install(root, False)
            source = (root / 'crates/lattice-bench/examples/admission_measure.rs').read_text()
            self.assertNotIn('read_prefix(', source)
            self.assertNotIn('open_streaming_with_capabilities', source)
            self.assertIn('Log::open_streaming(', source)
            self.assertNotIn('// OPEN_LOG', source)
            install(root, True)
            source = (root / 'crates/lattice-bench/examples/admission_measure.rs').read_text()
            self.assertIn('self.inner.read_prefix(p, max)?', source)
            self.assertIn('Log::open_streaming_with_capabilities', source)


if __name__ == '__main__':
    unittest.main()
