//! Server-arbitrated workflow state (WP47, P3-09).
//!
//! Workflow state is not merged like a CRDT: two replicas that change the state of one entity while
//! apart cannot both be right, and merging would silently pick one. Instead the server arbitrates.
//! Every state change arrives as a [`Request`] that names the state the client saw (`from`) and the
//! entity version it saw it at (`from_version`), and a pure function ([`decide`]) answers with
//! exactly one of [`Decision::Accepted`], [`Decision::Rejected`] (with a stable [`RejectReason`]
//! code) or [`Decision::Conflict`] (a [`ConflictEntity`] that lists every competing proposal and the
//! ways to resolve it).
//!
//! # Rules
//!
//! 1. A request is keyed by `(actor, id)`. Re-sending an accepted or conflicting request returns the
//!    stored answer and changes nothing; re-using the key for different content is rejected.
//!    Rejections are not stored (so they cannot be used to fill the history); a re-sent rejected
//!    request is simply decided again against the current state. **`Request.actor` must be bound to
//!    the authenticated principal by the caller**: this module trusts it, and keying by actor only
//!    stops one principal from burning another's ids if that holds.
//! 2. Names must be known to the [`WorkflowDef`], the transition must be allowed and its [`Guard`]
//!    must admit the actor, otherwise the request is rejected.
//! 3. The entity version counts every accepted transition and every resolution, so it is exact:
//!    a request is current only if `from_version` equals the version, which settles the
//!    `todo -> doing -> todo` case where the state alone cannot tell stale from fresh. The state at
//!    `from_version` must equal `from`, else the request is rejected.
//! 4. A request that observed a state later folded into a conflict is rejected
//!    ([`RejectReason::StateSuperseded`]).
//! 5. If the entity has an open conflict, a request based on the conflict's base state joins the
//!    conflict as another proposal (up to [`MAX_PROPOSALS`]); anything else is rejected until the
//!    conflict is resolved.
//! 6. A current request is accepted. A stale one means another transition was accepted since the
//!    client looked, so it becomes a conflict, unless it asks for the state the entity is already in
//!    ([`RejectReason::AlreadyInTarget`]) or a resolution happened since
//!    ([`RejectReason::StaleAfterResolution`]).
//! 7. A new conflict holds the transitions accepted since the observed version (marked superseded)
//!    together with the stale request. The entity returns to the base state and stays there until
//!    [`History::resolve`] closes the conflict.
//! 8. [`History::resolve`] needs the resolver to be listed in the definition
//!    ([`WorkflowDef::set_resolvers`]; nobody by default), and a `Choose(s)` also needs `base -> s`
//!    to be an allowed transition whose guard admits the resolver.
//! 9. History per entity is bounded ([`MAX_ENTRIES`], [`MAX_DECIDED`]). When full, new transitions
//!    are rejected with [`RejectReason::HistoryFull`]; one slot is kept so an open conflict can
//!    always be resolved. Nothing is pruned, so idempotency never silently lapses.
//!
//! # What converges
//!
//! Proposals made from the same base state converge: the conflict is a sorted set, so the same
//! conflict and base state result from any arrival order, for two proposals or for many, up to
//! [`MAX_PROPOSALS`]. Beyond the cap the earliest arrivals win and later ones are rejected with
//! `too_many_proposals`, so that case depends on arrival order. A chain (a request made after
//! another accepted one, then a stale request from the older state) is also order dependent: the
//! chained request is rejected with a code when the stale one arrives first. In every case each
//! proposal is accepted, rejected with a code, or recorded in a conflict; none is dropped silently.
//!
//! The state machine reads no clock and does no I/O, and uses ordered collections only, so
//! replaying the same ordered requests always gives the same result.

use lattice_abi::Hlc;
use std::collections::{BTreeMap, BTreeSet};

/// Longest state name, in bytes.
pub const MAX_NAME_LEN: usize = 64;
/// Most states a definition may hold.
pub const MAX_STATES: usize = 256;
/// Most proposals one conflict may hold.
pub const MAX_PROPOSALS: usize = 16;
/// Most history entries (accepted transitions and resolutions) one entity may hold.
pub const MAX_ENTRIES: usize = 4096;
/// Most stored decisions (accepted requests and conflict proposals) one entity may hold.
pub const MAX_DECIDED: usize = 16384;

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
    resolvers: BTreeSet<u64>,
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
            resolvers: BTreeSet::new(),
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

    /// Sets who may resolve conflicts. Nobody may until this is called.
    pub fn set_resolvers(&mut self, actors: impl IntoIterator<Item = u64>) {
        self.resolvers = actors.into_iter().collect();
    }

    /// The state every entity starts in.
    pub fn initial(&self) -> &str {
        &self.initial
    }
}

/// A client's request to change the state of one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Unique per `(actor, id)`; makes duplicates idempotent.
    pub id: u64,
    /// The entity whose state changes.
    pub entity: u128,
    /// The state the client saw when it made the request.
    pub from: String,
    /// The entity version the client saw `from` at (see [`History::version`]).
    pub from_version: u64,
    /// The state the client wants.
    pub to: String,
    /// Who asks. The caller must bind this to the authenticated principal.
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

    /// Checks everything `decode` demands: ordinal at least 1, a valid base, 2 to
    /// [`MAX_PROPOSALS`] strictly sorted proposals with valid names, options equal to the ones
    /// derived from the proposals, and a resolution choice that is one of the options.
    pub fn validate(&self) -> Result<(), ConflictCodecError> {
        if self.ordinal == 0 || !valid_name(&self.base) {
            return Err(ConflictCodecError::Invalid("base or ordinal"));
        }
        if !(2..=MAX_PROPOSALS).contains(&self.proposals.len()) {
            return Err(ConflictCodecError::Invalid("proposal count"));
        }
        if self
            .proposals
            .iter()
            .any(|p| !valid_name(&p.from) || !valid_name(&p.to))
        {
            return Err(ConflictCodecError::Invalid("name"));
        }
        if self.proposals.windows(2).any(|w| match w {
            [a, b] => a >= b,
            _ => false,
        }) {
            return Err(ConflictCodecError::Invalid("proposal order"));
        }
        if self.options != options_for(&self.proposals) {
            return Err(ConflictCodecError::Invalid("options"));
        }
        if let Some(r) = &self.resolution
            && !self.options.contains(&r.choice)
        {
            return Err(ConflictCodecError::Invalid("resolution choice"));
        }
        Ok(())
    }

    /// Canonical bytes: the same conflict always encodes to the same bytes.
    pub fn encode(&self) -> Result<Vec<u8>, ConflictCodecError> {
        self.validate()?;
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
        let c = Self {
            entity,
            ordinal,
            base,
            proposals,
            options,
            resolution,
        };
        c.validate()?;
        Ok(c)
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
    /// The `(actor, id)` key was already used for different content.
    RequestIdReused,
    /// The entity has an open conflict and the request is not based on its base state.
    EntityConflicted,
    /// The open conflict (or a new one) would exceed [`MAX_PROPOSALS`].
    TooManyProposals,
    /// `from_version` is in the future, or the state at that version is not `from`.
    StateNeverObserved,
    /// The client observed a state that a conflict later folded away.
    StateSuperseded,
    /// The request is stale and a conflict resolution happened since.
    StaleAfterResolution,
    /// The request is stale and the entity is already in the requested state.
    AlreadyInTarget,
    /// The entity's history reached [`MAX_ENTRIES`] or [`MAX_DECIDED`].
    HistoryFull,
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
            Self::StateSuperseded => "state_superseded",
            Self::StaleAfterResolution => "stale_after_resolution",
            Self::AlreadyInTarget => "already_in_target",
            Self::HistoryFull => "history_full",
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
    /// The actor is not a resolver, or the guard of the chosen transition does not admit them.
    #[error("actor may not resolve this conflict that way")]
    NotPermitted,
    /// The workflow has no transition from the base state to the chosen state.
    #[error("the chosen transition is not allowed from the base state")]
    TransitionNotAllowed,
    /// The history is full.
    #[error("history is full")]
    HistoryFull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    state_after: String,
    /// `None` for a conflict resolution.
    request: Option<Request>,
    /// Accepted once, later folded into a conflict.
    superseded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stored {
    Accepted,
    Conflict(u32),
}

/// Everything the arbiter knows about one entity. Cloneable and comparable so tests can check that
/// replays and duplicates leave it unchanged. Memory is bounded by [`MAX_ENTRIES`] and
/// [`MAX_DECIDED`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    entity: u128,
    initial: String,
    state: String,
    entries: Vec<Entry>,
    decided: BTreeMap<(u64, u64), (Request, Stored)>,
    open: Option<ConflictEntity>,
    closed: Vec<ConflictEntity>,
}

enum Effect {
    None,
    Accept(Request),
    NewConflict {
        conflict: ConflictEntity,
        from_index: usize,
        by: Request,
    },
    Join(ConflictEntity, Request),
}

impl History {
    /// A new entity in the workflow's initial state, at version 0.
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

    /// The entity version: the number of accepted transitions and resolutions so far. Clients put
    /// the version they saw in [`Request::from_version`].
    pub fn version(&self) -> u64 {
        u64::try_from(self.entries.len()).unwrap_or(u64::MAX)
    }

    /// The state the entity had at `version` (0 is the initial state), if that version exists.
    pub fn state_at(&self, version: u64) -> Option<&str> {
        if version == 0 {
            return Some(&self.initial);
        }
        let i = usize::try_from(version - 1).ok()?;
        self.entries.get(i).map(|e| e.state_after.as_str())
    }

    /// The unresolved conflict, if any.
    pub fn open_conflict(&self) -> Option<&ConflictEntity> {
        self.open.as_ref()
    }

    /// Resolved conflicts, oldest first.
    pub fn closed_conflicts(&self) -> &[ConflictEntity] {
        &self.closed
    }

    /// `(actor, id)` of requests that were accepted and not later folded into a conflict, in order.
    pub fn applied_requests(&self) -> Vec<(u64, u64)> {
        self.entries
            .iter()
            .filter(|e| !e.superseded)
            .filter_map(|e| e.request.as_ref().map(|r| (r.actor, r.id)))
            .collect()
    }

    /// `(actor, id)` of accepted requests that a conflict later superseded.
    pub fn superseded_requests(&self) -> Vec<(u64, u64)> {
        self.entries
            .iter()
            .filter(|e| e.superseded)
            .filter_map(|e| e.request.as_ref().map(|r| (r.actor, r.id)))
            .collect()
    }

    /// Number of stored decisions, for bound checks.
    pub fn decided_len(&self) -> usize {
        self.decided.len()
    }

    /// Number of history entries, for bound checks.
    pub fn entries_len(&self) -> usize {
        self.entries.len()
    }

    /// Decides the request and records the outcome.
    pub fn submit(&mut self, def: &WorkflowDef, req: &Request) -> Decision {
        let (decision, effect) = plan(def, self, req);
        let key = (req.actor, req.id);
        match effect {
            Effect::None => {}
            Effect::Accept(r) => {
                self.state.clone_from(&r.to);
                self.entries.push(Entry {
                    state_after: r.to.clone(),
                    request: Some(r.clone()),
                    superseded: false,
                });
                self.decided.insert(key, (r, Stored::Accepted));
            }
            Effect::NewConflict {
                conflict,
                from_index,
                by,
            } => {
                for e in self.entries.iter_mut().skip(from_index) {
                    e.superseded = true;
                }
                self.state.clone_from(&conflict.base);
                self.decided
                    .insert(key, (by, Stored::Conflict(conflict.ordinal)));
                self.open = Some(conflict);
            }
            Effect::Join(conflict, by) => {
                self.decided
                    .insert(key, (by, Stored::Conflict(conflict.ordinal)));
                self.open = Some(conflict);
            }
        }
        decision
    }

    /// Closes the open conflict `ordinal` with `choice`, decided by `actor`.
    ///
    /// The actor must be one of the definition's resolvers. `Choose(s)` also needs `base -> s` to be
    /// an allowed transition whose guard admits the actor, so resolving cannot reach a state the
    /// workflow would not let the actor reach. `Revert` returns to the base state. Closing a
    /// conflict again with the same choice returns the closed conflict unchanged.
    pub fn resolve(
        &mut self,
        def: &WorkflowDef,
        ordinal: u32,
        choice: ConflictOption,
        actor: u64,
    ) -> Result<ConflictEntity, ResolveError> {
        if !def.resolvers.contains(&actor) {
            return Err(ResolveError::NotPermitted);
        }
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
            ConflictOption::Choose(s) => {
                match def.transitions.get(&(open.base.clone(), s.clone())) {
                    None => return Err(ResolveError::TransitionNotAllowed),
                    Some(Guard::Actors(a)) if !a.contains(&actor) => {
                        return Err(ResolveError::NotPermitted);
                    }
                    Some(_) => s.clone(),
                }
            }
        };
        if self.entries.len() >= MAX_ENTRIES {
            return Err(ResolveError::HistoryFull);
        }
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
    if let Some((orig, stored)) = h.decided.get(&(req.actor, req.id)) {
        if orig != req {
            return reject(RejectReason::RequestIdReused);
        }
        let found = match stored {
            Stored::Accepted => Some(Decision::Accepted {
                state: orig.to.clone(),
            }),
            Stored::Conflict(n) => h
                .open
                .iter()
                .chain(h.closed.iter())
                .find(|c| c.ordinal == *n)
                .map(|c| Decision::Conflict(c.clone())),
        };
        return match found {
            Some(d) => (d, Effect::None),
            None => reject(RejectReason::StateNeverObserved),
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
    if h.state_at(req.from_version) != Some(req.from.as_str()) {
        return reject(RejectReason::StateNeverObserved);
    }
    let start = usize::try_from(req.from_version).unwrap_or(usize::MAX);
    if let Some(prev) = start.checked_sub(1).and_then(|i| h.entries.get(i))
        && prev.superseded
    {
        return reject(RejectReason::StateSuperseded);
    }
    if h.decided.len() >= MAX_DECIDED {
        return reject(RejectReason::HistoryFull);
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
        return (
            Decision::Conflict(joined.clone()),
            Effect::Join(joined, req.clone()),
        );
    }
    if start == h.entries.len() {
        // One slot stays free so a later conflict can always be resolved.
        if h.entries.len() + 2 > MAX_ENTRIES {
            return reject(RejectReason::HistoryFull);
        }
        return (
            Decision::Accepted {
                state: req.to.clone(),
            },
            Effect::Accept(req.clone()),
        );
    }
    // Stale: everything after the observed version is "since".
    let since = h.entries.get(start..).unwrap_or_default();
    if since.iter().any(|e| e.request.is_none()) {
        return reject(RejectReason::StaleAfterResolution);
    }
    if req.to == h.state {
        return reject(RejectReason::AlreadyInTarget);
    }
    if h.entries.len() + 1 > MAX_ENTRIES {
        return reject(RejectReason::HistoryFull);
    }
    let mut set: BTreeSet<Proposal> = since
        .iter()
        .filter(|e| !e.superseded)
        .filter_map(|e| e.request.as_ref().map(Proposal::of))
        .collect();
    set.insert(Proposal::of(req));
    if set.len() < 2 {
        return reject(RejectReason::StaleAfterResolution);
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
            by: req.clone(),
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
    const RESOLVER: u64 = 5;

    fn def() -> WorkflowDef {
        let mut d = WorkflowDef::new("todo", STATES).unwrap();
        d.allow("todo", "doing", Guard::Open).unwrap();
        d.allow("todo", "blocked", Guard::Open).unwrap();
        d.allow("todo", "done", Guard::Open).unwrap();
        d.allow("blocked", "doing", Guard::Open).unwrap();
        d.allow("doing", "todo", Guard::Open).unwrap();
        d.allow("doing", "done", Guard::Actors([1, 2].into()))
            .unwrap();
        d.set_resolvers([RESOLVER, 6]);
        d
    }

    /// A request: id, state seen, version seen, wanted state, actor, wall clock.
    fn req(id: u64, from: &str, ver: u64, to: &str, actor: u64, wall: u64) -> Request {
        Request {
            id,
            entity: E,
            from: from.into(),
            from_version: ver,
            to: to.into(),
            actor,
            hlc: Hlc {
                wall_ms: wall,
                counter: 0,
                node: actor as u32,
            },
        }
    }

    fn rejected(d: Decision) -> &'static str {
        match d {
            Decision::Rejected(c) => c.code(),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    /// The two-replica scenario: both started from `todo` at version 0.
    fn offline_pair() -> (Request, Request) {
        (
            req(1, "todo", 0, "doing", 1, 100),
            req(2, "todo", 0, "blocked", 2, 90),
        )
    }

    #[test]
    fn fresh_transition_is_accepted() {
        let d = def();
        let mut h = History::new(E, &d);
        let r = h.submit(&d, &req(1, "todo", 0, "doing", 1, 10));
        assert_eq!(
            r,
            Decision::Accepted {
                state: "doing".into()
            }
        );
        assert_eq!((h.state(), h.version()), ("doing", 1));
        assert_eq!(h.state_at(0), Some("todo"));
        assert_eq!(h.state_at(2), None);
    }

    #[test]
    fn invalid_transitions_carry_stable_codes() {
        let d = def();
        let mut h = History::new(E, &d);
        assert_eq!(
            rejected(h.submit(&d, &req(1, "blocked", 0, "done", 1, 1))),
            "not_allowed"
        );
        assert_eq!(
            rejected(h.submit(&d, &req(2, "todo", 0, "nope", 1, 1))),
            "unknown_state"
        );
        assert_eq!(
            rejected(h.submit(&d, &req(3, "", 0, "doing", 1, 1))),
            "invalid_name"
        );
        let mut other = req(4, "todo", 0, "doing", 1, 1);
        other.entity = 8;
        assert_eq!(rejected(h.submit(&d, &other)), "wrong_entity");
        // wrong version for the state named, and a version from the future
        assert_eq!(
            rejected(h.submit(&d, &req(7, "doing", 0, "todo", 1, 1))),
            "state_never_observed"
        );
        assert_eq!(
            rejected(h.submit(&d, &req(8, "todo", 3, "doing", 1, 1))),
            "state_never_observed"
        );
        h.submit(&d, &req(5, "todo", 0, "doing", 1, 2));
        assert_eq!(
            rejected(h.submit(&d, &req(6, "doing", 1, "done", 9, 3))),
            "guard_denied"
        );
        assert_eq!(h.state(), "doing");
    }

    #[test]
    fn two_replicas_offline_conflict_identically_in_both_orders() {
        let d = def();
        let (a, b) = offline_pair();
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
        assert_eq!((h1.state(), h2.state()), ("todo", "todo"));
        assert_eq!(h1.open_conflict(), h2.open_conflict());
        assert_eq!(h1.superseded_requests().len(), 1);
    }

    #[test]
    fn resolution_closes_conflict_and_unblocks_entity() {
        let d = def();
        let mut h = History::new(E, &d);
        let (a, b) = offline_pair();
        h.submit(&d, &a);
        h.submit(&d, &b);
        // while open, a request based on another state is rejected
        assert_eq!(
            rejected(h.submit(&d, &req(3, "blocked", 1, "doing", 1, 110))),
            "state_never_observed"
        );
        assert_eq!(
            h.resolve(&d, 1, ConflictOption::Choose("done".into()), RESOLVER),
            Err(ResolveError::NotAnOption)
        );
        assert_eq!(
            h.resolve(&d, 9, ConflictOption::Revert, RESOLVER),
            Err(ResolveError::NoSuchConflict)
        );
        let closed = h
            .resolve(&d, 1, ConflictOption::Choose("blocked".into()), RESOLVER)
            .unwrap();
        assert_eq!(closed.resolution.as_ref().unwrap().actor, RESOLVER);
        assert_eq!(h.state(), "blocked");
        assert!(h.open_conflict().is_none());
        // idempotent close, but not a different one
        assert_eq!(
            h.resolve(&d, 1, ConflictOption::Choose("blocked".into()), 6)
                .unwrap(),
            closed
        );
        assert_eq!(
            h.resolve(&d, 1, ConflictOption::Revert, 6),
            Err(ResolveError::AlreadyResolved)
        );
        // normal work resumes at the new version
        let r = h.submit(&d, &req(4, "blocked", h.version(), "doing", 1, 120));
        assert_eq!(
            r,
            Decision::Accepted {
                state: "doing".into()
            }
        );
        // a request made before the conflict, arriving after the resolution
        let late = req(5, "todo", 0, "done", 3, 95);
        assert_eq!(rejected(h.submit(&d, &late)), "stale_after_resolution");
    }

    #[test]
    fn resolution_cannot_bypass_permissions_or_guards() {
        let mut d = def();
        // todo -> done is guarded to actor 1 only
        d.allow("todo", "done", Guard::Actors([1].into())).unwrap();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", 0, "done", 1, 10));
        let Decision::Conflict(c) = h.submit(&d, &req(2, "todo", 0, "doing", 2, 11)) else {
            panic!("expected conflict");
        };
        // anyone who is not a resolver, including a proposer, is refused
        for who in [1, 2, 99] {
            assert_eq!(
                h.resolve(&d, c.ordinal, ConflictOption::Revert, who),
                Err(ResolveError::NotPermitted)
            );
        }
        // a resolver the guard does not admit cannot reach the guarded state
        let done = ConflictOption::Choose("done".into());
        assert_eq!(
            h.resolve(&d, c.ordinal, done.clone(), RESOLVER),
            Err(ResolveError::NotPermitted)
        );
        assert!(h.open_conflict().is_some());
        // a definition that no longer allows the transition refuses it
        let mut stricter = def();
        stricter
            .transitions
            .remove(&("todo".into(), "doing".into()));
        assert_eq!(
            h.resolve(
                &stricter,
                c.ordinal,
                ConflictOption::Choose("doing".into()),
                RESOLVER
            ),
            Err(ResolveError::TransitionNotAllowed)
        );
        // a resolver the guard admits may choose it
        let mut d2 = d.clone();
        d2.set_resolvers([RESOLVER, 1]);
        assert!(h.resolve(&d2, c.ordinal, done, 1).is_ok());
        assert_eq!(h.state(), "done");
        // nobody can resolve when no resolvers are configured
        let mut none = d.clone();
        none.set_resolvers([]);
        let mut h2 = History::new(E, &none);
        h2.submit(&none, &req(1, "todo", 0, "doing", 1, 10));
        h2.submit(&none, &req(2, "todo", 0, "blocked", 2, 11));
        assert_eq!(
            h2.resolve(&none, 1, ConflictOption::Revert, 1),
            Err(ResolveError::NotPermitted)
        );
    }

    #[test]
    fn aba_return_to_same_state_is_still_stale() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", 0, "doing", 1, 10));
        h.submit(&d, &req(2, "doing", 1, "todo", 1, 11));
        assert_eq!((h.state(), h.version()), ("todo", 2));
        // seen "todo" at version 0, before both changes: not fresh although the state matches
        let Decision::Conflict(c) = h.submit(&d, &req(3, "todo", 0, "blocked", 2, 12)) else {
            panic!("a request from before the round trip must conflict");
        };
        assert_eq!(c.proposals.len(), 3);
        let mut h2 = History::new(E, &d);
        h2.submit(&d, &req(1, "todo", 0, "doing", 1, 10));
        h2.submit(&d, &req(2, "doing", 1, "todo", 1, 11));
        // seen "todo" at version 2: fresh
        let r = h2.submit(&d, &req(3, "todo", 2, "blocked", 2, 12));
        assert!(matches!(r, Decision::Accepted { .. }));
    }

    #[test]
    fn state_folded_into_a_conflict_is_reported_as_superseded() {
        let d = def();
        let mut h = History::new(E, &d);
        let (a, b) = offline_pair();
        h.submit(&d, &a);
        h.submit(&d, &b);
        // a client that saw "doing" (the superseded state) at version 1
        assert_eq!(
            rejected(h.submit(&d, &req(3, "doing", 1, "done", 1, 120))),
            "state_superseded"
        );
    }

    #[test]
    fn three_proposals_converge_in_every_arrival_order() {
        let d = def();
        let ps = [
            req(1, "todo", 0, "doing", 1, 100),
            req(2, "todo", 0, "blocked", 2, 90),
            req(3, "todo", 0, "done", 3, 95),
        ];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let mut outcomes = Vec::new();
        for o in orders {
            let mut h = History::new(E, &d);
            for i in o {
                h.submit(&d, &ps[i]);
            }
            outcomes.push((h.state().to_string(), h.open_conflict().cloned()));
        }
        assert!(outcomes.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(outcomes[0].1.as_ref().unwrap().proposals.len(), 3);
    }

    #[test]
    fn chained_requests_are_order_dependent_but_never_silent() {
        let d = def();
        let a = req(1, "todo", 0, "doing", 1, 100);
        let a2 = req(2, "doing", 1, "done", 1, 101);
        let b = req(3, "todo", 0, "blocked", 2, 90);
        let mut h1 = History::new(E, &d);
        for r in [&a, &a2, &b] {
            h1.submit(&d, r);
        }
        let mut h2 = History::new(E, &d);
        h2.submit(&d, &b);
        h2.submit(&d, &a);
        let last = h2.submit(&d, &a2);
        assert!(matches!(last, Decision::Rejected(_)));
        assert_eq!(h1.open_conflict().unwrap().proposals.len(), 3);
        assert_eq!(h2.open_conflict().unwrap().proposals.len(), 2);
        assert_ne!(h1.open_conflict(), h2.open_conflict());
    }

    #[test]
    fn third_proposal_joins_open_conflict() {
        let d = def();
        let mut h = History::new(E, &d);
        let (a, b) = offline_pair();
        h.submit(&d, &a);
        h.submit(&d, &b);
        let Decision::Conflict(c) = h.submit(&d, &req(3, "todo", 0, "done", 3, 95)) else {
            panic!("expected conflict");
        };
        assert_eq!((c.proposals.len(), c.ordinal), (3, 1));
        assert_eq!(h.open_conflict(), Some(&c));
    }

    #[test]
    fn stale_request_for_state_already_reached_is_rejected_not_lost() {
        let d = def();
        let mut h = History::new(E, &d);
        h.submit(&d, &req(1, "todo", 0, "doing", 1, 10));
        assert_eq!(
            rejected(h.submit(&d, &req(2, "todo", 0, "doing", 2, 11))),
            "already_in_target"
        );
    }

    #[test]
    fn duplicates_are_idempotent_and_keyed_by_actor_and_id() {
        let d = def();
        let mut h = History::new(E, &d);
        let a = req(1, "todo", 0, "doing", 1, 100);
        let first = h.submit(&d, &a);
        let snapshot = h.clone();
        assert_eq!(h.submit(&d, &a), first);
        assert_eq!(h, snapshot);
        let clash = req(1, "todo", 0, "blocked", 1, 100);
        assert_eq!(rejected(h.submit(&d, &clash)), "request_id_reused");
        assert_eq!(h, snapshot);
        assert_eq!(decide(&d, &h, &a), first);
        // another actor using the same id neither collides with it nor burns it
        let mut h2 = History::new(E, &d);
        let other = req(1, "todo", 0, "blocked", 2, 100);
        assert!(matches!(h2.submit(&d, &other), Decision::Accepted { .. }));
        let mine = req(1, "todo", 1, "doing", 1, 101);
        assert_ne!(rejected_or_ok(h2.submit(&d, &mine)), "request_id_reused");
    }

    fn rejected_or_ok(d: Decision) -> &'static str {
        match d {
            Decision::Rejected(c) => c.code(),
            Decision::Accepted { .. } => "accepted",
            Decision::Conflict(_) => "conflict",
        }
    }

    #[test]
    fn rejections_are_not_stored() {
        let d = def();
        let mut h = History::new(E, &d);
        for id in 0..100 {
            assert!(matches!(
                h.submit(&d, &req(id, "blocked", 0, "done", 1, 1)),
                Decision::Rejected(_)
            ));
        }
        assert_eq!((h.decided_len(), h.entries_len()), (0, 0));
    }

    #[test]
    fn history_is_bounded_and_a_conflict_can_still_be_resolved() {
        let d = def();
        let mut h = History::new(E, &d);
        let mut id = 0;
        loop {
            id += 1;
            let (from, to) = if h.state() == "todo" {
                ("todo", "doing")
            } else {
                ("doing", "todo")
            };
            let r = req(id, from, h.version(), to, 1, id);
            match h.submit(&d, &r) {
                Decision::Accepted { .. } => {}
                other => {
                    assert_eq!(other, Decision::Rejected(RejectReason::HistoryFull));
                    break;
                }
            }
        }
        assert_eq!(h.entries_len(), MAX_ENTRIES - 1);
        assert_eq!(h.decided_len(), MAX_ENTRIES - 1);
        // a repeat of an old accepted request still gets its stored answer
        let again = req(1, "todo", 0, "doing", 1, 1);
        assert!(matches!(h.submit(&d, &again), Decision::Accepted { .. }));
        // a late stale proposal still conflicts, and the conflict can be closed
        let v = h.version() - 1;
        let from = h.state_at(v).unwrap().to_string();
        let stale = req(id + 1, &from, v, "blocked", 2, 5);
        assert!(matches!(h.submit(&d, &stale), Decision::Conflict(_)));
        assert!(h.resolve(&d, 1, ConflictOption::Revert, RESOLVER).is_ok());
        assert_eq!(h.entries_len(), MAX_ENTRIES);
        assert!(h.decided_len() <= MAX_DECIDED);
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

    fn sample_conflict() -> ConflictEntity {
        let d = def();
        let mut h = History::new(E, &d);
        let (a, b) = offline_pair();
        h.submit(&d, &a);
        let Decision::Conflict(c) = h.submit(&d, &b) else {
            panic!("expected conflict");
        };
        c
    }

    #[test]
    fn conflict_codec_roundtrips_and_is_strict() {
        let c = sample_conflict();
        let bytes = c.encode().unwrap();
        assert_eq!(ConflictEntity::decode(&bytes).unwrap(), c);
        let mut closed = c.clone();
        closed.resolution = Some(Resolution {
            choice: ConflictOption::Revert,
            actor: 3,
        });
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

    #[test]
    fn encode_refuses_what_decode_would_refuse() {
        let c = sample_conflict();
        let mut one = c.clone();
        one.proposals.truncate(1);
        one.options = options_for(&one.proposals);
        let mut unsorted = c.clone();
        unsorted.proposals.reverse();
        let mut dup = c.clone();
        dup.proposals[1] = dup.proposals[0].clone();
        let mut options = c.clone();
        options.options.pop();
        let mut choice = c.clone();
        choice.resolution = Some(Resolution {
            choice: ConflictOption::Choose("done".into()),
            actor: 1,
        });
        let mut ordinal = c.clone();
        ordinal.ordinal = 0;
        let mut name = c.clone();
        name.base = "x".repeat(MAX_NAME_LEN + 1);
        let mut many = c;
        many.proposals = (0..=MAX_PROPOSALS as u64)
            .map(|i| Proposal {
                hlc: Hlc {
                    wall_ms: i,
                    counter: 0,
                    node: 0,
                },
                request_id: i,
                from: "todo".into(),
                to: "doing".into(),
                actor: 1,
            })
            .collect();
        many.options = options_for(&many.proposals);
        for bad in [one, unsorted, dup, options, choice, ordinal, name, many] {
            assert!(bad.encode().is_err(), "{bad:?}");
        }
    }

    /// Worked example from `formats/workflow-v1.md`, built by hand outside the encoder.
    const GOLDEN: &str = "0100000000000000000000000000000007010000000400746f646f02005a0000000000000000000000020000000200000000000000\
0400746f646f0700626c6f636b656402000000000000006400000000000000000000000100000001000000000000000400746f646f0500646f696e67\
0100000000000000030000010700626c6f636b6564010500646f696e6700";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn worked_example_matches_the_format_document() {
        let bytes = unhex(GOLDEN);
        assert_eq!(bytes.len(), 143);
        let c = ConflictEntity::decode(&bytes).unwrap();
        assert_eq!(c, sample_conflict());
        assert_eq!(c.encode().unwrap(), bytes);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Submit {
            id: u64,
            ver: u64,
            honest: bool,
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
            4 => (0u64..12, 0u64..8, any::<bool>(), 0usize..4, 0usize..4, 1u64..4, 0u64..50)
                .prop_map(|(id, ver, honest, from, to, actor, wall)| Op::Submit {
                    id, ver, honest, from, to, actor, wall
                }),
            1 => (0usize..6, 1u64..8).prop_map(|(pick, actor)| Op::Resolve { pick, actor }),
        ]
    }

    fn run(d: &WorkflowDef, ops: &[Op], double: bool) -> (History, Vec<Option<Decision>>) {
        let mut h = History::new(E, d);
        let mut out = Vec::new();
        for o in ops {
            match o {
                Op::Submit {
                    id,
                    ver,
                    honest,
                    from,
                    to,
                    actor,
                    wall,
                } => {
                    let ver = ver % (h.version() + 2);
                    let from = match (honest, h.state_at(ver)) {
                        (true, Some(s)) => s.to_string(),
                        _ => STATES[*from].to_string(),
                    };
                    let r = req(*id, &from, ver, STATES[*to], *actor, *wall);
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
                        // permission and guard failures are fine; they must not change anything
                        let before = h.clone();
                        if h.resolve(d, c.ordinal, choice, *actor).is_err() {
                            assert_eq!(h, before);
                        }
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
            let listed = |actor: u64, id: u64| {
                all_conflicts
                    .iter()
                    .any(|c| c.proposals.iter().any(|p| p.request_id == id && p.actor == actor))
            };
            let applied = h.applied_requests();
            for (o, dec) in ops.iter().zip(&out) {
                if let (Op::Submit { id, actor, .. }, Some(dec)) = (o, dec) {
                    match dec {
                        Decision::Rejected(r) => prop_assert!(!r.code().is_empty()),
                        Decision::Conflict(_) => prop_assert!(listed(*actor, *id)),
                        Decision::Accepted { .. } => {
                            prop_assert!(applied.contains(&(*actor, *id)) || listed(*actor, *id));
                        }
                    }
                }
            }
            for (actor, id) in h.superseded_requests() {
                prop_assert!(listed(actor, id), "superseded request {} not in any conflict", id);
            }
            prop_assert!(h.entries_len() <= MAX_ENTRIES && h.decided_len() <= MAX_DECIDED);
            for c in all_conflicts {
                prop_assert_eq!(&ConflictEntity::decode(&c.encode().unwrap()).unwrap(), c);
            }
        }

        #[test]
        fn two_stale_proposals_converge_in_any_arrival_order(
            to_a in 1usize..4, to_b in 1usize..4, wa in 0u64..20, wb in 0u64..20
        ) {
            let d = def();
            let a = req(1, "todo", 0, STATES[to_a], 1, wa);
            let b = req(2, "todo", 0, STATES[to_b], 2, wb);
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
            let mut bytes = sample_conflict().encode().unwrap();
            let n = bytes.len();
            bytes[pos % n] = byte;
            if let Ok(c2) = ConflictEntity::decode(&bytes) {
                prop_assert_eq!(c2.encode().unwrap(), bytes);
            }
        }
    }
}
