//! Replicated state converges when replicas exchange it over a hostile network: messages are
//! dropped, duplicated, reordered and partitioned. After the network heals and a few gossip rounds
//! run, every replica must hold identical state (invariant I5). A deliberately broken merge proves
//! the harness can fail.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_range_loop
)]

use lattice_abi::Hlc;
use lattice_core::HlcClock;
use lattice_core::crdt::{Crdt, LwwRegister, OrSet, Rga};
use lattice_seams::{Net, NodeId};
use lattice_sim::{SeededRng, SimNet};
use std::collections::BTreeMap;

#[derive(Clone, PartialEq, Debug, Default)]
struct State {
    set: OrSet<u8>,
    reg: LwwRegister<u8>,
    text: Rga,
}

impl Crdt for State {
    fn merge(&mut self, other: &Self) {
        self.set.merge(&other.set);
        self.reg.merge(&other.reg);
        self.text.merge(&other.text);
    }
}

/// A merge that just takes the other side's state: not commutative, so replicas must diverge.
#[derive(Clone, PartialEq, Debug, Default)]
struct Overwrite(State);

trait Replicated: Clone + PartialEq + Default {
    fn merge_in(&mut self, other: &Self);
    fn state_mut(&mut self) -> &mut State;
}

impl Replicated for State {
    fn merge_in(&mut self, other: &Self) {
        self.merge(other);
    }
    fn state_mut(&mut self) -> &mut State {
        self
    }
}

impl Replicated for Overwrite {
    fn merge_in(&mut self, other: &Self) {
        self.0 = other.0.clone();
    }
    fn state_mut(&mut self) -> &mut State {
        &mut self.0
    }
}

fn local_op<R: Replicated>(r: &mut R, clock: &mut HlcClock, rng: &mut SeededRng, now: u64) {
    let tag: Hlc = clock.tick(now).unwrap();
    let s = r.state_mut();
    match rng.below(5) {
        0 => s.set.add(rng.below(6) as u8, tag),
        1 => s.set.remove(&(rng.below(6) as u8)),
        2 => s.reg.set(tag, rng.below(200) as u8),
        3 => {
            let vis = s.text.chars();
            let after = if vis.is_empty() {
                None
            } else {
                vis.get(rng.below(vis.len() as u64) as usize)
                    .map(|(id, _)| *id)
            };
            s.text
                .insert(after, (b'a' + rng.below(5) as u8) as char, tag);
        }
        _ => {
            let vis = s.text.chars();
            if let Some((id, _)) = vis.get(rng.below(vis.len().max(1) as u64) as usize) {
                s.text.delete(*id);
            }
        }
    }
}

/// Runs a cluster. Returns the replicas after healing and gossip, or `Err` with the divergent pair.
fn run_cluster<R: Replicated>(seed: u64, replicas: usize, rounds: usize) -> Result<Vec<R>, String> {
    let mut rng = SeededRng::new(seed);
    let mut net = SimNet::new(seed ^ 1, 200_000, 100_000, 200_000);
    let mut states: Vec<R> = (0..replicas).map(|_| R::default()).collect();
    let mut clocks: Vec<HlcClock> = (0..replicas).map(|i| HlcClock::new(i as u32 + 1)).collect();
    let mut snapshots: BTreeMap<u64, R> = BTreeMap::new();
    let mut next_key = 0u64;
    let send = |net: &mut SimNet,
                snapshots: &mut BTreeMap<u64, R>,
                next_key: &mut u64,
                from: usize,
                to: usize,
                state: &R| {
        *next_key += 1;
        snapshots.insert(*next_key, state.clone());
        net.send(
            from as NodeId,
            to as NodeId,
            next_key.to_le_bytes().to_vec(),
        );
    };
    for round in 0..rounds {
        let now = 1_000 + round as u64 * 10;
        if rng.below(4) == 0 {
            let (a, b) = (
                rng.below(replicas as u64) as NodeId,
                rng.below(replicas as u64) as NodeId,
            );
            net.block(a, b);
        }
        if rng.below(6) == 0 {
            net.heal();
        }
        for i in 0..replicas {
            for _ in 0..=rng.below(3) {
                local_op(&mut states[i], &mut clocks[i], &mut rng, now);
            }
            let peer = (i + 1 + rng.below(replicas as u64 - 1) as usize) % replicas;
            let snap = states[i].clone();
            send(&mut net, &mut snapshots, &mut next_key, i, peer, &snap);
        }
        for i in 0..replicas {
            while let Some((_, bytes)) = net.recv(i as NodeId) {
                let key = u64::from_le_bytes(bytes.as_slice().try_into().unwrap());
                if let Some(s) = snapshots.get(&key) {
                    states[i].merge_in(&s.clone());
                }
            }
        }
    }
    // Heal the network and gossip with no faults until the replicas settle.
    net.heal();
    let mut clean = SimNet::new(seed ^ 2, 0, 0, 0);
    for _ in 0..replicas {
        for i in 0..replicas {
            for j in 0..replicas {
                if i != j {
                    send(
                        &mut clean,
                        &mut snapshots,
                        &mut next_key,
                        i,
                        j,
                        &states[i].clone(),
                    );
                }
            }
        }
        for i in 0..replicas {
            while let Some((_, bytes)) = clean.recv(i as NodeId) {
                let key = u64::from_le_bytes(bytes.as_slice().try_into().unwrap());
                if let Some(s) = snapshots.get(&key) {
                    states[i].merge_in(&s.clone());
                }
            }
        }
    }
    for (i, replica) in states.iter().enumerate().skip(1) {
        if replica != &states[0] {
            return Err(format!("replica {i} differs from replica 0 (seed {seed})"));
        }
    }
    Ok(states)
}

#[test]
fn inv_i5_replicas_converge_after_drops_duplicates_reordering_and_partitions() {
    let mut nonempty = 0;
    for seed in 0..300 {
        let states = run_cluster::<State>(seed, 4, 40).unwrap_or_else(|e| panic!("{e}"));
        nonempty +=
            usize::from(!states[0].text.text().is_empty() && !states[0].set.elements().is_empty());
    }
    assert!(
        nonempty > 200,
        "the workload should actually build up state, only {nonempty} runs did"
    );
}

#[test]
fn a_non_commutative_merge_is_caught_as_divergence() {
    let diverged = (0..100)
        .filter(|seed| run_cluster::<Overwrite>(*seed, 4, 40).is_err())
        .count();
    assert!(
        diverged > 50,
        "a broken merge should fail to converge in most runs, only {diverged} did"
    );
}
