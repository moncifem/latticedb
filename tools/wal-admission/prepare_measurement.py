#!/usr/bin/env python3
"""Install reviewed host harnesses; old code never receives a new API shim."""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PREFIX = '''fn read_prefix(&self, p: &str, max: usize) -> Result<Vec<u8>, IoError> {
        let bytes = self.inner.read_prefix(p, max)?;
        self.counts.prefix_reads.set(self.counts.prefix_reads.get() + 1);
        self.counts.prefix_bytes.set(self.counts.prefix_bytes.get() + bytes.len());
        self.counts.max_request.set(self.counts.max_request.get().max(max));
        self.counts.max_return.set(self.counts.max_return.get().max(bytes.len()));
        self.counts.max_capacity.set(self.counts.max_capacity.get().max(bytes.capacity()));
        Ok(bytes)
    }'''
VISIT = '''{
        if record.lsn != last_lsn + 1 { return; }
        last_lsn = record.lsn;
        records += 1;
        bytes += record.payload.len();
    }'''


def install(root, modern):
    source = (ROOT / 'tools/wal-admission/measurement_reader.rs').read_text()
    source = source.replace('// PREFIX_METHOD', PREFIX if modern else '')
    call = ('Log::open_streaming_with_capabilities(io, "wal", 64 * 1024 * 1024, 1, |record, _| '
            if modern else 'Log::open_streaming(io, "wal", 64 * 1024 * 1024, |record| ')
    source = source.replace('// OPEN_LOG', 'let (log, report) = ' + call + VISIT + ')?;')
    target = root / 'crates/lattice-bench/examples/admission_measure.rs'
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(source)


if __name__ == '__main__':
    install(ROOT, True)
    install(ROOT / 'frozen', False)
