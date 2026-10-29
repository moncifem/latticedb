//! Offline outbox: queue writes while disconnected and deliver them at least once, in order,
//! with idempotent record ids (WP49, P3-04).
//!
//! [`Outbox`] is a pure state machine in the same style as [`crate::client::SyncClient`]: no I/O,
//! no thread, no real time, no randomness. The caller owns the transport, the clock (any
//! monotonic logical counter) and the jitter value, and stores the queue with [`Outbox::snapshot`].
//!
//! # Why a crash can neither lose nor double a write
//!
//! * A write gets its record id at [`Outbox::enqueue`]: `"<client id>-<counter>"`, where the
//!   counter is a monotonic number stored with the queue. The id never depends on time, on
//!   randomness or on how many times the write was sent, so a resend after a restore carries the
//!   same id and the server's dedupe (`duplicate` receipt) makes it a no-op.
//! * An item leaves the queue only when a `recorded`, `duplicate` or `fenced` receipt for its id
//!   arrives. A crash before the receipt is processed leaves the item in the stored queue; the
//!   restored outbox sends it again.
//! * The one dangerous crash is a send of an id the stored queue never contained: after restore the
//!   counter would be reused for a different write and the server would answer `duplicate` for a
//!   write it never applied. [`Outbox::poll`] therefore sends nothing that is newer than the last
//!   [`Outbox::mark_persisted`] call, and that call takes a [`Snapshot`]: take one with
//!   [`Outbox::snapshot`], store its bytes, then hand it back. The snapshot carries the counter it
//!   was taken at, so an enqueue between the two steps stays unsendable, and there is no way to
//!   report a counter that was not stored.
//!
//! # Retry
//!
//! One batch is in flight at a time (stop and wait), which keeps delivery in enqueue order. A batch
//! is resent when its deadline passes: `base_delay << attempt`, capped at `max_delay`, plus the
//! caller's jitter reduced to at most half of that delay. After a lost connection
//! ([`Outbox::connection_lost`]) the unacked batch is sent again at the next poll and reports
//! `attempt` 0 again: the attempt number counts resends on one connection, not over the life of
//! the write.
//!
//! # Fencing
//!
//! A `fenced` receipt means another turn took over: the write was not applied and never will be.
//! The outbox drops every queued write of that turn, returns them in [`Outcome::fenced`] for the
//! application and refuses further writes for the turn. It never retries them. The report lives
//! only in the returned [`Outcome`]: if the process dies before the application handled it and the
//! queue is restored from older bytes, the writes are sent again, the server answers `fenced`
//! again and they are reported again.
//!
//! # Stored format (version 1)
//!
//! All integers are little endian; there is exactly one encoding of a queue.
//!
//! ```text
//! magic "LXOB" | version u8 = 1 | client id (u16 len, bytes) | next counter u64
//! | fenced turn count u16, u64 each, strictly increasing
//! | item count u32, per item: counter u64, turn u64, kind u8, body (u32 len, UTF-8)
//! | CRC-32 (IEEE) u32 of every byte before it
//! ```
//!
//! Item counters strictly increase and stay below the next counter; no item belongs to a fenced
//! turn; the client id uses the characters allowed by [`MAX_CLIENT_ID_BYTES`]; there are no
//! trailing bytes. [`Outbox::from_bytes`] checks the length bound and the checksum first and never
//! panics on any input. CRC-32 detects accidental corruption (torn writes, bit rot) only; it is not
//! authentication, and anyone who can write the bytes can forge a valid queue. Limits come from the
//! configuration passed to `from_bytes`, so a queue stored under a larger configuration fails with
//! [`DecodeError::OverLimit`] after the limits are tightened; restore it with the old limits and
//! drain it first.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;

use crate::contract::{Kind, MAX_BODY_BYTES, MAX_INT, MAX_RECORDS, Receipt, Record, Status};

/// Longest client id, in bytes. Allowed characters: ASCII letters, digits, `.`, `_` and `-`.
/// With a 16 digit counter and the separator the record id stays within `MAX_RID_BYTES`.
pub const MAX_CLIENT_ID_BYTES: usize = 100;
/// Largest stored queue, in bytes; larger input is refused before it is read.
pub const MAX_STORED_BYTES: usize = 32 * 1024 * 1024;
/// Largest `max_items` a configuration may ask for.
pub const MAX_ITEMS_LIMIT: usize = 65_536;
/// Largest `max_bytes` a configuration may ask for.
pub const MAX_BYTES_LIMIT: usize = 16 * 1024 * 1024;
/// Most fenced turns remembered; the oldest are forgotten first.
pub const MAX_FENCED_TURNS: usize = 256;

const MAGIC: [u8; 4] = *b"LXOB";
const VERSION: u8 = 1;

/// Limits and timing, in logical ticks. Not stored with the queue: the application supplies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxConfig {
    /// Most queued writes.
    pub max_items: usize,
    /// Most queued body bytes.
    pub max_bytes: usize,
    /// Most records in one batch, at most [`MAX_RECORDS`].
    pub max_batch: usize,
    /// Wait before the first resend, at least 1.
    pub base_delay: u64,
    /// Longest wait before a resend (without jitter), at least `base_delay`.
    pub max_delay: u64,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            max_items: 1024,
            max_bytes: 4 * 1024 * 1024,
            max_batch: 32,
            base_delay: 2,
            max_delay: 64,
        }
    }
}

impl OutboxConfig {
    fn validate(&self) -> bool {
        (1..=MAX_ITEMS_LIMIT).contains(&self.max_items)
            && (1..=MAX_BYTES_LIMIT).contains(&self.max_bytes)
            && (1..=MAX_RECORDS).contains(&self.max_batch)
            && self.base_delay >= 1
            && self.max_delay >= self.base_delay
    }
}

/// Why an outbox call was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxError {
    /// The configuration is out of range.
    InvalidConfig,
    /// The client id is empty, too long or uses a character outside the allowed set.
    InvalidClientId,
    /// The queue holds the most items or bytes the configuration allows. Nothing was queued.
    Full {
        /// Items queued.
        items: usize,
        /// Body bytes queued.
        bytes: usize,
    },
    /// The body is longer than the contract allows.
    BodyTooLarge,
    /// The body alone is larger than `max_bytes`, so it could never be queued, even when empty.
    BodyExceedsBudget,
    /// The turn number is above the largest wire integer.
    TurnTooLarge,
    /// The turn was fenced; its writes are refused.
    TurnFenced(u64),
    /// The record counter reached the largest wire integer.
    CounterExhausted,
}

impl fmt::Display for OutboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => write!(f, "invalid outbox configuration"),
            Self::InvalidClientId => write!(f, "invalid client id"),
            Self::Full { items, bytes } => {
                write!(f, "outbox full ({items} items, {bytes} bytes)")
            }
            Self::BodyTooLarge => write!(f, "body too large"),
            Self::BodyExceedsBudget => write!(f, "body larger than the outbox byte budget"),
            Self::TurnTooLarge => write!(f, "turn number too large"),
            Self::TurnFenced(t) => write!(f, "turn {t} is fenced"),
            Self::CounterExhausted => write!(f, "record counter exhausted"),
        }
    }
}

impl std::error::Error for OutboxError {}

/// Why stored bytes were refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Longer than [`MAX_STORED_BYTES`].
    TooLarge,
    /// Shorter than the fixed parts.
    TooShort,
    /// Does not start with the magic bytes.
    BadMagic,
    /// A version this code does not read.
    UnsupportedVersion(u8),
    /// The checksum does not match the content.
    BadChecksum,
    /// A length field runs past the end of the data.
    Truncated,
    /// Bytes remain after the last field.
    TrailingBytes,
    /// A field breaks the canonical form (the text says which).
    NonCanonical(&'static str),
    /// The queue holds more than the supplied configuration allows.
    OverLimit,
    /// The supplied configuration is out of range.
    InvalidConfig,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "stored outbox too large"),
            Self::TooShort => write!(f, "stored outbox too short"),
            Self::BadMagic => write!(f, "not a stored outbox"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported outbox version {v}"),
            Self::BadChecksum => write!(f, "checksum mismatch"),
            Self::Truncated => write!(f, "stored outbox truncated"),
            Self::TrailingBytes => write!(f, "trailing bytes after the stored outbox"),
            Self::NonCanonical(why) => write!(f, "non-canonical stored outbox: {why}"),
            Self::OverLimit => write!(f, "stored outbox exceeds the configured limits"),
            Self::InvalidConfig => write!(f, "invalid outbox configuration"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The stored form of the queue at one moment. Created only by [`Outbox::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    bytes: Vec<u8>,
    upto: u64,
}

impl Snapshot {
    /// The bytes to store; [`Outbox::from_bytes`] reads them back.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes to store, by value.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A write waiting for its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Idempotent record id.
    pub rid: String,
    /// Turn the write belongs to.
    pub turn: u64,
    /// Record kind.
    pub kind: Kind,
    /// Record body.
    pub body: String,
}

/// A batch to send as one `append` for `turn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Turn every record belongs to.
    pub turn: u64,
    /// Records in enqueue order.
    pub records: Vec<Record>,
    /// 0 for the first send of this batch, then 1, 2, ... for each resend.
    pub attempt: u32,
}

/// What a set of receipts changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Record ids cleared by a `recorded` or `duplicate` receipt, in receipt order.
    pub done: Vec<String>,
    /// Writes dropped because their turn was fenced. The application decides what to do.
    pub fenced: Vec<Pending>,
    /// Receipts that answered no queued write (late duplicates, foreign ids).
    pub unknown: usize,
}

#[derive(Debug, Clone)]
struct Item {
    counter: u64,
    turn: u64,
    kind: Kind,
    body: String,
}

#[derive(Debug, Clone)]
struct Flight {
    counters: Vec<u64>,
    deadline: u64,
}

/// The offline outbox. See the module documentation.
#[derive(Debug, Clone)]
pub struct Outbox {
    cfg: OutboxConfig,
    client_id: String,
    next_counter: u64,
    persisted_upto: u64,
    fenced: BTreeSet<u64>,
    items: VecDeque<Item>,
    bytes: usize,
    flight: Option<Flight>,
    attempt: u32,
}

fn valid_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CLIENT_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Outbox {
    /// An empty outbox for `client_id`. The id must be stable across restarts of the same
    /// installation and never shared by two live installations: it scopes the record ids.
    pub fn new(client_id: &str, cfg: OutboxConfig) -> Result<Self, OutboxError> {
        if !cfg.validate() {
            return Err(OutboxError::InvalidConfig);
        }
        if !valid_client_id(client_id) {
            return Err(OutboxError::InvalidClientId);
        }
        Ok(Self {
            cfg,
            client_id: client_id.to_owned(),
            next_counter: 0,
            persisted_upto: 0,
            fenced: BTreeSet::new(),
            items: VecDeque::new(),
            bytes: 0,
            flight: None,
            attempt: 0,
        })
    }

    /// Queue a write and return its record id. The write is not sent until it has been persisted
    /// (see [`Outbox::mark_persisted`]). On an error nothing changes.
    pub fn enqueue(&mut self, turn: u64, kind: Kind, body: &str) -> Result<String, OutboxError> {
        if turn > MAX_INT {
            return Err(OutboxError::TurnTooLarge);
        }
        if self.fenced.contains(&turn) {
            return Err(OutboxError::TurnFenced(turn));
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(OutboxError::BodyTooLarge);
        }
        if body.len() > self.cfg.max_bytes {
            return Err(OutboxError::BodyExceedsBudget);
        }
        if self.next_counter > MAX_INT {
            return Err(OutboxError::CounterExhausted);
        }
        let new_bytes = self.bytes.saturating_add(body.len());
        if self.items.len() >= self.cfg.max_items || new_bytes > self.cfg.max_bytes {
            return Err(OutboxError::Full {
                items: self.items.len(),
                bytes: self.bytes,
            });
        }
        let counter = self.next_counter;
        self.next_counter = counter.saturating_add(1);
        self.bytes = new_bytes;
        self.items.push_back(Item {
            counter,
            turn,
            kind,
            body: body.to_owned(),
        });
        Ok(self.rid(counter))
    }

    fn rid(&self, counter: u64) -> String {
        format!("{}-{counter}", self.client_id)
    }

    /// The counter a record id carries, when it is one this outbox could have issued.
    fn counter_of(&self, rid: &str) -> Option<u64> {
        let digits = rid
            .strip_prefix(self.client_id.as_str())?
            .strip_prefix('-')?;
        if digits.is_empty()
            || digits.len() > 16
            || (digits.len() > 1 && digits.starts_with('0'))
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        digits.parse().ok()
    }

    /// Declare that `snapshot` is safely stored. Writes enqueued before it was taken become
    /// sendable; later ones stay held until a later snapshot is stored. An older snapshot than one
    /// already marked changes nothing.
    ///
    /// The counter cannot be passed on its own, so the mistake of reporting a counter newer than
    /// the stored bytes does not compile:
    ///
    /// ```compile_fail
    /// # use lattice_sdk::outbox::{Outbox, OutboxConfig};
    /// let mut o = Outbox::new("c", OutboxConfig::default()).unwrap();
    /// o.mark_persisted(7);
    /// ```
    pub fn mark_persisted(&mut self, snapshot: &Snapshot) {
        if snapshot.upto > self.persisted_upto && snapshot.upto <= self.next_counter {
            self.persisted_upto = snapshot.upto;
        }
    }

    /// The queue as it is now, in the canonical stored form, together with the point up to which
    /// writes are in it. Store [`Snapshot::as_bytes`], then pass the snapshot to
    /// [`Outbox::mark_persisted`].
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            bytes: self.encode(),
            upto: self.next_counter,
        }
    }

    /// The next batch to send at logical time `now`, or `None` when nothing is due. `jitter` is any
    /// number the caller chooses; it adds between 0 and half the delay to the retry deadline.
    pub fn poll(&mut self, now: u64, jitter: u64) -> Option<Batch> {
        let counters: Vec<u64> = match &self.flight {
            Some(f) => {
                if now < f.deadline {
                    return None;
                }
                self.attempt = self.attempt.saturating_add(1);
                f.counters.clone()
            }
            None => self.next_batch(),
        };
        let turn = counters.first().and_then(|c| self.find(*c))?.turn;
        let records = counters
            .iter()
            .filter_map(|c| self.find(*c))
            .map(|i| Record {
                rid: self.rid(i.counter),
                kind: i.kind,
                body: i.body.clone(),
            })
            .collect();
        let deadline = now.saturating_add(self.delay(jitter));
        self.flight = Some(Flight { counters, deadline });
        Some(Batch {
            turn,
            records,
            attempt: self.attempt,
        })
    }

    /// The oldest run of persisted writes of one turn, up to `max_batch`.
    fn next_batch(&self) -> Vec<u64> {
        let mut out = Vec::new();
        let mut turn = None;
        for it in &self.items {
            if it.counter >= self.persisted_upto || out.len() >= self.cfg.max_batch {
                break;
            }
            if *turn.get_or_insert(it.turn) != it.turn {
                break;
            }
            out.push(it.counter);
        }
        out
    }

    fn index_of(&self, counter: u64) -> Option<usize> {
        self.items
            .binary_search_by_key(&counter, |i| i.counter)
            .ok()
    }

    fn find(&self, counter: u64) -> Option<&Item> {
        self.items.get(self.index_of(counter)?)
    }

    fn delay(&self, jitter: u64) -> u64 {
        let shift = self.attempt.min(63);
        let base = self
            .cfg
            .base_delay
            .checked_shl(shift)
            .filter(|d| d >> shift == self.cfg.base_delay)
            .unwrap_or(u64::MAX)
            .min(self.cfg.max_delay);
        base.saturating_add(jitter % (base / 2).saturating_add(1))
    }

    /// The logical time at which [`Outbox::poll`] will next resend, if a batch is in flight.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        self.flight.as_ref().map(|f| f.deadline)
    }

    /// Process receipts from an `append.receipt`. `recorded` and `duplicate` clear the write;
    /// `fenced` drops every queued write of that turn and reports them.
    pub fn on_receipts(&mut self, receipts: &[Receipt]) -> Outcome {
        let mut out = Outcome::default();
        for r in receipts {
            let Some(idx) = self.counter_of(&r.rid).and_then(|c| self.index_of(c)) else {
                out.unknown += 1;
                continue;
            };
            match r.status {
                Status::Recorded | Status::Duplicate => {
                    self.remove_at(idx);
                    out.done.push(r.rid.clone());
                    self.attempt = 0;
                }
                Status::Fenced => {
                    let turn = self.items.get(idx).map_or(0, |i| i.turn);
                    self.fence_turn(turn, &mut out);
                }
            }
        }
        if let Some(f) = &mut self.flight {
            let items = &self.items;
            f.counters
                .retain(|c| items.binary_search_by_key(c, |i| i.counter).is_ok());
            if f.counters.is_empty() {
                self.flight = None;
            }
        }
        out
    }

    fn remove_at(&mut self, idx: usize) -> Option<Item> {
        let item = self.items.remove(idx)?;
        self.bytes = self.bytes.saturating_sub(item.body.len());
        Some(item)
    }

    fn fence_turn(&mut self, turn: u64, out: &mut Outcome) {
        self.fenced.insert(turn);
        while self.fenced.len() > MAX_FENCED_TURNS {
            self.fenced.pop_first();
        }
        // One pass: linear in the queue length.
        for it in std::mem::take(&mut self.items) {
            if it.turn == turn {
                self.bytes = self.bytes.saturating_sub(it.body.len());
                out.fenced.push(Pending {
                    rid: self.rid(it.counter),
                    turn: it.turn,
                    kind: it.kind,
                    body: it.body,
                });
            } else {
                self.items.push_back(it);
            }
        }
        self.attempt = 0;
    }

    /// The connection dropped: the unacked batch is sent again at the next poll, with the first
    /// attempt's delay. Queued writes are kept.
    pub fn connection_lost(&mut self) {
        self.flight = None;
        self.attempt = 0;
    }

    /// Number of queued writes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Queued body bytes.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The client id scoping this outbox's record ids.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Turns whose writes were fenced and are refused.
    pub fn fenced_turns(&self) -> impl Iterator<Item = u64> + '_ {
        self.fenced.iter().copied()
    }

    /// The queued writes in delivery order.
    #[must_use]
    pub fn pending(&self) -> Vec<Pending> {
        self.items
            .iter()
            .map(|i| Pending {
                rid: self.rid(i.counter),
                turn: i.turn,
                kind: i.kind,
                body: i.body.clone(),
            })
            .collect()
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.bytes + self.items.len() * 21);
        b.extend_from_slice(&MAGIC);
        b.push(VERSION);
        b.extend_from_slice(&(self.client_id.len() as u16).to_le_bytes());
        b.extend_from_slice(self.client_id.as_bytes());
        b.extend_from_slice(&self.next_counter.to_le_bytes());
        b.extend_from_slice(&(self.fenced.len() as u16).to_le_bytes());
        for t in &self.fenced {
            b.extend_from_slice(&t.to_le_bytes());
        }
        b.extend_from_slice(&(self.items.len() as u32).to_le_bytes());
        for it in &self.items {
            b.extend_from_slice(&it.counter.to_le_bytes());
            b.extend_from_slice(&it.turn.to_le_bytes());
            b.push(kind_byte(it.kind));
            b.extend_from_slice(&(it.body.len() as u32).to_le_bytes());
            b.extend_from_slice(it.body.as_bytes());
        }
        let crc = crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// Restore a queue stored from [`Outbox::snapshot`] bytes. Every restored write keeps its record id;
    /// the in-flight state is not stored, so unacked writes are sent again at the next poll.
    pub fn from_bytes(bytes: &[u8], cfg: OutboxConfig) -> Result<Self, DecodeError> {
        if !cfg.validate() {
            return Err(DecodeError::InvalidConfig);
        }
        if bytes.len() > MAX_STORED_BYTES {
            return Err(DecodeError::TooLarge);
        }
        // magic + version + id len + counter + fenced count + item count + crc
        if bytes.len() < 4 + 1 + 2 + 8 + 2 + 4 + 4 {
            return Err(DecodeError::TooShort);
        }
        let (body, tail) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(tail.try_into().map_err(|_| DecodeError::TooShort)?);
        let mut r = Reader { data: body };
        if r.take(4)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        if crc32(body) != stored {
            return Err(DecodeError::BadChecksum);
        }
        let id_len = usize::from(r.u16()?);
        let client_id = std::str::from_utf8(r.take(id_len)?)
            .map_err(|_| DecodeError::NonCanonical("client id is not UTF-8"))?
            .to_owned();
        if !valid_client_id(&client_id) {
            return Err(DecodeError::NonCanonical("client id"));
        }
        let next_counter = r.u64()?;
        if next_counter > MAX_INT.saturating_add(1) {
            return Err(DecodeError::NonCanonical("counter too large"));
        }
        let n_fenced = usize::from(r.u16()?);
        if n_fenced > MAX_FENCED_TURNS {
            return Err(DecodeError::NonCanonical("too many fenced turns"));
        }
        let mut fenced = BTreeSet::new();
        let mut prev: Option<u64> = None;
        for _ in 0..n_fenced {
            let t = r.u64()?;
            if t > MAX_INT || prev.is_some_and(|p| t <= p) {
                return Err(DecodeError::NonCanonical("fenced turns"));
            }
            prev = Some(t);
            fenced.insert(t);
        }
        let n_items = r.u32()? as usize;
        if n_items > cfg.max_items {
            return Err(DecodeError::OverLimit);
        }
        let mut items = VecDeque::new();
        let mut total = 0usize;
        let mut prev: Option<u64> = None;
        for _ in 0..n_items {
            let counter = r.u64()?;
            let turn = r.u64()?;
            let kind = kind_from_byte(r.u8()?)?;
            let len = r.u32()? as usize;
            if counter >= next_counter || prev.is_some_and(|p| counter <= p) {
                return Err(DecodeError::NonCanonical("item counters"));
            }
            if turn > MAX_INT || fenced.contains(&turn) {
                return Err(DecodeError::NonCanonical("item turn"));
            }
            if len > MAX_BODY_BYTES {
                return Err(DecodeError::NonCanonical("body too large"));
            }
            total = total.saturating_add(len);
            if total > cfg.max_bytes {
                return Err(DecodeError::OverLimit);
            }
            let text = std::str::from_utf8(r.take(len)?)
                .map_err(|_| DecodeError::NonCanonical("body is not UTF-8"))?
                .to_owned();
            prev = Some(counter);
            items.push_back(Item {
                counter,
                turn,
                kind,
                body: text,
            });
        }
        if !r.data.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(Self {
            cfg,
            client_id,
            next_counter,
            persisted_upto: next_counter,
            fenced,
            items,
            bytes: total,
            flight: None,
            attempt: 0,
        })
    }
}

fn kind_byte(k: Kind) -> u8 {
    match k {
        Kind::Message => 0,
        Kind::ToolCall => 1,
        Kind::ToolResult => 2,
        Kind::Note => 3,
    }
}

fn kind_from_byte(b: u8) -> Result<Kind, DecodeError> {
    Ok(match b {
        0 => Kind::Message,
        1 => Kind::ToolCall,
        2 => Kind::ToolResult,
        3 => Kind::Note,
        _ => return Err(DecodeError::NonCanonical("record kind")),
    })
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let (head, rest) = self
            .data
            .split_at_checked(n)
            .ok_or(DecodeError::Truncated)?;
        self.data = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        self.take(1)?.first().copied().ok_or(DecodeError::Truncated)
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self
            .take(2)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?;
        Ok(u16::from_le_bytes(b))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self
            .take(4)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?;
        Ok(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self
            .take(8)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?;
        Ok(u64::from_le_bytes(b))
    }
}

/// CRC-32 (IEEE 802.3, reflected, polynomial 0xEDB88320), bitwise; the queue is small.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn crc32_matches_the_published_check_value() {
        // The standard check value for CRC-32/ISO-HDLC over "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
