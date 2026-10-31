//! Seeded schedules for the kill and reconnect scenarios. Pure arithmetic: no clock, no ambient
//! randomness, so a failing seed can be replayed exactly.

/// A reproducible connection-kill schedule: `count` strictly increasing counts (of frames or events,
/// as the test defines), each one between 1 and 64 after the previous point. The same seed gives the
/// same schedule on every machine. The harness injects no kills itself: a test applies a point by
/// dropping its client socket after that many events (`tests/reconnect_kill.rs`).
pub fn kill_points(seed: u64, count: usize) -> Vec<u64> {
    let mut state = seed;
    let mut at = 0u64;
    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        let (next, draw) = splitmix64(state);
        state = next;
        at = at.saturating_add(1 + draw % 64);
        points.push(at);
    }
    points
}

/// One SplitMix64 step: the advanced state and the output drawn from it.
fn splitmix64(state: u64) -> (u64, u64) {
    let state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (state, z ^ (z >> 31))
}
