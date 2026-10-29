#!/usr/bin/env python3
"""Bounded validator tests; no Rust builds or benchmark execution."""
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

import foundation_comparison as study


class ComparisonTests(unittest.TestCase):
    def receipt(self, pair, arm, ratio=1):
        return {'pair': pair, 'arm': arm, 'exit_code': 0,
                'measurement': {'ns_per_op': {name: 10 * (ratio if arm == 'candidate' else 1)
                                             for name in study.WORKLOADS}}}

    def rows(self):
        return [self.receipt(p, a, 1 + p / 10) for p in range(9) for a in study.order(p)]

    def test_nine_pairs_alternate_and_report_ratios_without_criterion_verdict(self):
        self.assertEqual(study.order(0), ['baseline', 'candidate'])
        self.assertEqual(study.order(1), ['candidate', 'baseline'])
        result = study.summarize(self.rows())
        self.assertTrue(result['complete'])
        self.assertEqual(result['adr_c4_status'], 'not_evaluated')
        for row in result['workloads'].values():
            self.assertAlmostEqual(row['paired_candidate_over_baseline']['median'], 1.4)
            self.assertAlmostEqual(row['paired_candidate_over_baseline']['mad'], 0.2)
            self.assertEqual(row['baseline_ns']['sample_stdev'], 0)

    def test_missing_duplicate_nonzero_and_failed_samples_cannot_complete(self):
        for damage in ('missing', 'duplicate', 'exit', 'error'):
            rows = self.rows()
            if damage == 'missing': rows.pop()
            if damage == 'duplicate': rows[-1] = rows[0]
            if damage == 'exit': rows[0]['exit_code'] = 1
            if damage == 'error': rows[0]['error'] = 'interrupted'
            result = study.summarize(rows)
            self.assertFalse(result['complete'], damage)
            self.assertIsNone(result['workloads'])
            self.assertEqual(result['adr_c4_status'], 'not_evaluated')

    def test_bad_workload_set_and_nonpositive_nonfinite_values_refused(self):
        for damage in ('missing', 'extra', 'zero', 'nan', 'bool'):
            rows = self.rows()
            values = rows[0]['measurement']['ns_per_op']
            name = next(iter(values))
            if damage == 'missing': values.pop(name)
            elif damage == 'extra': values['sql'] = 1
            else: values[name] = {'zero': 0, 'nan': float('nan'), 'bool': True}[damage]
            with self.assertRaises(ValueError): study.summarize(rows)

    def ledger(self, root, value, crc_damage=False):
        payload = b'bench\n' + json.dumps(value, separators=(',', ':')).encode()
        cs = b'\1' + bytes(48) + b'\0' + bytes(2) + struct.pack('<I', len(payload)) + payload
        head = struct.pack('<I', len(cs)) + struct.pack('<QB', 1, 1)
        # Separate bit-at-a-time reference, pinned by the published CRC32C check vector below.
        checksum = 0xffffffff
        for byte in head + cs:
            for shift in range(8):
                bit = (checksum ^ (byte >> shift)) & 1
                checksum >>= 1
                if bit: checksum ^= 0x82f63b78
        checksum ^= 0xffffffff
        raw = b'LATLOG01' + struct.pack('<II', 1, 0) + head[:4] + struct.pack('<I', checksum) + head[4:] + cs
        if crc_damage: raw = raw[:-1] + bytes([raw[-1] ^ 1])
        path = root / 'ledger'
        path.mkdir()
        (path / '00000000000000000001.seg').write_bytes(raw)
        return path

    def value(self):
        return {'profile': 'ci-ubuntu', 'scale': 5, 'ns_per_op': {n: 12.5 for n in study.WORKLOADS}}

    def test_crc32c_published_check_vector(self):
        self.assertEqual(study.crc32c(b'123456789'), 0xe3069283)

    def test_legacy_and_current_ledger_accept_same_measurement_fields(self):
        for modern in (False, True):
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                value = self.value()
                if modern: value.update(verdict='passed', deltas={}, tolerance=2.0)
                self.ledger(root, value)
                self.assertEqual(study.read_measurement(root), value)

    def test_crc_drift_trailing_bytes_and_multiple_records_refused(self):
        for damage in ('crc', 'trailing', 'second_segment'):
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                path = self.ledger(root, self.value(), damage == 'crc')
                segment = next(path.glob('*.seg'))
                if damage == 'trailing': segment.write_bytes(segment.read_bytes() + b'\0')
                if damage == 'second_segment': (path / '00000000000000000002.seg').write_bytes(b'')
                with self.assertRaises(ValueError): study.read_measurement(root)

    def test_wrong_scale_and_incomplete_coverage_refused_from_actual_bytes(self):
        for damage in ('scale', 'workload'):
            with tempfile.TemporaryDirectory() as tmp:
                value = self.value()
                if damage == 'scale': value['scale'] = True
                else: value['ns_per_op'].pop(next(iter(value['ns_per_op'])))
                self.ledger(Path(tmp), value)
                with self.assertRaises(ValueError): study.read_measurement(tmp)

    def test_nonzero_sample_retains_raw_logs_and_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / 'fake-binary'
            binary.write_bytes(b'identity')
            def child(args, **kwargs):
                kwargs['stdout'].write(b'failed benchmark\n')
                raise OSError('injected failure')
            with patch.object(study.subprocess, 'run', side_effect=child):
                row = study.sample(binary, root, root / 'sample', 0, 'baseline')
            self.assertIsNone(row['exit_code'])
            self.assertIn('injected failure', row['error'])
            self.assertEqual((root / 'sample/stdout.log').read_text(), 'failed benchmark\n')
            self.assertTrue((root / 'sample/receipt.json').exists())


if __name__ == '__main__':
    unittest.main()
