//! Server-arbitrated workflow state (WP47, P3-09).
//!
//! Workflow state is not merged like a CRDT: two replicas that change the state of one entity while
//! apart cannot both be right, and merging would silently pick one. Instead the server arbitrates.
//! Every state change arrives as a [`Request`] that names the state the client saw (`from`), and a
//! pure function ([`decide`]) answers with exactly one of [`Decision::Accepted`],
//! [`Decision::Rejected`] (with a stable [`RejectReason`] code) or [`Decision::Conflict`] (a
//! [`ConflictEntity`] that lists every competing proposal and the ways to resolve it).
//!
//! # Rules
//!
//! 1. A request id is single use. Re-sending a request returns the stored decision and changes
//!    nothing; re-using an id for different content is rejected.
//! 2. Names must be known to the [`WorkflowDef`], the transition must be allowed and its [`Guard`]
//!    must admit the actor, otherwise the request is rejected.
//! 3. If the entity has an open conflict, a request based on the conflict's base state joins the
//!    conflict as another proposal (up to [`MAX_PROPOSALS`]); anything else is rejected until the
//!    conflict is resolved.
//! 4. A request whose `from` equals the current state is accepted.
//! 5. A request whose `from` is stale is accepted only if no other transition was accepted since the
//!    client saw that state. Since the current state differs, one was; so a stale request is
//!    a conflict, unless it asks for the state the entity is already in
//!    ([`RejectReason::AlreadyInTarget`]), or a conflict resolution happened since
//!    ([`RejectReason::StaleAfterResolution`]), or the client named a state the entity never had
//!    ([`RejectReason::StateNeverObserved`]).
//! 6. A new conflict holds the transitions accepted since the base state (they are marked
//!    superseded) together with the stale request. The entity returns to the base state and stays
//!    there until [`History::resolve`] closes the conflict. Because the conflict content is a sorted
//!    set of proposals, two replicas arriving in either order yield the same conflict and state.
//!
//! No proposal is ever dropped without a trace: each is accepted, rejected with a code, or recorded
//! in a conflict. The state machine reads no clock and does no I/O, and uses ordered collections
//! only, so replaying the same ordered requests always gives the same result.

use lattice_abi::Hlc;
use std::collections::{BTreeMap, BTreeSet};

/// Longest state name, in bytes.
pub const MAX_NAME_LEN: usize = 64;
/// Most states a definition may hold.
pub const MAX_STATES: usize = 256;
/// Most proposals one conflict may hold.
pub const MAX_PROPOSALS: usize = 16;

const VERSION: u8 = 1;

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_NAME_LEN
}

/// Why a [`WorkflowDef`] could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefError {
    /// A state name that is empty or longer than [`MAX_NAME_LEN`].
    #[error("invalid state name")]
    InvalidName,
    /// A transition names a state that is not in the definition.
    #[error("unknown state {0:?}")]
    UnknownState(String),
    /// More than [`MAX_STATES`] states.
    #[error("too many states")]
    TooManyStates,
}

/// A condition on a transition, expressed as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    /// Anyone may make the transition.
    Open,
    /// Only these actors may make the transition.
    Actors(BTreeSet<u64>),
}

/// The states of one workflow and the transitions between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDef {
    initial: String,
    states: BTreeSet<String>,
    transitions: BTreeMap<(String, String), Guard>,
}

impl WorkflowDef {
    /// A definition with a start state and no transitions. `initial` is added to `states`.
    pub fn new<I, S>(initial: &str, states: I) -> Result<Self, DefError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut set: BTreeSet<String> = states.into_iter().map(Into::into).collect();
        set.insert(initial.to_string());
        if set.len() > MAX_STATES {
            return Err(DefError::TooManyStates);
        }
        if !set.iter().all(|s| valid_name(s)) {
            return Err(DefError::InvalidName);
        }
        Ok(Self {
            initial: initial.to_string(),
            states: set,
            transitions: BTreeMap::new(),
        })
    }

    /// Allows `from -> to` under `guard`, replacing any earlier rule for the pair.
    pub fn allow(&mut self, from: &str, to: &str, guard: Guard) -> Result<(), DefError> {
        for s in [from, to] {
            if !self.states.contains(s) {
                return Err(DefError::UnknownState(s.to_string()));
            }
        }
        self.transitions
            .insert((from.to_string(), to.to_string()), guard);
        Ok(())
    }

    /// The state every entity starts in.
    pub fn initial(&self) -> &str {
        &self.initial
    }
}

/// A client's request to change the state of one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Unique per request; makes duplicates idempotent.
    pub id: u64,
    /// The entity whose state changes.
    pub entity: u128,
    /// The state the client saw when it made the request.
    pub from: String,
    /// The state the client wants.
    pub to: String,
    /// Who asks.
    pub actor: u64,
    /// When the client made the request.
    pub hlc: Hlc,
}

/// One request as recorded in a conflict. Ordered by `(hlc, request_id)` first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Proposal {
    /// When the client made the request.
    pub hlc: Hlc,
    /// The request id.
    pub request_id: u64,
    /// The state the client saw.
    pub from: String,
    /// The state the client wanted.
    pub to: String,
    /// Who asked.
    pub actor: u64,
}

impl Proposal {
    fn of(r: &Request) -> Self {
        Self {
            hlc: r.hlc,
            request_id: r.id,
            from: r.from.clone(),
            to: r.to.clone(),
            actor: r.actor,
        }
    }
}

/// One way to close a conflict.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictOption {
    /// Return to the base state and take none of the proposals.
    Revert,
    /// Move to this state, which one of the proposals asked for.
    Choose(String),
}

/// How a conflict was closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The option taken.
    pub choice: ConflictOption,
    /// Who decided.
    pub actor: u64,
}

/// Competing transitions of one entity, kept so none is lost silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntity {
    /// The entity in conflict.
    pub entity: u128,
    /// 1 for the first conflict of the entity, then counting up.
    pub ordinal: u32,
    /// The state all proposals were made from.
    pub base: String,
    /// Every competing proposal, sorted and without duplicates.
    pub proposals: Vec<Proposal>,
    /// The ways to close the conflict, sorted: `Revert` and one `Choose` per distinct target.
    pub options: Vec<ConflictOption>,
    /// Set once the conflict is closed.
    pub resolution: Option<Resolution>,
}

fn options_for(proposals: &[Proposal]) -> Vec<ConflictOption> {
    let mut set = BTreeSet::new();
    set.insert(ConflictOption::Revert);
    for p in proposals {
        set.insert(ConflictOption::Choose(p.to.clone()));
    }
    set.into_iter().collect()
}

impl ConflictEntity {
    fn build(entity: u128, ordinal: u32, base: &str, set: BTreeSet<Proposal>) -> Self {
        let proposals: Vec<Proposal> = set.into_iter().collect();
        let options = options_for(&proposals);
        Self {
            entity,
            ordinal,
            base: base.to_string(),
            proposals,
            options,
            resolution: None,
        }
    }

    /// Canonical bytes: the same conflict always encodes to the same bytes.
    pub fn encode(&self) -> Result<Vec<u8>, ConflictCodecError> {
        if self.ordinal == 0 || !valid_name(&self.base) {
            return Err(ConflictCodecError::Invalid("base or ordinal"));
        }
        if self.proposals.len() > MAX_PROPOSALS || self.options.len() > MAX_PROPOSALS + 1 {
            return Err(ConflictCodecError::Invalid("too many proposals"));
        }
        let mut o = vec![VERSION];
        o.extend_from_slice(&self.entity.to_be_bytes());
        o.extend_from_slice(&self.ordinal.to_le_bytes());
        put_name(&mut o, &self.base)?;
        put_len(&mut o, self.proposals.len())?;
        for p in &self.proposals {
            o.extend_from_slice(&p.hlc.to_bytes());
            o.extend_from_slice(&p.request_id.to_le_bytes());
            put_name(&mut o, &p.from)?;
            put_name(&mut o, &p.to)?;
            o.extend_from_slice(&p.actor.to_le_bytes());
        }
        put_len(&mut o, self.options.len())?;
        for opt in &self.options {
            put_option(&mut o, opt)?;
        }
        match &self.resolution {
            None => o.push(0),
            Some(r) => {
                o.push(1);
                put_option(&mut o, &r.choice)?;
                o.extend_from_slice(&r.actor.to_le_bytes());
            }
        }
        Ok(o)
    }

    /// Decodes only bytes that [`ConflictEntity::encode`] could have produced.
    pub fn decode(buf: &[u8]) -> Result<Self, ConflictCodecError> {
        let mut r = Reader { buf, pos: 0 };
        let v = r.u8()?;
        if v != VERSION {
            return Err(ConflictCodecError::Version(v));
        }
        let entity = u128::from_be_bytes(r.array()?);
        let ordinal = r.u32()?;
        if ordinal == 0 {
            return Err(ConflictCodecError::Invalid("ordinal"));
        }
        let base = r.name()?;
        let n = usize::from(r.u16()?);
        if !(2..=MAX_PROPOSALS).contains(&n) {
            return Err(ConflictCodecError::Invalid("proposal count"));
        }
        let mut proposals: Vec<Proposal> = Vec::new();
        for _ in 0..n {
            let hlc = Hlc::from_bytes(&r.array()?);
            let request_id = r.u64()?;
            let from = r.name()?;
            let to = r.name()?;
            let actor = r.u64()?;
            let p = Proposal {
                hlc,
                request_id,
                from,
                to,
                actor,
            };
            if proposals.last().is_some_and(|last| *last >= p) {
                return Err(ConflictCodecError::Invalid("proposal order"));
            }
            proposals.push(p);
        }
        let m = usize::from(r.u16()?);
        if m > MAX_PROPOSALS + 1 {
            return Err(ConflictCodecError::Invalid("option count"));
        }
        let mut options = Vec::new();
        for _ in 0..m {
            options.push(r.option()?);
        }
        if options != options_for(&proposals) {
            return Err(ConflictCodecError::Invalid("options"));
        }
        let resolution = match r.u8()? {
            0 => None,
            1 => {
                let choice = r.option()?;
                if !options.contains(&choice) {
                    return Err(ConflictCodecError::Invalid("resolution choice"));
                }
                Some(Resolution {
                    choice,
                    actor: r.u64()?,
                })
            }
            _ => return Err(ConflictCodecError::Invalid("resolution flag")),
        };
        r.finish()?;
        Ok(Self {
            entity,
            ordinal,
            base,
            proposals,
            options,
            resolution,
        })
    }
}

/// Why bytes are not a canonical conflict entity, or an entity cannot be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConflictCodecError {
    /// The bytes ended early.
    #[error("conflict truncated")]
    Truncated,
    /// A format version this code cannot read.
    #[error("unsupported conflict version {0}")]
    Version(u8),
    /// Bytes remained after a complete value.
    #[error("trailing bytes after conflict")]
    Trailing,
    /// A field holds an impossible or non-canonical value.
    #[error("invalid conflict field: {0}")]
    Invalid(&'static str),
}

fn put_len(o: &mut Vec<u8>, n: usize) -> Result<(), ConflictCodecError> {
    let n = u16::try_from(n).map_err(|_| ConflictCodecError::Invalid("length"))?;
    o.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

fn put_name(o: &mut Vec<u8>, s: &str) -> Result<(), ConflictCodecError> {
    if !valid_name(s) {
        return Err(ConflictCodecError::Invalid("name"));
    }
    put_len(o, s.len())?;
    o.extend_from_slice(s.as_bytes());
    Ok(())
}

fn put_option(o: &mut Vec<u8>, opt: &ConflictOption) -> Result<(), ConflictCodecError> {
    match opt {
        ConflictOption::Revert => o.push(0),
        ConflictOption::Choose(s) => {
            o.push(1);
            put_name(o, s)?;
        }
    }
    Ok(())
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ConflictCodecError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(ConflictCodecError::Truncated)?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or(ConflictCodecError::Truncated)?;
        self.pos = end;
        Ok(s)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ConflictCodecError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ConflictCodecError::Truncated)
    }
    fn u8(&mut self) -> Result<u8, ConflictCodecError> {
        Ok(u8::from_le_bytes(self.array()?))
    }
    fn u16(&mut self) -> Result<u16, ConflictCodecError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, ConflictCodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, ConflictCodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn name(&mut self) -> Result<String, ConflictCodecError> {
        let n = usize::from(self.u16()?);
        if n == 0 || n > MAX_NAME_LEN {
            return Err(ConflictCodecError::Invalid("name length"));
        }
        let bytes = self.take(n)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| ConflictCodecError::Invalid("name utf-8"))
    }
    fn option(&mut self) -> Result<ConflictOption, ConflictCodecError> {
        match self.u8()? {
            0 => Ok(ConflictOption::Revert),
            1 => Ok(ConflictOption::Choose(self.name()?)),
            _ => Err(ConflictCodecError::Invalid("option tag")),
        }
    }
    fn finish(&self) -> Result<(), ConflictCodecError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ConflictCodecError::Trailing)
        }
    }
}

/// Why a request was rejected. The [`RejectReason::code`] strings are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RejectReason {
    /// The request is for another entity than the history.
    WrongEntity,
    /// A state name is empty or too long.
    InvalidName,
    /// A state name is not in the workflow.
    UnknownState,
    /// The workflow has no such transition.
    NotAllowed,
    /// The transition's guard does not admit the actor.
    GuardDenied,
    /// The request id was already used for different content.
    RequestIdReused,
    /// The entity has an open conflict and the request is not based on its base state.
    EntityConflicted,
    /// The open conflict (or a new one) would exceed [`MAX_PROPOSALS`].
    TooManyProposals,
    /// The client named a state the entity has never been in.
    StateNeverObserved,
    /// The request is stale and a conflict resolution happened since.
    StaleAfterResolution,
    /// The request is stale and the entity is already in the requested state.
    AlreadyInTarget,
}

impl RejectReason {
    /// Stable machine-readable code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::WrongEntity => "wrong_entity",
            Self::InvalidName => "invalid_name",
            Self::UnknownState => "unknown_state",
            Self::NotAllowed => "not_allowed",
            Self::GuardDenied => "guard_denied",
            Self::RequestIdReused => "request_id_reused",
            Self::EntityConflicted => "entity_conflicted",
            Self::TooManyProposals => "too_many_proposals",
            Self::StateNeverObserved => "state_never_observed",
            Self::StaleAfterResolution => "stale_after_resolution",
            Self::AlreadyInTarget => "already_in_target",
        }
    }
}

/// The arbiter's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The transition was applied; the entity is now in `state`.
    Accepted {
        /// The new state.
        state: String,
    },
    /// The request was refused.
    Rejected(RejectReason),
    /// The request competes with another; the entity is held in the base state until resolved.
    Conflict(ConflictEntity),
}

/// Why [`History::resolve`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// There is no open conflict with that ordinal.
    #[error("no open conflict with that ordinal")]
    NoSuchConflict,
    /// The choice is not one of the conflict's options.
    #[error("choice is not an option of the conflict")]
    NotAnOption,
    /// The conflict was already closed with a different choice.
    #[error("conflict already resolved differently")]
    AlreadyResolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    state_after: String,
    /// `None` for a conflict resolution.
    request: Option<Request>,
    /// Accepted once, later folded into a conflict.
    superseded: bool,
}

/// Everything the arbiter knows about one entity. Cloneable and comparable so tests can check that
/// replays and duplicates leave it unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    entity: u128,
    initial: String,
    state: String,
    entries: Vec<Entry>,
    decided: BTreeMap<u64, (Request, Decision)>,
    open: Option<ConflictEntity>,
    closed: Vec<ConflictEntity>,
}

enum Effect {
    None,
    Accept(Request),
    NewConflict {
        conflict: ConflictEntity,
        from_index: usize,
    },
    Join(ConflictEntity),
}

impl History {
    /// A new entity in the workflow's initial state.
    pub fn new(entity: u128, def: &WorkflowDef) -> Self {
        Self {
            entity,
            initial: def.initial.clone(),
            state: def.initial.clone(),
            entries: Vec::new(),
            decided: BTreeMap::new(),
            open: None,
            closed: Vec::new(),
        }
    }

    /// The entity's current state (the base state while a conflict is open).
    pub fn state(&self) -> &str {
        &self.state
    }

    /// The unresolved conflict, if any.
    pub fn open_conflict(&self) -> Option<&ConflictEntity> {
        self.open.as_ref()
    }

    /// Resolved conflicts, oldest first.
    pub fn closed_conflicts(&self) -> &[ConflictEntity] {
        &self.closed
    }

    /// The stored decision for a request id, if it was decided.
    pub fn decision_for(&self, request_id: u64) -> Option<&Decision> {
        self.decided.get(&request_id).map(|(_, d)| d)
    }

    /// Ids of requests that were accepted and not later folded into a conflict, in order.
    pub fn applied_requests(&self) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|e| !e.superseded)
            .filter_map(|e| e.request.as_ref().map(|r| r.id))
            .collect()
    }

    /// Ids of accepted requests that a conflict later superseded.
    pub fn superseded_requests(&self) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|e| e.superseded)
            .filter_map(|e| e.request.as_ref().map(|r| r.id))
            .collect()
    }

    /// Decides the request and records the outcome.
    pub fn submit(&mut self, def: &WorkflowDef, req: &Request) -> Decision {
        let (decision, effect) = plan(def, self, req);
        match effect {
            Effect::None => {}
            Effect::Accept(r) => {
                self.state.clone_from(&r.to);
                self.entries.push(Entry {
                    state_after: r.to.clone(),
                    request: Some(r),
                    superseded: false,
                });
            }
            Effect::NewConflict {
                conflict,
                from_index,
            } => {
                for e in self.entries.iter_mut().skip(from_index) {
                    e.superseded = true;
                }
                self.state.clone_from(&conflict.base);
                self.open = Some(conflict);
            }
            Effect::Join(conflict) => self.open = Some(conflict),
        }
        if !matches!(decision, Decision::Rejected(RejectReason::RequestIdReused))
            && !self.decided.contains_key(&req.id)
        {
            self.decided.insert(req.id, (req.clone(), decision.clone()));
        }
        decision
    }

    /// Closes the open conflict `ordinal` with `choice`, decided by `actor`. Closing it again with
    /// the same choice returns the closed conflict unchanged.
    pub fn resolve(
        &mut self,
        ordinal: u32,
        choice: ConflictOption,
        actor: u64,
    ) -> Result<ConflictEntity, ResolveError> {
        let Some(open) = self.open.as_ref().filter(|c| c.ordinal == ordinal) else {
            return match self.closed.iter().find(|c| c.ordinal == ordinal) {
                Some(c) if c.resolution.as_ref().is_some_and(|r| r.choice == choice) => {
                    Ok(c.clone())
                }
                Some(_) => Err(ResolveError::AlreadyResolved),
                None => Err(ResolveError::NoSuchConflict),
            };
        };
        if !open.options.contains(&choice) {
            return Err(ResolveError::NotAnOption);
        }
        let state = match &choice {
            ConflictOption::Revert => open.base.clone(),
            ConflictOption::Choose(s) => s.clone(),
        };
        let mut done = open.clone();
        done.resolution = Some(Resolution { choice, actor });
        self.open = None;
        self.closed.push(done.clone());
        self.state.clone_from(&state);
        self.entries.push(Entry {
            state_after: state,
            request: None,
            superseded: false,
        });
        Ok(done)
    }
}

/// The pure arbiter: what `submit` would answer, without changing the history.
pub fn decide(def: &WorkflowDef, history: &History, req: &Request) -> Decision {
    plan(def, history, req).0
}

fn plan(def: &WorkflowDef, h: &History, req: &Request) -> (Decision, Effect) {
    let reject = |r| (Decision::Rejected(r), Effect::None);
    if let Some((orig, decision)) = h.decided.get(&req.id) {
        return if orig == req {
            (decision.clone(), Effect::None)
        } else {
            reject(RejectReason::RequestIdReused)
        };
    }
    if req.entity != h.entity {
        return reject(RejectReason::WrongEntity);
    }
    if !valid_name(&req.from) || !valid_name(&req.to) {
        return reject(RejectReason::InvalidName);
    }
    if !def.states.contains(&req.from) || !def.states.contains(&req.to) {
        return reject(RejectReason::UnknownState);
    }
    match def.transitions.get(&(req.from.clone(), req.to.clone())) {
        None => return reject(RejectReason::NotAllowed),
        Some(Guard::Actors(allowed)) if !allowed.contains(&req.actor) => {
            return reject(RejectReason::GuardDenied);
        }
        Some(_) => {}
    }
    if let Some(c) = &h.open {
        if req.from != c.base {
            return reject(RejectReason::EntityConflicted);
        }
        if c.proposals.len() >= MAX_PROPOSALS {
            return reject(RejectReason::TooManyProposals);
        }
        let mut set: BTreeSet<Proposal> = c.proposals.iter().cloned().collect();
        set.insert(Proposal::of(req));
        let joined = ConflictEntity::build(c.entity, c.ordinal, &c.base, set);
        return (Decision::Conflict(joined.clone()), Effect::Join(joined));
    }
    if req.from == h.state {
        return (
            Decision::Accepted {
                state: req.to.clone(),
            },
            Effect::Accept(req.clone()),
        );
    }
    // Stale: find where the client's state was reached; everything after it is "since".
    let start = match h.entries.iter().rposition(|e| e.state_after == req.from) {
        Some(i) => i + 1,
        None if req.from == h.initial => 0,
        None => return reject(RejectReason::StateNeverObserved),
    };
    let since = h.entries.get(start..).unwrap_or_default();
    if since.iter().any(|e| e.request.is_none()) {
        return reject(RejectReason::StaleAfterResolution);
    }
    if req.to == h.state {
        return reject(RejectReason::AlreadyInTarget);
    }
    let mut set: BTreeSet<Proposal> = since
        .iter()
        .filter(|e| !e.superseded)
        .filter_map(|e| e.request.as_ref().map(Proposal::of))
        .collect();
    set.insert(Proposal::of(req));
    if set.len() < 2 {
        return reject(RejectReason::StateNeverObserved);
    }
    if set.len() > MAX_PROPOSALS {
        return reject(RejectReason::TooManyProposals);
    }
    let ordinal = u32::try_from(h.closed.len())
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1);
    let conflict = ConflictEntity::build(h.entity, ordinal, &req.from, set);
    (
        Decision::Conflict(conflict.clone()),
        Effect::NewConflict {
            conflict,
            from_index: start,
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use proptest::prelude::*;

    const E: u128 = 7;
    const STATES: [&str; 4] = ["todo", "doing", "blocked", "done"];

    fn def() -> WorkflowDef {
        let mut d = WorkflowDef::new("todo", STATES).unwrap();
        d.allow("todo", "doing", Guard::Open).unwrap();
        d.allow("todo", "blocked", Guard::Open).unwrap();
        d.allow("todo", "done", Guard::Open).unwrap();
        d.allow("blocked", "doing", Guard::Open).unwrap();
        d.allow("doing", "todo", Guard::Open).unwrap();
        d.allow("doing", "done", Guard::Actors([1, 2].into()))
            .unwrap();
        d
    }

    fn req(id: u64, from: &str, to: &str, actor: u64, wall: u64) -> Request {
        Request {
            id,
            entity: E,
            from: from.into(),
            to: to.into(),
            actor,
            hlc: Hlc {
                wall_ms: wall,
                counter: 0,
                node: actor as u32,
            },
        }
    }

    #[test]
    fn fresh_transition_is_accepted() {
        let d = def();
        let mut h = History::new(E, &d);
        let r = h.submit(&d, &req(1, "todo", "doing", 1, 10));
        assert_eq!(
            r,
            Decision::Accepted {
                state: "doing".into()
            }
        );
        assert_eq!(h.state(), "doing");
    }

    #[test]
    fn invalid_transitions_carry_stable_codes() {
        let d = def();
        let mut h = History::new(E, &d);
        let code = |h: &mut History, r: Request| match h.submit(&d, &r) {
            Decision::Rejected(c) => c.code(),
            other => panic!("expected rejection, got {other:?}"),
        };
        assert_eq!(code(&mut h, req(1, "blocked", "done", 1, 1)), "not_allowed");
        assert_eq!(code(&mut h, req(2, "todo", "nope", 1, 1)), "unknown_state");
        assert_eq!(code(&mut h, req(3, "", "doing", 1, 1)), "invalid_name");
        let mut other = req(4, "todo", "doing", 1, 1);
        other.entity = 8;
        assert_eq!(code(&mut h, other), "wrong_entity");
        h.submit(&d, &req(5, "todo", "doing", 1, 2));
        assert_eq!(code(&mut h, req(6, "doing", "done", 9, 3)), "guard_denied");
        assert_eq!(h.state(), "doing");
    }

    #[test]
    fn two_replicas_offline_conflict_identically_in_both_orders() {
        let d = def();
        let a = req(1, "todo", "doing", 1, 100);
        let b = req(2, "todo", "blocked", 2, 90);
        let run = |first: &Request, second: &Request| {
            let mut h = History::new(E, &d);
            let d1 = h.submit(&d, first);
            let d2 = h.submit(&d, second);
            (h, d1, d2)
        };
        let (h1, x1, y1) = run(&a, &b);
        let (h2, x2, y2) = run(&b, &a);
        assert!(matches!(x1, Decision::Accepted { .. }));
        assert!(matches!(x2, Decision::Accepted { .. }));
        let (Decision::Conflict(c1), Decision::Conflict(c2)) = (y1, y2) else {
            panic!("second arrival must conflict");
        };
        assert_eq!(c1, c2);
        assert_eq!(c1.encode().unwrap(), c2.encode().unwrap());
        assert_eq!(c1.base, "todo");
        // sorted by hlc: b (90) before a (100)
        assert_eq!(c1.proposals[0].request_id, 2);
        assert_eq!(c1.proposals[1].request_id, 1);
        assert_eq!(
            c1.options,
            vec![
                ConflictOption::Revert,
                ConflictOption::Choose("blocked".into()),
                ConflictOption::Choose("doing".into()),
            ]
        );
        assert_eq!(h1.state(), "todo");
        assert_eq!(h2.state(), "todo");
        assert_eq!(h1.open_conflict(), h2.open_conflict());
        assert_eq!(h1.superseded_requests().len(), 1);
    }

    #[test]
    fn resolution_closes_conflict_and_unblocks_entity() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", "doing", 1, 100));
        h.submit(&d, &req(2, "todo", "blocked", 2, 90));
        // while open, an unrelated-base request is rejected
        let r = h.submit(&d, &req(3, "doing", "done", 1, 110));
        assert_eq!(r, Decision::Rejected(RejectReason::EntityConflicted));
        assert_eq!(
            h.resolve(1, ConflictOption::Choose("done".into()), 5),
            Err(ResolveError::NotAnOption)
        );
        assert_eq!(
            h.resolve(9, ConflictOption::Revert, 5),
            Err(ResolveError::NoSuchConflict)
        );
        let closed = h
            .resolve(1, ConflictOption::Choose("blocked".into()), 5)
            .unwrap();
        assert_eq!(closed.resolution.as_ref().unwrap().actor, 5);
        assert_eq!(h.state(), "blocked");
        assert!(h.open_conflict().is_none());
        // idempotent close, but not a different one
        assert_eq!(
            h.resolve(1, ConflictOption::Choose("blocked".into()), 6)
                .unwrap(),
            closed
        );
        assert_eq!(
            h.resolve(1, ConflictOption::Revert, 6),
            Err(ResolveError::AlreadyResolved)
        );
        // normal work resumes
        let r = h.submit(&d, &req(4, "blocked", "doing", 1, 120));
        assert_eq!(
            r,
            Decision::Accepted {
                state: "doing".into()
            }
        );
        // a request made before the conflict, arriving after the resolution
        let r = h.submit(&d, &req(5, "todo", "done", 3, 95));
        assert_eq!(r, Decision::Rejected(RejectReason::StaleAfterResolution));
    }

    #[test]
    fn third_proposal_joins_open_conflict() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", "doing", 1, 100));
        h.submit(&d, &req(2, "todo", "blocked", 2, 90));
        let Decision::Conflict(c) = h.submit(&d, &req(3, "todo", "done", 3, 95)) else {
            panic!("expected conflict");
        };
        assert_eq!(c.proposals.len(), 3);
        assert_eq!(c.ordinal, 1);
        assert_eq!(h.open_conflict(), Some(&c));
    }

    #[test]
    fn stale_request_for_state_already_reached_is_rejected_not_lost() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", "doing", 1, 10));
        let r = h.submit(&d, &req(2, "todo", "doing", 2, 11));
        assert_eq!(r, Decision::Rejected(RejectReason::AlreadyInTarget));
        let r = h.submit(&d, &req(3, "blocked", "doing", 2, 11));
        assert_eq!(r, Decision::Rejected(RejectReason::StateNeverObserved));
    }

    #[test]
    fn duplicates_are_idempotent_and_id_reuse_is_rejected() {
        let d = def();
        let mut h = History::new(E, &d);
        let a = req(1, "todo", "doing", 1, 100);
        let first = h.submit(&d, &a);
        let snapshot = h.clone();
        assert_eq!(h.submit(&d, &a), first);
        assert_eq!(h, snapshot);
        let clash = req(1, "todo", "blocked", 1, 100);
        assert_eq!(
            h.submit(&d, &clash),
            Decision::Rejected(RejectReason::RequestIdReused)
        );
        assert_eq!(h, snapshot);
        assert_eq!(decide(&d, &h, &a), first);
    }

    #[test]
    fn decide_does_not_change_history() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", "doing", 1, 100));
        let snapshot = h.clone();
        let _ = decide(&d, &h, &req(2, "todo", "blocked", 2, 90));
        assert_eq!(h, snapshot);
    }

    #[test]
    fn definition_rejects_bad_input() {
        assert_eq!(
            WorkflowDef::new("", ["a"]).err(),
            Some(DefError::InvalidName)
        );
        let mut d = WorkflowDef::new("a", ["b"]).unwrap();
        assert_eq!(
            d.allow("a", "zz", Guard::Open),
            Err(DefError::UnknownState("zz".into()))
        );
    }

    #[test]
    fn conflict_codec_roundtrips_and_is_strict() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", "doing", 1, 100));
        let Decision::Conflict(c) = h.submit(&d, &req(2, "todo", "blocked", 2, 90)) else {
            panic!("expected conflict");
        };
        let bytes = c.encode().unwrap();
        assert_eq!(ConflictEntity::decode(&bytes).unwrap(), c);
        let closed = h.resolve(1, ConflictOption::Revert, 3).unwrap();
        assert_eq!(
            ConflictEntity::decode(&closed.encode().unwrap()).unwrap(),
            closed
        );
        for cut in 0..bytes.len() {
            assert!(ConflictEntity::decode(&bytes[..cut]).is_err());
        }
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(
            ConflictEntity::decode(&long),
            Err(ConflictCodecError::Trailing)
        );
        let mut bad = bytes;
        bad[0] = 9;
        assert_eq!(
            ConflictEntity::decode(&bad),
            Err(ConflictCodecError::Version(9))
        );
    }

    #[derive(Debug, Clone)]
    enum Op {
        Submit {
            id: u64,
            from: usize,
            to: usize,
            actor: u64,
            wall: u64,
        },
        Resolve {
            pick: usize,
            actor: u64,
        },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0u64..12, 0usize..4, 0usize..4, 1u64..4, 0u64..50).prop_map(
                |(id, from, to, actor, wall)| Op::Submit { id, from, to, actor, wall }
            ),
            1 => (0usize..6, 1u64..4).prop_map(|(pick, actor)| Op::Resolve { pick, actor }),
        ]
    }

    fn run(d: &WorkflowDef, ops: &[Op], double: bool) -> (History, Vec<Option<Decision>>) {
        let mut h = History::new(E, d);
        let mut out = Vec::new();
        for o in ops {
            match o {
                Op::Submit {
                    id,
                    from,
                    to,
                    actor,
                    wall,
                } => {
                    let r = req(*id, STATES[*from], STATES[*to], *actor, *wall);
                    let first = h.submit(d, &r);
                    if double {
                        let before = h.clone();
                        assert_eq!(h.submit(d, &r), first, "duplicate changed the answer");
                        assert_eq!(h, before, "duplicate changed the history");
                    }
                    out.push(Some(first));
                }
                Op::Resolve { pick, actor } => {
                    if let Some(c) = h.open_conflict().cloned() {
                        let choice = c.options[pick % c.options.len()].clone();
                        h.resolve(c.ordinal, choice, *actor).unwrap();
                    }
                    out.push(None);
                }
            }
        }
        (h, out)
    }

    proptest! {
        #[test]
        fn replay_is_deterministic_and_duplicates_are_idempotent(
            ops in proptest::collection::vec(op(), 0..40)
        ) {
            let d = def();
            let (h1, o1) = run(&d, &ops, false);
            let (h2, o2) = run(&d, &ops, false);
            prop_assert_eq!(&h1, &h2);
            prop_assert_eq!(&o1, &o2);
            // resubmitting every request right away changes nothing
            let (h3, o3) = run(&d, &ops, true);
            prop_assert_eq!(&h1, &h3);
            prop_assert_eq!(&o1, &o3);
        }

        #[test]
        fn no_proposal_is_dropped_silently(ops in proptest::collection::vec(op(), 0..40)) {
            let d = def();
            let (h, out) = run(&d, &ops, false);
            let all_conflicts: Vec<&ConflictEntity> =
                h.closed_conflicts().iter().chain(h.open_conflict()).collect();
            let listed = |id: u64| all_conflicts.iter().any(|c| c.proposals.iter().any(|p| p.request_id == id));
            let applied = h.applied_requests();
            for (o, dec) in ops.iter().zip(&out) {
                if let (Op::Submit { id, .. }, Some(dec)) = (o, dec) {
                    match dec {
                        Decision::Rejected(r) => prop_assert!(!r.code().is_empty()),
                        Decision::Conflict(_) => prop_assert!(listed(*id)),
                        Decision::Accepted { .. } => {
                            prop_assert!(applied.contains(id) || listed(*id));
                        }
                    }
                }
            }
            for id in h.superseded_requests() {
                prop_assert!(listed(id), "superseded request {} not in any conflict", id);
            }
            // every conflict round-trips through the canonical encoding
            for c in all_conflicts {
                prop_assert_eq!(&ConflictEntity::decode(&c.encode().unwrap()).unwrap(), c);
            }
        }

        #[test]
        fn two_stale_proposals_converge_in_any_arrival_order(
            to_a in 1usize..4, to_b in 1usize..4, wa in 0u64..20, wb in 0u64..20
        ) {
            let d = def();
            let a = req(1, "todo", STATES[to_a], 1, wa);
            let b = req(2, "todo", STATES[to_b], 2, wb);
            let mut h1 = History::new(E, &d);
            h1.submit(&d, &a);
            h1.submit(&d, &b);
            let mut h2 = History::new(E, &d);
            h2.submit(&d, &b);
            h2.submit(&d, &a);
            prop_assert_eq!(h1.state(), h2.state());
            prop_assert_eq!(h1.open_conflict(), h2.open_conflict());
        }

        #[test]
        fn decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            if let Ok(c) = ConflictEntity::decode(&bytes) {
                prop_assert_eq!(c.encode().unwrap(), bytes);
            }
        }

        #[test]
        fn decoder_survives_mutated_valid_bytes(pos in 0usize..200, byte in any::<u8>()) {
            let d = def();
            let mut h = History::new(E, &d);
            h.submit(&d, &req(1, "todo", "doing", 1, 100));
            let Decision::Conflict(c) = h.submit(&d, &req(2, "todo", "blocked", 2, 90)) else {
                panic!("expected conflict");
            };
            let mut bytes = c.encode().unwrap();
            let n = bytes.len();
            bytes[pos % n] = byte;
            if let Ok(c2) = ConflictEntity::decode(&bytes) {
                prop_assert_eq!(c2.encode().unwrap(), bytes);
            }
        }
    }
}
