//! Phase 3 hot-path microbenchmarks (WP53, issue #255, first slice). Everything is in memory with a
//! fixed seed and fixed sizes; preparation and semantic checks are untimed.
//!
//! Cases: tree CRDT apply (in order, late, and late before and after compaction), the compacting
//! text CRDT (apply and compact) and the sequence CRDT, register and set merge, change feed cursor
//! and event codec, sync frame codec, fan-out through the real backpressure `Hub` to 1, 100 and
//! 10,000 subscribers with stalled ones, and a reconnect storm against `SubscriptionHost`.
//!
//! Metadata schema 2 (the follow-up slice): `fanout_offer_*` now measures the real `Hub` instead
//! of a local queue, and `lww_register_merge` now measures merges that change the register (the
//! old behaviour is kept as `lww_register_merge_noop`). A schema 1 baseline is refused.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use lattice_abi::Hlc;
use lattice_api::backpressure::{Hub, Limits, State};
use lattice_api::subscribe::{EventSource, HostConfig, Output, SourceError, SubscriptionHost};
use lattice_api::sync::{self, Event, Message};
use lattice_bench::{compare, parse_results, render_results};
use lattice_core::changeset::{ChangeSetId, TurnRef};
use lattice_core::crdt::{Crdt, LwwRegister, OrSet, Rga};
use lattice_core::crdt_text as text;
use lattice_core::crdt_tree::{NodeId, Op, Outcome, Tree};
use lattice_core::feed::{ChangeEvent, ChangeOp, Cursor, EntityChange};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

type Results = BTreeMap<String, f64>;
const SEED: u64 = 0x5EED_0003_0000_0255;
const FANOUT_QUEUE_CAP: usize = 16;
const FANOUT_DRAIN_EVERY: usize = 8;
const FANOUT_SLOW_EVERY: usize = 10;
const FANOUT_MAX_BYTES: usize = 1 << 20;
/// Visible characters of the text workload per unit of scale (scale 8 is the 10,000 characters of
/// the ADR 0010 measurement).
const TEXT_CHARS_PER_SCALE: u64 = 1250;
/// Reconnecting clients per unit of scale, and the length of the event log they resume from.
const STORM_CLIENTS_PER_SCALE: u64 = 32;
const STORM_LOG_EVENTS: u64 = 512;
const STORM_SCOPE: &str = "ws/bench";

#[derive(Clone, Debug)]
struct Options {
    profile: String,
    scale: u64,
    samples: usize,
    tolerance: f64,
    write: bool,
}
impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut out = Self {
            profile: "local".into(),
            scale: 8,
            samples: 5,
            tolerance: 0.25,
            write: false,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--write-baseline" {
                out.write = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {arg}"))?;
            match arg.as_str() {
                "--profile" => out.profile = value,
                "--scale" => out.scale = value.parse().map_err(|_| "invalid scale")?,
                "--samples" => out.samples = value.parse().map_err(|_| "invalid samples")?,
                "--tolerance" => out.tolerance = value.parse().map_err(|_| "invalid tolerance")?,
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        if out.profile.is_empty()
            || out.profile.len() > 64
            || !out
                .profile
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(
                "profile must contain 1..=64 ASCII letters, digits, dashes or underscores".into(),
            );
        }
        if !(1..=1024).contains(&out.scale) || !(1..=31).contains(&out.samples) {
            return Err("require scale 1..=1024, samples 1..=31".into());
        }
        if !out.tolerance.is_finite() || out.tolerance < 0.0 {
            return Err("tolerance must be finite and nonnegative".into());
        }
        Ok(out)
    }
    /// Workload sizes derive from `scale`; they are part of the metadata so a different workload
    /// is refused instead of compared.
    fn tree_ops(&self) -> u64 {
        256 * self.scale
    }
    fn codec_ops(&self) -> u64 {
        1024 * self.scale
    }
    fn fanout_events(&self) -> u64 {
        (8 * self.scale).max(32)
    }
    fn text_chars(&self) -> u64 {
        TEXT_CHARS_PER_SCALE * self.scale
    }
    fn storm_clients(&self) -> u64 {
        STORM_CLIENTS_PER_SCALE * self.scale
    }
    fn metadata(&self) -> Value {
        json!({"schema":2,"benchmark":"phase3","profile":self.profile,"seed":SEED,
               "scale":self.scale,"samples":self.samples,"tree_ops":self.tree_ops(),
               "tree_late_ops":(self.tree_ops() / 16).max(1),"codec_ops":self.codec_ops(),
               "fanout_events":self.fanout_events(),"fanout_subscribers":[1,100,10000],
               "fanout_queue_capacity":FANOUT_QUEUE_CAP,"fanout_drain_every":FANOUT_DRAIN_EVERY,
               "fanout_slow_every":FANOUT_SLOW_EVERY,
               "fanout_max_bytes":FANOUT_MAX_BYTES,"fanout_backend":"lattice_api::backpressure::Hub",
               "tree_compact_at":"half of the in-order log",
               "text_visible_chars":self.text_chars(),"text_delete_percent":40,
               "storm_clients":self.storm_clients(),"storm_log_events":STORM_LOG_EVENTS,
               "cpu_model":cpu_model(),
               "architecture":std::env::consts::ARCH,"os":std::env::consts::OS,
               "debug_assertions":cfg!(debug_assertions),"backend":"in memory"})
    }
}

/// Best-effort CPU model through std only: `sysctl` on macOS, `/proc/cpuinfo` on Linux.
/// `"unknown"` when neither works. It is reported in the metadata but not compared with a
/// baseline (hosted runners differ between runs).
fn cpu_model() -> String {
    let found = if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("model name") || l.starts_with("Model"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.to_owned())
        })
    } else {
        None
    };
    let name: String = found
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    if name.is_empty() {
        "unknown".into()
    } else {
        name
    }
}

/// Deterministic splitmix64 stream.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn hlc(wall_ms: u64, node: u32) -> Hlc {
    Hlc {
        wall_ms,
        counter: 0,
        node,
    }
}

fn nanos(start: Instant) -> f64 {
    start.elapsed().as_nanos() as f64
}

/// Raw samples per case: `(total ns, operations)` per sample.
type Samples = BTreeMap<String, Vec<(f64, u64)>>;

fn add(samples: &mut Samples, name: &str, ns: f64, ops: u64) {
    samples.entry(name.into()).or_default().push((ns, ops));
}

/// A tree op stream: node `i` moves under a random earlier node (or the root).
fn tree_ops(n: u64, seed: u64, wall_offset: u64, step: u64, node_base: u64) -> Vec<Op> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|i| {
            let node = node_base + i;
            let parent = if i == 0 || rng.below(8) == 0 {
                NodeId::ROOT
            } else {
                NodeId(node_base + rng.below(i))
            };
            Op::new(
                hlc(wall_offset + i * step, 1),
                NodeId(2 + node),
                if parent == NodeId::ROOT {
                    parent
                } else {
                    NodeId(2 + parent.0)
                },
                rng.below(1000),
            )
        })
        .collect()
}

fn apply_all(tree: &mut Tree, ops: &[Op]) -> Result<(), String> {
    for op in ops {
        match tree.apply(*op) {
            Ok(Outcome::Applied | Outcome::Skipped) => {}
            other => return Err(format!("unexpected tree apply result {other:?}")),
        }
    }
    Ok(())
}

fn bench_tree(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let n = o.tree_ops();
    // Even times are in order ops; late ops are interleaved at odd times, so each one lands in the
    // middle of the log and forces undo and redo of the ops after it.
    let base = tree_ops(n, SEED, 1_000, 2, 0);
    let late_n = (n / 16).max(1);
    let late = tree_ops(late_n, SEED ^ 1, 1_001, 2 * (n / late_n).max(1), 1_000_000);
    // Compaction cases. The stable frontier is the middle of the in-order log. The late ops land at
    // odd times in the upper half, so they are above the frontier and valid after compaction; the
    // same ops are applied to an uncompacted and a compacted copy (as in PR 313's measurement) and
    // the two trees must agree.
    let stable = base.get(base.len() / 2 - 1).ok_or("tree log too short")?.id;
    let dropped_expected = base.len() / 2;
    let late_hi = tree_ops(
        late_n,
        SEED ^ 6,
        1_000 + n + 1,
        2 * ((n / 2) / late_n).max(1),
        2_000_000,
    );
    let mut built = Tree::new();
    apply_all(&mut built, &base)?;
    for _ in 0..o.samples {
        let mut uncompacted = built.clone();
        let start = Instant::now();
        apply_all(&mut uncompacted, black_box(&late_hi))?;
        add(
            samples,
            "tree_apply_late_before_compact",
            nanos(start),
            late_n,
        );
        let mut compacted = built.clone();
        let start = Instant::now();
        let dropped = black_box(&mut compacted)
            .compact(black_box(stable))
            .map_err(|e| e.to_string())?;
        add(samples, "tree_compact", nanos(start), dropped as u64);
        let start = Instant::now();
        apply_all(&mut compacted, black_box(&late_hi))?;
        add(
            samples,
            "tree_apply_late_after_compact",
            nanos(start),
            late_n,
        );
        if dropped != dropped_expected
            || compacted.op_count() != uncompacted.op_count() - dropped
            || compacted.below_frontier_arrivals() != 0
            || compacted.snapshot() != uncompacted.snapshot()
            || !compacted.is_acyclic()
        {
            return Err("tree compaction changed the visible tree or dropped the wrong ops".into());
        }
    }
    for _ in 0..o.samples {
        let mut tree = Tree::new();
        let start = Instant::now();
        apply_all(&mut tree, black_box(&base))?;
        add(samples, "tree_apply_in_order", nanos(start), n);
        if !tree.is_acyclic() || tree.op_count() as u64 != n {
            return Err("tree in-order apply broke an invariant".into());
        }
        let mut tree = tree.clone();
        let start = Instant::now();
        apply_all(&mut tree, black_box(&late))?;
        add(samples, "tree_apply_late", nanos(start), late_n);
        if !tree.is_acyclic() || tree.op_count() as u64 != n + late_n {
            return Err("tree late apply broke an invariant".into());
        }
    }
    Ok(())
}

/// The ADR 0010 workload with a fixed seed: random single-character inserts and deletes (about 40
/// percent deletes) until `visible` characters are on screen. Returns the operation log.
fn text_workload(visible: u64) -> Result<Vec<text::Op>, String> {
    let mut rng = Rng(SEED ^ 7);
    let mut doc = text::Doc::new();
    let mut ops = Vec::new();
    let mut clock = 0u64;
    while (doc.len() as u64) < visible {
        clock += 1;
        let id = hlc(clock, 1);
        let len = doc.len() as u64;
        let op = if len > 0 && rng.below(10) < 4 {
            let target = doc
                .visible_id(rng.below(len) as usize)
                .ok_or("text position out of range")?;
            text::Op::Delete { id, target }
        } else {
            let at = rng.below(len + 1) as usize;
            let after = match at.checked_sub(1) {
                None => None,
                Some(i) => Some(doc.visible_id(i).ok_or("text position out of range")?),
            };
            text::Op::Insert {
                id,
                after,
                text: char::from(b'a' + rng.below(26) as u8).to_string(),
            }
        };
        doc.apply(op.clone()).map_err(|e| e.to_string())?;
        ops.push(op);
    }
    Ok(ops)
}

fn bench_text_crdt(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let ops = text_workload(o.text_chars())?;
    let n_ops = ops.len();
    let last = ops.last().ok_or("empty text workload")?.id();
    let mut reference = text::Doc::new();
    for op in &ops {
        reference.apply(op.clone()).map_err(|e| e.to_string())?;
    }
    let expected = reference.text();
    // Every operation here is issued by node 1, so one ack covers the whole history.
    let frontier = text::Frontier {
        stable: last,
        acks: BTreeMap::from([(1, last)]),
    };
    // Three retained revert points at 25, 50 and 75 percent of the log, as in ADR 0010.
    let marks: Vec<Hlc> = [n_ops / 4, n_ops / 2, 3 * n_ops / 4]
        .iter()
        .map(|i| ops.get(*i).map(text::Op::id))
        .collect::<Option<_>>()
        .ok_or("text workload too short")?;
    for _ in 0..o.samples {
        let input = ops.clone();
        let mut doc = text::Doc::new();
        let start = Instant::now();
        for op in input {
            black_box(&mut doc).apply(op).map_err(|e| e.to_string())?;
        }
        add(samples, "text_crdt_apply", nanos(start), n_ops as u64);
        if doc.text() != expected || doc.pending() != 0 {
            return Err("text replay diverged from the reference document".into());
        }
        let stored = doc.stored() as u64;
        for (name, retained) in [
            ("text_crdt_compact", &[][..]),
            ("text_crdt_compact_retained", &marks[..]),
        ] {
            let mut copy = doc.clone();
            let start = Instant::now();
            let result = black_box(&mut copy)
                .compact(black_box(&frontier), black_box(retained))
                .map_err(|e| e.to_string())?;
            // Denominator: stored elements before the pass (it is a linear scan of them).
            add(samples, name, nanos(start), stored);
            if result.removed == 0
                || copy.text() != expected
                || copy.tombstones() != result.tombstones_left
                || copy.stored() as u64 + result.removed as u64 != stored
            {
                return Err("text compaction changed the text or miscounted".into());
            }
        }
    }
    Ok(())
}

fn bench_text(o: &Options, samples: &mut Samples) -> Result<(), String> {
    // The plain sequence CRDT (`Rga`); the compacting text CRDT is `bench_text_crdt`.
    let n = (o.tree_ops() / 4).max(8);
    for _ in 0..o.samples {
        let mut rga = Rga::default();
        let mut rng = Rng(SEED ^ 2);
        let mut ids: Vec<Hlc> = Vec::new();
        let start = Instant::now();
        for i in 0..n {
            let after = if ids.is_empty() || rng.below(4) != 0 {
                ids.last().copied()
            } else {
                ids.get(rng.below(ids.len() as u64) as usize).copied()
            };
            let id = hlc(1 + i, 1);
            if !black_box(&mut rga).insert(after, 'x', id) {
                return Err("rga insert refused".into());
            }
            ids.push(id);
        }
        add(samples, "text_rga_insert", nanos(start), n);
        if rga.stored_chars() as u64 != n {
            return Err("rga lost characters".into());
        }
    }
    Ok(())
}

fn bench_merge(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let elements = o.tree_ops();
    let copies = 16usize;
    let mut left: OrSet<u64> = OrSet::default();
    let mut right: OrSet<u64> = OrSet::default();
    let mut rng = Rng(SEED ^ 3);
    for i in 0..elements {
        left.add(rng.below(elements * 2), hlc(i + 1, 1));
        right.add(rng.below(elements * 2), hlc(i + 1, 2));
        if i % 7 == 0 {
            right.remove(&rng.below(elements * 2));
        }
    }
    let mut expected = left.clone();
    expected.merge(&right);
    for _ in 0..o.samples {
        let mut targets: Vec<OrSet<u64>> = (0..copies).map(|_| left.clone()).collect();
        let start = Instant::now();
        for t in &mut targets {
            t.merge(black_box(&right));
        }
        add(
            samples,
            "orset_merge_element",
            nanos(start),
            copies as u64 * elements,
        );
        if targets.iter().any(|t| *t != expected) {
            return Err("set merge diverged from the reference merge".into());
        }
    }
    let merges = o.codec_ops();
    // Schema 2: every incoming register is strictly newer than the one before, so each merge
    // replaces the value. (Schema 1 cycled 64 registers, so after the first pass every merge was
    // the no-change path; that is now `lww_register_merge_noop`.)
    let newer: Vec<LwwRegister<u64>> = (0..merges)
        .map(|i| {
            let mut r = LwwRegister::default();
            r.set(hlc(i + 1, (i % 8) as u32), rng.next());
            r
        })
        .collect();
    let newest = newer.last().cloned().ok_or("no registers to merge")?;
    for _ in 0..o.samples {
        let mut reg = LwwRegister::default();
        let start = Instant::now();
        for r in &newer {
            reg.merge(black_box(r));
        }
        add(samples, "lww_register_merge", nanos(start), merges);
        if reg != newest {
            return Err("register merge did not end at the newest write".into());
        }
    }
    for _ in 0..o.samples {
        // Already holds the newest write: each merge is the no-change path.
        let mut reg = newest.clone();
        let start = Instant::now();
        for r in newer.iter().take(merges as usize) {
            reg.merge(black_box(r));
        }
        add(samples, "lww_register_merge_noop", nanos(start), merges);
        if reg != newest {
            return Err("an older write replaced the newest one".into());
        }
    }
    Ok(())
}

fn event(i: u64, rng: &mut Rng) -> ChangeEvent {
    ChangeEvent {
        cursor: Cursor::new(1 + i / 4, (i % 4) as u32).unwrap_or(Cursor::ORIGIN),
        set_len: 4,
        change_set: ChangeSetId::from_raw(u128::from(i) + 1),
        hlc: hlc(1_000 + i, 1),
        author: u128::from(rng.next()),
        turn: Some(TurnRef {
            session: 1,
            turn: i,
        }),
        change: EntityChange {
            kind: 7,
            key: rng.next().to_le_bytes().repeat(4),
            op: if i.is_multiple_of(5) {
                ChangeOp::Delete
            } else {
                ChangeOp::Upsert(vec![(i & 0xff) as u8; 128])
            },
        },
    }
}

fn bench_feed(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let n = o.codec_ops();
    let cursors: Vec<Cursor> = (0..n)
        .map(|i| Cursor::new(1 + i * 3, (i % 8) as u32))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut rng = Rng(SEED ^ 4);
    let events: Vec<ChangeEvent> = (0..n).map(|i| event(i, &mut rng)).collect();
    for _ in 0..o.samples {
        let start = Instant::now();
        let bytes: Vec<[u8; Cursor::ENCODED_LEN]> =
            cursors.iter().map(|c| black_box(*c).to_bytes()).collect();
        add(samples, "feed_cursor_encode", nanos(start), n);
        let start = Instant::now();
        let back: Vec<Cursor> = bytes
            .iter()
            .map(|b| Cursor::from_bytes(black_box(b)).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        add(samples, "feed_cursor_decode", nanos(start), n);
        if back != cursors {
            return Err("cursor codec did not round trip".into());
        }
        let start = Instant::now();
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| black_box(e).encode().map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        add(samples, "feed_event_encode", nanos(start), n);
        let start = Instant::now();
        let decoded: Vec<ChangeEvent> = encoded
            .iter()
            .map(|b| ChangeEvent::decode(black_box(b)).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        add(samples, "feed_event_decode", nanos(start), n);
        if decoded != events {
            return Err("event codec did not round trip".into());
        }
    }
    Ok(())
}

fn bench_sync(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let frames = (o.codec_ops() / 32).max(1);
    let mut rng = Rng(SEED ^ 5);
    let events_frame = |rng: &mut Rng| Message::Events {
        sub: 1,
        events: (0..32)
            .map(|_| Event {
                cursor: rng.next().to_be_bytes().to_vec(),
                data: vec![(rng.next() & 0xff) as u8; 96],
            })
            .collect(),
    };
    let msgs: Vec<Message> = (0..frames).map(|_| events_frame(&mut rng)).collect();
    let acks: Vec<Message> = (0..frames * 8)
        .map(|i| Message::Ack {
            sub: 1,
            cursor: i.to_be_bytes().to_vec(),
        })
        .collect();
    for (label, set) in [("events32", &msgs), ("ack", &acks)] {
        for _ in 0..o.samples {
            let mut buf = Vec::new();
            let start = Instant::now();
            for m in set {
                sync::encode(black_box(m), &mut buf).map_err(|e| e.to_string())?;
            }
            add(
                samples,
                &format!("sync_frame_encode_{label}"),
                nanos(start),
                set.len() as u64,
            );
            let mut pos = 0;
            let mut count = 0usize;
            let start = Instant::now();
            while let Some((m, used)) = sync::decode(black_box(
                buf.get(pos..).ok_or("frame cursor out of range")?,
            ))
            .map_err(|e| e.to_string())?
            {
                black_box(&m);
                pos += used;
                count += 1;
            }
            add(
                samples,
                &format!("sync_frame_decode_{label}"),
                nanos(start),
                set.len() as u64,
            );
            if count != set.len() || pos != buf.len() {
                return Err("sync frame decode did not consume every frame".into());
            }
        }
    }
    Ok(())
}

/// Fan-out through the real backpressure `Hub`. The timed part is `publish_shared` of one event
/// to every subscriber; every tenth subscriber never drains or acknowledges, so it overflows and
/// must not stall the writer. Draining and acknowledging the others is untimed.
fn bench_fanout(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let events = o.fanout_events();
    let payload: Arc<[u8]> = Arc::from(vec![9u8; 128]);
    let limits = Limits {
        max_events: FANOUT_QUEUE_CAP,
        max_bytes: FANOUT_MAX_BYTES,
        max_in_flight: FANOUT_QUEUE_CAP,
    };
    for subs in [1usize, 100, 10_000] {
        let slow = |i: usize| i % FANOUT_SLOW_EVERY == FANOUT_SLOW_EVERY - 1;
        for _ in 0..o.samples {
            let mut hub = Hub::new();
            for _ in 0..subs {
                hub.add(limits, 0);
            }
            let mut publish_ns = 0.0;
            let mut out = Vec::with_capacity(FANOUT_QUEUE_CAP);
            for seq in 1..=events {
                let start = Instant::now();
                let fan = hub
                    .publish_shared(seq, black_box(Arc::clone(&payload)))
                    .map_err(|e| format!("{e:?}"))?;
                publish_ns += nanos(start);
                if fan.queued + fan.overflowed_now + fan.skipped != subs {
                    return Err("a publish did not reach every subscriber".into());
                }
                if seq % FANOUT_DRAIN_EVERY as u64 == 0 {
                    for i in (0..subs).filter(|i| !slow(*i)) {
                        let q = hub.get_mut(i).ok_or("missing subscriber")?;
                        out.clear();
                        q.drain_into(&mut out, FANOUT_QUEUE_CAP);
                        if let Some(last) = out.last() {
                            q.ack(last.seq).map_err(|e| format!("{e:?}"))?;
                        }
                    }
                }
            }
            add(
                samples,
                &format!("fanout_offer_{subs}_subs"),
                publish_ns,
                events,
            );
            // All publishes completed (no stall); only the stalled subscribers fell behind.
            for i in 0..subs {
                let overflowed = matches!(
                    hub.get(i).ok_or("missing subscriber")?.state(),
                    State::Overflowed { .. }
                );
                if overflowed != slow(i) {
                    return Err(format!(
                        "subscriber {i}: overflowed={overflowed} expected {}",
                        slow(i)
                    ));
                }
            }
            if hub.stats().publishes != events {
                return Err("hub publish count differs from the events published".into());
            }
        }
    }
    Ok(())
}

/// An in-memory event log for the reconnect storm: event `k` has the 8 byte big-endian cursor `k`
/// (1-based), so a read is a slice and costs the same for every client.
struct LogSource {
    events: Vec<Event>,
}
impl LogSource {
    fn new(len: u64) -> Self {
        Self {
            events: (1..=len)
                .map(|k| Event {
                    cursor: k.to_be_bytes().to_vec(),
                    data: vec![(k & 0xff) as u8; 96],
                })
                .collect(),
        }
    }
}
impl EventSource for LogSource {
    fn read_after(
        &self,
        scope: &str,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError> {
        if scope != STORM_SCOPE {
            return Err(SourceError::UnknownScope);
        }
        let from = if after.is_empty() {
            0
        } else {
            let b: [u8; 8] = after.try_into().map_err(|_| SourceError::BadCursor)?;
            usize::try_from(u64::from_be_bytes(b)).map_err(|_| SourceError::BadCursor)?
        };
        if from > self.events.len() {
            return Err(SourceError::BadCursor);
        }
        let rest = self.events.get(from..).ok_or(SourceError::BadCursor)?;
        Ok(rest.iter().take(limit).cloned().collect())
    }
}

/// Reconnect storm: each client opens a fresh `SubscriptionHost`, says hello, subscribes with a
/// cursor somewhere in the log (some at the head, some at the beginning) and is pumped until it
/// has every event, acknowledging each frame. One operation is one whole reconnect.
fn bench_storm(o: &Options, samples: &mut Samples) -> Result<(), String> {
    let source = LogSource::new(STORM_LOG_EVENTS);
    let clients = o.storm_clients();
    let mut rng = Rng(SEED ^ 8);
    // Cursor 0 means "from the beginning" (empty bytes); the others resume after that event.
    let resume_at: Vec<u64> = (0..clients)
        .map(|i| match i % 8 {
            0 => STORM_LOG_EVENTS,
            1 => 0,
            _ => rng.below(STORM_LOG_EVENTS),
        })
        .collect();
    let missing: u64 = resume_at.iter().map(|c| STORM_LOG_EVENTS - c).sum();
    for _ in 0..o.samples {
        let mut delivered = 0u64;
        let start = Instant::now();
        for (i, cursor) in resume_at.iter().enumerate() {
            let mut host =
                SubscriptionHost::new(HostConfig::new("lattice-bench"), 0).map_err(|e| e.0)?;
            let hello = Message::Hello {
                min_version: 1,
                max_version: 1,
                agent: "bench".into(),
            };
            let mut outputs = host.handle(black_box(hello), 0, &source);
            let subscribe = Message::Subscribe {
                sub: 1,
                scope: STORM_SCOPE.into(),
                cursor: if *cursor == 0 {
                    Vec::new()
                } else {
                    cursor.to_be_bytes().to_vec()
                },
            };
            outputs.extend(host.handle(black_box(subscribe), 0, &source));
            for tick in 1..=STORM_LOG_EVENTS {
                let mut got = false;
                let mut last: Option<Vec<u8>> = None;
                for out in outputs.drain(..) {
                    match out {
                        Output::Send(Message::Events { events, .. }) => {
                            delivered += events.len() as u64;
                            got |= !events.is_empty();
                            last = events.last().map(|e| e.cursor.clone()).or(last);
                        }
                        Output::Send(_) => {}
                        Output::Close(_) => return Err(format!("client {i} was closed")),
                    }
                }
                if let Some(cursor) = last {
                    let ack = Message::Ack { sub: 1, cursor };
                    if !host.handle(ack, tick, &source).is_empty() {
                        return Err(format!("client {i}: unexpected reply to an ack"));
                    }
                }
                if !got {
                    break;
                }
                outputs = host.poll(tick, &source);
            }
        }
        add(samples, "reconnect_storm_client", nanos(start), clients);
        if delivered != missing {
            return Err(format!(
                "reconnect storm delivered {delivered} events, expected {missing}"
            ));
        }
    }
    Ok(())
}

fn measure(o: &Options) -> Result<(Results, Value), String> {
    let mut samples = Samples::new();
    bench_tree(o, &mut samples)?;
    bench_text_crdt(o, &mut samples)?;
    bench_text(o, &mut samples)?;
    bench_storm(o, &mut samples)?;
    bench_merge(o, &mut samples)?;
    bench_feed(o, &mut samples)?;
    bench_sync(o, &mut samples)?;
    bench_fanout(o, &mut samples)?;
    let mut results = Results::new();
    let mut cases = serde_json::Map::new();
    for (name, runs) in &samples {
        let mut per_op: Vec<f64> = runs.iter().map(|(ns, ops)| ns / *ops as f64).collect();
        per_op.sort_by(f64::total_cmp);
        let mid = per_op.len() / 2;
        let ns_per_op = if per_op.len().is_multiple_of(2) {
            let lo = per_op.get(mid.wrapping_sub(1)).ok_or("no samples")?;
            let hi = per_op.get(mid).ok_or("no samples")?;
            lo / 2.0 + hi / 2.0
        } else {
            *per_op.get(mid).ok_or("no samples")?
        };
        if !(ns_per_op.is_finite() && ns_per_op > 0.0) {
            return Err(format!("case {name} produced a non-positive time"));
        }
        let ops = runs.first().map_or(0, |r| r.1);
        cases.insert(
            name.clone(),
            json!({"ns_per_op":ns_per_op,"ops_per_s":1e9 / ns_per_op,"ops":ops}),
        );
        results.insert(name.clone(), ns_per_op);
    }
    Ok((
        results,
        json!({"metadata":o.metadata(),"cases":Value::Object(cases)}),
    ))
}

fn baseline_paths(profile: &str) -> Result<(PathBuf, PathBuf), String> {
    let current = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = current
        .ancestors()
        .find(|path| {
            path.join("Cargo.toml").is_file()
                && path.join("crates/lattice-bench/Cargo.toml").is_file()
                && path.join("benches/baselines").is_dir()
        })
        .ok_or("run phase3_bench inside the Lattice workspace")?;
    let dir = root.join("benches/baselines/local");
    Ok((
        dir.join(format!("phase3-{profile}.json")),
        dir.join(format!("phase3-{profile}-metadata.json")),
    ))
}

/// Metadata without the CPU model: it is recorded for the reader, but hosted runners change CPU
/// between runs, so it must not make a baseline incomparable.
fn comparable(metadata: &Value) -> Value {
    let mut copy = metadata.clone();
    if let Some(map) = copy.as_object_mut() {
        map.remove("cpu_model");
    }
    copy
}

fn run(options: Options) -> Result<bool, String> {
    let (baseline_path, metadata_path) = baseline_paths(&options.profile)?;
    let baseline = if options.write {
        None
    } else {
        let metadata: Value = serde_json::from_str(
            &std::fs::read_to_string(&metadata_path).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if comparable(&metadata) != comparable(&options.metadata()) {
            return Err(
                "baseline workload/platform metadata differs; record a new baseline".into(),
            );
        }
        Some(parse_results(
            &std::fs::read_to_string(&baseline_path).map_err(|e| e.to_string())?,
        )?)
    };
    let (current, report) = measure(&options)?;
    for (name, ns) in &current {
        eprintln!("{name:<36} {ns:>12.1} ns/op {:>16.0} ops/s", 1e9 / ns);
    }
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|e| e.to_string())?
    );
    if let Some(baseline) = baseline {
        if baseline.keys().ne(current.keys()) {
            return Err("baseline benchmark set differs; record a new baseline".into());
        }
        let regressions = compare(&baseline, &current, options.tolerance)?;
        for r in &regressions {
            eprintln!("REGRESSION {}: {:+.1}%", r.name, r.slowdown() * 100.0);
        }
        Ok(regressions.is_empty())
    } else {
        let parent = baseline_path.parent().ok_or("baseline has no directory")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        std::fs::write(&baseline_path, render_results(&current)).map_err(|e| e.to_string())?;
        std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&options.metadata()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        eprintln!("local baseline written to {}", baseline_path.display());
        Ok(true)
    }
}

fn main() -> std::process::ExitCode {
    match Options::parse(std::env::args().skip(1)).and_then(run) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("phase 3 benchmark: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn options_reject_bad_profiles_scales_and_tolerances() {
        for args in [
            vec!["--profile", "../x"],
            vec!["--scale", "0"],
            vec!["--samples", "0"],
            vec!["--tolerance", "NaN"],
            vec!["--bogus", "1"],
        ] {
            assert!(opts(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn tiny_workload_reports_every_case_in_the_json_shape() {
        let o = opts(&["--scale", "1", "--samples", "1"]).unwrap();
        let (results, report) = measure(&o).unwrap();
        let names = [
            "tree_apply_in_order",
            "tree_apply_late",
            "tree_apply_late_before_compact",
            "tree_compact",
            "tree_apply_late_after_compact",
            "text_crdt_apply",
            "text_crdt_compact",
            "text_crdt_compact_retained",
            "text_rga_insert",
            "reconnect_storm_client",
            "orset_merge_element",
            "lww_register_merge",
            "lww_register_merge_noop",
            "feed_cursor_encode",
            "feed_cursor_decode",
            "feed_event_encode",
            "feed_event_decode",
            "sync_frame_encode_events32",
            "sync_frame_decode_events32",
            "sync_frame_encode_ack",
            "sync_frame_decode_ack",
            "fanout_offer_1_subs",
            "fanout_offer_100_subs",
            "fanout_offer_10000_subs",
        ];
        assert_eq!(results.len(), names.len());
        let cases = report["cases"].as_object().unwrap();
        for name in names {
            let c = &cases[name];
            assert!(c["ns_per_op"].as_f64().unwrap() > 0.0, "{name}");
            assert!(c["ops_per_s"].as_f64().unwrap() > 0.0, "{name}");
            assert!(c["ops"].as_u64().unwrap() > 0, "{name}");
        }
        assert_eq!(report["metadata"]["seed"], SEED);
        assert_eq!(report["metadata"]["schema"], 2);
        assert_eq!(report["metadata"], o.metadata());
        let cpu = report["metadata"]["cpu_model"].as_str().unwrap();
        assert!(!cpu.is_empty() && cpu.len() <= 128, "{cpu}");
        for (name, c) in cases {
            let keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(keys, ["ns_per_op", "ops", "ops_per_s"], "{name}");
        }
    }

    #[test]
    fn cpu_model_never_blocks_a_baseline_comparison() {
        let o = opts(&[]).unwrap();
        let mut other = o.metadata();
        other["cpu_model"] = json!("some other cpu");
        assert_eq!(comparable(&o.metadata()), comparable(&other));
        other["scale"] = json!(9);
        assert_ne!(comparable(&o.metadata()), comparable(&other));
    }
}
