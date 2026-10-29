fn main() {
    let store = lattice_engines::store::RowStore::open(
        lattice_sim::SimIo::new(1, lattice_sim::FaultProfile::clean()),
        lattice_sim::SimClock::new(1),
        lattice_sim::SeededRng::new(2),
        &lattice_core_config(),
    )
    .unwrap();
    std::hint::black_box(store.snapshot());
}
// The bridge-only baseline intentionally contains no SQL parser, executor or report serialization.
fn lattice_core_config() -> lattice_core::Config {
    lattice_core::Config {
        dir: "wal".into(),
        segment_limit: 262144,
        node: 1,
    }
}
