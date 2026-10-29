//! A small in-memory plugin that follows the contract, plus switchable faults used to prove that each
//! conformance check can fail.

use std::collections::BTreeMap;

use lattice_sdk::contract::{
    ErrorCode, Kind, Message, Receipt, Record, Status, TurnState, decode_line, encode_line,
};

use super::suite::PluginUnderTest;

/// One deliberate violation of the contract. `None` is the correct plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Correct behaviour.
    None,
    /// Every `session.create` makes a new session.
    CreateNotIdempotent,
    /// A parent that differs from the existing session's is accepted silently.
    ConflictIgnored,
    /// A resent record is stored again and reported `recorded`.
    DuplicateRecordedAgain,
    /// A superseded turn is treated as current.
    SupersededAccepted,
    /// A superseded turn is answered `fenced` but still written.
    FencedStillWrites,
    /// The fence check runs after the duplicate check, so a stored rid reports `duplicate`.
    DuplicateBeforeFence,
    /// `live` is never sent.
    NoLive,
    /// `live` is sent before the replay.
    LiveBeforeItems,
    /// A watch replays from the start whatever the cursor.
    ResumeIgnoresCursor,
    /// Open watches are not told about new items.
    NoIncremental,
    /// A cursor beyond the head is accepted.
    CursorBeyondHeadAccepted,
    /// The `parent` of a child session is not echoed.
    ParentNotEchoed,
    /// Lines that fail to decode get no answer.
    SilentOnBadLine,
    /// A wrong protocol version is reported as `bad_request`.
    WrongVersionCode,
}

enum Entry {
    Item {
        pos: u64,
        turn: u64,
        rid: String,
        kind: Kind,
        body: String,
    },
    Turn {
        pos: u64,
        turn: u64,
        state: TurnState,
    },
}

impl Entry {
    fn pos(&self) -> u64 {
        match self {
            Self::Item { pos, .. } | Self::Turn { pos, .. } => *pos,
        }
    }
}

struct Session {
    id: String,
    source: String,
    external_id: String,
    parent: Option<String>,
    entries: Vec<Entry>,
    rids: BTreeMap<String, u64>,
    current: u64,
    state: TurnState,
}

impl Session {
    fn head(&self) -> u64 {
        self.entries.last().map_or(0, Entry::pos)
    }

    fn store(&mut self, turn: u64, r: &Record) -> u64 {
        let pos = self.head() + 1;
        self.entries.push(Entry::Item {
            pos,
            turn,
            rid: r.rid.clone(),
            kind: r.kind,
            body: r.body.clone(),
        });
        self.rids.insert(r.rid.clone(), pos);
        pos
    }

    fn turn_event(&mut self, turn: u64, state: TurnState) {
        let pos = self.head() + 1;
        self.entries.push(Entry::Turn { pos, turn, state });
    }
}

struct Watch {
    id: String,
    session: String,
}

/// The reference plugin.
pub struct ReferencePlugin {
    fault: Fault,
    sessions: Vec<Session>,
    watches: Vec<Watch>,
}

impl ReferencePlugin {
    /// A plugin with the given fault (`Fault::None` for the correct one).
    pub fn new(fault: Fault) -> Self {
        Self {
            fault,
            sessions: Vec::new(),
            watches: Vec::new(),
        }
    }

    /// The correct plugin.
    pub fn correct() -> Self {
        Self::new(Fault::None)
    }

    fn error(id: Option<&str>, code: ErrorCode, message: &str) -> Message {
        Message::Error {
            id: id.map(Into::into),
            code,
            message: message.into(),
        }
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    fn event_message(wid: &str, session: &str, e: &Entry) -> Message {
        match e {
            Entry::Item {
                pos,
                turn,
                rid,
                kind,
                body,
            } => Message::Item {
                id: wid.into(),
                session: session.into(),
                pos: *pos,
                turn: *turn,
                rid: rid.clone(),
                kind: *kind,
                body: body.clone(),
            },
            Entry::Turn { pos, turn, state } => Message::TurnEvent {
                id: wid.into(),
                session: session.into(),
                pos: *pos,
                turn: *turn,
                state: *state,
            },
        }
    }

    /// Messages for open watches about the entries a session gained after index `from`.
    fn notify(&self, at: usize, from: usize) -> Vec<Message> {
        if self.fault == Fault::NoIncremental {
            return Vec::new();
        }
        let Some(s) = self.sessions.get(at) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in s.entries.iter().skip(from) {
            for w in self.watches.iter().filter(|w| w.session == s.id) {
                out.push(Self::event_message(&w.id, &s.id, e));
            }
        }
        out
    }

    fn handle_line(&mut self, line: &[u8]) -> Vec<Message> {
        match decode_line(line) {
            Ok(m) => self.handle(m),
            Err(e) if self.fault == Fault::SilentOnBadLine => {
                let _ = e;
                Vec::new()
            }
            Err(e) => {
                let code = if self.fault == Fault::WrongVersionCode
                    && e.code() == ErrorCode::UnsupportedVersion
                {
                    ErrorCode::BadRequest
                } else {
                    e.code()
                };
                vec![Self::error(None, code, &e.to_string())]
            }
        }
    }

    fn handle(&mut self, msg: Message) -> Vec<Message> {
        match msg {
            Message::SessionCreate {
                id,
                source,
                external_id,
                parent,
            } => self.create(&id, source, external_id, parent),
            Message::TurnTake { id, session } => {
                let Some(at) = self.find(&session) else {
                    return vec![Self::error(
                        Some(&id),
                        ErrorCode::UnknownSession,
                        "unknown session",
                    )];
                };
                let before = self.sessions.get(at).map_or(0, |s| s.entries.len());
                let mut turn = 0;
                if let Some(s) = self.sessions.get_mut(at) {
                    if s.current > 0 && s.state == TurnState::Running {
                        let old = s.current;
                        s.turn_event(old, TurnState::Superseded);
                    }
                    s.current += 1;
                    s.state = TurnState::Running;
                    turn = s.current;
                    s.turn_event(turn, TurnState::Running);
                }
                let mut out = vec![Message::TurnStateReply {
                    id,
                    session,
                    turn,
                    state: TurnState::Running,
                }];
                out.extend(self.notify(at, before));
                out
            }
            Message::TurnEnd { id, session, turn } => {
                let Some(at) = self.find(&session) else {
                    return vec![Self::error(
                        Some(&id),
                        ErrorCode::UnknownSession,
                        "unknown session",
                    )];
                };
                let before = self.sessions.get(at).map_or(0, |s| s.entries.len());
                let current = self.sessions.get(at).map_or(0, |s| s.current);
                let state = if turn == current {
                    if let Some(s) = self.sessions.get_mut(at) {
                        s.state = TurnState::Idle;
                        s.turn_event(turn, TurnState::Idle);
                    }
                    TurnState::Idle
                } else if turn < current {
                    TurnState::Superseded
                } else {
                    return vec![Self::error(
                        Some(&id),
                        ErrorCode::UnknownTurn,
                        "turn was never issued",
                    )];
                };
                let mut out = vec![Message::TurnStateReply {
                    id,
                    session,
                    turn,
                    state,
                }];
                out.extend(self.notify(at, before));
                out
            }
            Message::Append {
                id,
                session,
                turn,
                records,
            } => self.append(id, session, turn, &records),
            Message::Watch {
                id,
                session,
                cursor,
            } => self.open_watch(&id, &session, cursor),
            Message::WatchStop { id } => match self.watches.iter().position(|w| w.id == id) {
                Some(i) => {
                    self.watches.remove(i);
                    vec![Message::WatchStopped { id }]
                }
                None => vec![Self::error(
                    Some(&id),
                    ErrorCode::UnknownWatch,
                    "no such watch",
                )],
            },
            _ => vec![Self::error(None, ErrorCode::BadRequest, "not a request")],
        }
    }

    fn create(
        &mut self,
        id: &str,
        source: String,
        external_id: String,
        parent: Option<String>,
    ) -> Vec<Message> {
        let existing = self.sessions.iter().find(|s| {
            s.source == source
                && s.external_id == external_id
                && self.fault != Fault::CreateNotIdempotent
        });
        if let Some(s) = existing {
            if s.parent != parent && self.fault != Fault::ConflictIgnored {
                return vec![Self::error(
                    Some(id),
                    ErrorCode::Conflict,
                    "session exists with a different parent",
                )];
            }
            return vec![Message::SessionCreated {
                id: id.into(),
                session: s.id.clone(),
                created: false,
                parent: self.echo(s.parent.clone()),
            }];
        }
        if let Some(p) = &parent
            && self.find(p).is_none()
        {
            return vec![Self::error(
                Some(id),
                ErrorCode::UnknownSession,
                "unknown parent session",
            )];
        }
        let session = format!("s{}", self.sessions.len() + 1);
        self.sessions.push(Session {
            id: session.clone(),
            source,
            external_id,
            parent: parent.clone(),
            entries: Vec::new(),
            rids: BTreeMap::new(),
            current: 0,
            state: TurnState::Idle,
        });
        vec![Message::SessionCreated {
            id: id.into(),
            session,
            created: true,
            parent: self.echo(parent),
        }]
    }

    fn echo(&self, parent: Option<String>) -> Option<String> {
        if self.fault == Fault::ParentNotEchoed {
            None
        } else {
            parent
        }
    }

    fn append(
        &mut self,
        id: String,
        session: String,
        turn: u64,
        records: &[Record],
    ) -> Vec<Message> {
        let Some(at) = self.find(&session) else {
            return vec![Self::error(
                Some(&id),
                ErrorCode::UnknownSession,
                "unknown session",
            )];
        };
        let fault = self.fault;
        let before = self.sessions.get(at).map_or(0, |s| s.entries.len());
        let current = self.sessions.get(at).map_or(0, |s| s.current);
        if turn > current {
            return vec![Self::error(
                Some(&id),
                ErrorCode::UnknownTurn,
                "turn was never issued",
            )];
        }
        let superseded = turn < current && fault != Fault::SupersededAccepted;
        let mut receipts = Vec::new();
        if let Some(s) = self.sessions.get_mut(at) {
            for r in records {
                let known = s.rids.get(&r.rid).copied();
                let receipt = if superseded
                    && fault == Fault::DuplicateBeforeFence
                    && known.is_some()
                {
                    Receipt {
                        rid: r.rid.clone(),
                        status: Status::Duplicate,
                        pos: known,
                    }
                } else if superseded {
                    if fault == Fault::FencedStillWrites && known.is_none() {
                        s.store(turn, r);
                    }
                    Receipt {
                        rid: r.rid.clone(),
                        status: Status::Fenced,
                        pos: None,
                    }
                } else if let (Some(pos), true) = (known, fault != Fault::DuplicateRecordedAgain) {
                    Receipt {
                        rid: r.rid.clone(),
                        status: Status::Duplicate,
                        pos: Some(pos),
                    }
                } else {
                    let pos = s.store(turn, r);
                    Receipt {
                        rid: r.rid.clone(),
                        status: Status::Recorded,
                        pos: Some(pos),
                    }
                };
                receipts.push(receipt);
            }
        }
        let mut out = vec![Message::AppendReceipt {
            id,
            session,
            turn,
            receipts,
        }];
        out.extend(self.notify(at, before));
        out
    }

    fn open_watch(&mut self, id: &str, session: &str, cursor: u64) -> Vec<Message> {
        let Some(at) = self.find(session) else {
            return vec![Self::error(
                Some(id),
                ErrorCode::UnknownSession,
                "unknown session",
            )];
        };
        if self.watches.iter().any(|w| w.id == id) {
            return vec![Self::error(
                Some(id),
                ErrorCode::Conflict,
                "watch id in use",
            )];
        }
        let Some(s) = self.sessions.get(at) else {
            return Vec::new();
        };
        let head = s.head();
        if cursor > head && self.fault != Fault::CursorBeyondHeadAccepted {
            return vec![Self::error(
                Some(id),
                ErrorCode::BadRequest,
                "cursor is beyond head",
            )];
        }
        let from = if self.fault == Fault::ResumeIgnoresCursor {
            0
        } else {
            cursor
        };
        let live = Message::Live {
            id: id.into(),
            session: session.into(),
        };
        let mut out = vec![Message::Ready {
            id: id.into(),
            session: session.into(),
            head,
        }];
        if self.fault == Fault::LiveBeforeItems {
            out.push(live.clone());
        }
        out.extend(
            s.entries
                .iter()
                .filter(|e| e.pos() > from)
                .map(|e| Self::event_message(id, session, e)),
        );
        if self.fault != Fault::NoLive && self.fault != Fault::LiveBeforeItems {
            out.push(live);
        }
        self.watches.push(Watch {
            id: id.into(),
            session: session.into(),
        });
        out
    }
}

impl PluginUnderTest for ReferencePlugin {
    fn send(&mut self, line: &[u8]) -> Vec<Vec<u8>> {
        self.handle_line(line)
            .iter()
            .filter_map(|m| encode_line(m).ok())
            .map(String::into_bytes)
            .collect()
    }
}
