#![no_main]
use lattice_sdk::contract::{Receipt, Status};
use lattice_sdk::outbox::{Outbox, OutboxConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let cfg = OutboxConfig::default();
    // Stored bytes that decode must be the one canonical encoding of the queue they describe:
    // re-encoding gives the same bytes, and a second restore gives the same queue.
    if let Ok(mut o) = Outbox::from_bytes(data, cfg) {
        assert_eq!(o.to_bytes(), data);
        let again = Outbox::from_bytes(&o.to_bytes(), cfg).expect("a canonical queue restores");
        assert_eq!(again.pending(), o.pending());
        // A restored queue is sendable, and receipts for what it sent clear it.
        let mut now = 0;
        while let Some(batch) = o.poll(now, 7) {
            assert!(!batch.records.is_empty());
            now += 1;
            let receipts: Vec<Receipt> = batch
                .records
                .iter()
                .map(|r| Receipt {
                    rid: r.rid.clone(),
                    status: Status::Recorded,
                    pos: Some(0),
                })
                .collect();
            o.on_receipts(&receipts);
        }
        assert!(o.is_empty());
    }
});
