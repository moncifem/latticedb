//! The contract checks. Each check takes a fresh plugin and returns `Err` with the first rule it saw
//! broken. Expectations come from `formats/plugin-contract-v1.md`, not from any implementation.

use lattice_sdk::contract::{
    ErrorCode, Kind, MAX_LINE_BYTES, Message, Receipt, Record, Status, TurnState, decode_line,
    encode_line,
};

/// A plugin reachable through lines: send one request line, get every line produced because of it,
/// including stream messages for open watches.
pub trait PluginUnderTest {
    /// Deliver one line (without newline) and return the lines the plugin wrote in response.
    fn send(&mut self, line: &[u8]) -> Vec<Vec<u8>>;
}

/// One conformance check.
pub type Check = fn(&mut dyn PluginUnderTest) -> Result<(), String>;

/// Every check, by name.
pub const CHECKS: &[(&str, Check)] = &[
    ("create_is_idempotent", create_is_idempotent),
    (
        "append_receipts_and_duplicates",
        append_receipts_and_duplicates,
    ),
    ("superseded_turn_is_fenced", superseded_turn_is_fenced),
    ("watch_ready_items_live", watch_ready_items_live),
    ("child_sessions", child_sessions),
    ("rejects_bad_lines", rejects_bad_lines),
];

/// Run every check, each against a fresh plugin.
pub fn run_all(
    make: &dyn Fn() -> Box<dyn PluginUnderTest>,
) -> Vec<(&'static str, Result<(), String>)> {
    CHECKS
        .iter()
        .map(|(name, check)| {
            let mut plugin = make();
            (*name, check(plugin.as_mut()))
        })
        .collect()
}

fn decode_all(lines: Vec<Vec<u8>>) -> Result<Vec<Message>, String> {
    lines
        .iter()
        .map(|l| decode_line(l).map_err(|e| format!("plugin wrote an invalid line {l:?}: {e}")))
        .collect()
}

fn call(p: &mut dyn PluginUnderTest, msg: &Message) -> Result<Vec<Message>, String> {
    let line = encode_line(msg).map_err(|e| format!("suite built an invalid request: {e}"))?;
    decode_all(p.send(line.as_bytes()))
}

fn one(mut msgs: Vec<Message>, what: &str) -> Result<Message, String> {
    if msgs.len() != 1 {
        return Err(format!(
            "{what}: expected exactly one reply, got {}",
            msgs.len()
        ));
    }
    Ok(msgs.remove(0))
}

fn create(
    p: &mut dyn PluginUnderTest,
    source: &str,
    ext: &str,
    parent: Option<&str>,
) -> Result<Message, String> {
    let m = Message::SessionCreate {
        id: "c".into(),
        source: source.into(),
        external_id: ext.into(),
        parent: parent.map(Into::into),
    };
    one(call(p, &m)?, "session.create")
}

fn created(m: Message) -> Result<(String, bool, Option<String>), String> {
    match m {
        Message::SessionCreated {
            id,
            session,
            created,
            parent,
        } if id == "c" => Ok((session, created, parent)),
        other => Err(format!(
            "expected session.created echoing the request id, got {other:?}"
        )),
    }
}

fn take(p: &mut dyn PluginUnderTest, session: &str) -> Result<u64, String> {
    let m = Message::TurnTake {
        id: "t".into(),
        session: session.into(),
    };
    match one(call(p, &m)?, "turn.take")? {
        Message::TurnStateReply {
            id,
            turn,
            state: TurnState::Running,
            ..
        } if id == "t" => Ok(turn),
        other => Err(format!("expected turn.state running, got {other:?}")),
    }
}

fn rec(rid: &str, body: &str) -> Record {
    Record {
        rid: rid.into(),
        kind: Kind::Message,
        body: body.into(),
    }
}

/// Send an append; return the receipts and any further messages (stream items for open watches).
fn append(
    p: &mut dyn PluginUnderTest,
    session: &str,
    turn: u64,
    records: Vec<Record>,
) -> Result<(Vec<Receipt>, Vec<Message>), String> {
    let n = records.len();
    let m = Message::Append {
        id: "a".into(),
        session: session.into(),
        turn,
        records,
    };
    let mut msgs = call(p, &m)?;
    if msgs.is_empty() {
        return Err("append: no reply".into());
    }
    match msgs.remove(0) {
        Message::AppendReceipt {
            id,
            session: s,
            turn: t,
            receipts,
        } if id == "a" && s == session && t == turn => {
            if receipts.len() != n {
                return Err(format!(
                    "append: {} receipts for {n} records",
                    receipts.len()
                ));
            }
            Ok((receipts, msgs))
        }
        other => Err(format!(
            "append: expected append.receipt first, got {other:?}"
        )),
    }
}

fn watch(
    p: &mut dyn PluginUnderTest,
    wid: &str,
    session: &str,
    cursor: u64,
) -> Result<Vec<Message>, String> {
    call(
        p,
        &Message::Watch {
            id: wid.into(),
            session: session.into(),
            cursor,
        },
    )
}

/// Validate the opening sequence of a watch; returns the head and the replayed (pos, rid) pairs.
/// A turn event has an empty rid.
fn check_open(
    msgs: &[Message],
    wid: &str,
    session: &str,
    cursor: u64,
) -> Result<(u64, Vec<(u64, String)>), String> {
    let Some(Message::Ready {
        id,
        session: s,
        head,
    }) = msgs.first()
    else {
        return Err(format!(
            "watch must start with ready, got {:?}",
            msgs.first()
        ));
    };
    if id != wid || s != session {
        return Err("ready does not carry the watch id and session".into());
    }
    let Some(Message::Live { id, session: s }) = msgs.last() else {
        return Err("watch must end its replay with live".into());
    };
    if id != wid || s != session {
        return Err("live does not carry the watch id and session".into());
    }
    let mut seen = Vec::new();
    let mut last = cursor;
    for m in msgs.iter().skip(1).take(msgs.len().saturating_sub(2)) {
        let (pos, rid) = match m {
            Message::Item { id, pos, rid, .. } if id == wid => (*pos, rid.clone()),
            Message::TurnEvent { id, pos, .. } if id == wid => (*pos, String::new()),
            other => {
                return Err(format!(
                    "unexpected message between ready and live: {other:?}"
                ));
            }
        };
        if pos != last + 1 {
            return Err(format!(
                "replay position {pos} after {last}: gap, repeat or disorder"
            ));
        }
        last = pos;
        seen.push((pos, rid));
    }
    if last != *head {
        return Err(format!("replay ends at {last} but ready said head {head}"));
    }
    Ok((*head, seen))
}

fn rids(seen: &[(u64, String)]) -> Vec<&str> {
    seen.iter()
        .filter(|(_, r)| !r.is_empty())
        .map(|(_, r)| r.as_str())
        .collect()
}

/// The same source and external id is the same session; either part differing is a new one.
pub fn create_is_idempotent(p: &mut dyn PluginUnderTest) -> Result<(), String> {
    let (a, first, _) = created(create(p, "suite", "x1", None)?)?;
    if !first {
        return Err("a new (source, external_id) must report created true".into());
    }
    let (again, created_again, _) = created(create(p, "suite", "x1", None)?)?;
    if again != a || created_again {
        return Err(format!(
            "repeating a create must return session {a} with created false, got {again} / {created_again}"
        ));
    }
    let (other, is_new, _) = created(create(p, "suite", "x2", None)?)?;
    if other == a || !is_new {
        return Err("a different external_id must be a new session".into());
    }
    let (third, is_new, _) = created(create(p, "other-source", "x1", None)?)?;
    if third == a || third == other || !is_new {
        return Err("the same external_id under another source must be a new session".into());
    }
    Ok(())
}

/// New records are `recorded`; a resend is `duplicate` at the first position and stores nothing.
pub fn append_receipts_and_duplicates(p: &mut dyn PluginUnderTest) -> Result<(), String> {
    let (s, ..) = created(create(p, "suite", "dup", None)?)?;
    let turn = take(p, &s)?;
    let (r, _) = append(p, &s, turn, vec![rec("r1", "one"), rec("r2", "two")])?;
    let [first, second] = r.as_slice() else {
        return Err("two receipts expected".into());
    };
    let (Some(p1), Some(p2)) = (first.pos, second.pos) else {
        return Err("recorded receipts must carry pos".into());
    };
    if first.status != Status::Recorded || second.status != Status::Recorded || p2 <= p1 {
        return Err(format!(
            "new records must be recorded at increasing pos: {r:?}"
        ));
    }
    if first.rid != "r1" || second.rid != "r2" {
        return Err("receipts must follow the order of the records".into());
    }
    let (r, _) = append(p, &s, turn, vec![rec("r1", "one"), rec("r3", "three")])?;
    let [again, new] = r.as_slice() else {
        return Err("two receipts expected".into());
    };
    if again.status != Status::Duplicate || again.pos != Some(p1) {
        return Err(format!(
            "a resent record must be duplicate at its first pos {p1}: {again:?}"
        ));
    }
    if new.status != Status::Recorded || new.pos.is_none_or(|n| n <= p2) {
        return Err(format!(
            "a new record after a duplicate must be recorded: {new:?}"
        ));
    }
    let msgs = watch(p, "w1", &s, 0)?;
    let (_, seen) = check_open(&msgs, "w1", &s, 0)?;
    if rids(&seen) != ["r1", "r2", "r3"] {
        return Err(format!(
            "replay must hold each record once, got {:?}",
            rids(&seen)
        ));
    }
    Ok(())
}

/// After a takeover the old turn gets `fenced` and writes nothing.
pub fn superseded_turn_is_fenced(p: &mut dyn PluginUnderTest) -> Result<(), String> {
    let (s, ..) = created(create(p, "suite", "fence", None)?)?;
    let t1 = take(p, &s)?;
    let (r, _) = append(p, &s, t1, vec![rec("old", "before takeover")])?;
    if r.first().map(|x| x.status) != Some(Status::Recorded) {
        return Err("the current turn must be able to write".into());
    }
    let t2 = take(p, &s)?;
    if t2 <= t1 {
        return Err("a takeover must issue a higher turn".into());
    }
    let (r, extra) = append(p, &s, t1, vec![rec("late", "x"), rec("old", "y")])?;
    for x in &r {
        if x.status != Status::Fenced || x.pos.is_some() {
            return Err(format!(
                "every record under a superseded turn must be fenced without pos (even a stored rid): {x:?}"
            ));
        }
    }
    if !extra.is_empty() {
        return Err(format!("a fenced append must not notify anyone: {extra:?}"));
    }
    let msgs = watch(p, "w1", &s, 0)?;
    let (_, seen) = check_open(&msgs, "w1", &s, 0)?;
    if rids(&seen) != ["old"] {
        return Err(format!(
            "a fenced append wrote something: {:?}",
            rids(&seen)
        ));
    }
    let states: Vec<_> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::TurnEvent { turn, state, .. } => Some((*turn, *state)),
            _ => None,
        })
        .collect();
    let expect = [
        (t1, TurnState::Running),
        (t1, TurnState::Superseded),
        (t2, TurnState::Running),
    ];
    if states != expect {
        return Err(format!(
            "turn events must show the takeover, got {states:?}"
        ));
    }
    let (r, _) = append(p, &s, t2, vec![rec("late", "now allowed")])?;
    if r.first().map(|x| x.status) != Some(Status::Recorded) {
        return Err("the new turn must be able to write the rid the fence refused".into());
    }
    Ok(())
}

/// A watch sends `ready`, the replay in order, `live`, then new items; resume repeats nothing.
pub fn watch_ready_items_live(p: &mut dyn PluginUnderTest) -> Result<(), String> {
    let (s, ..) = created(create(p, "suite", "watch", None)?)?;
    let turn = take(p, &s)?;
    append(
        p,
        &s,
        turn,
        vec![rec("a", "1"), rec("b", "2"), rec("c", "3")],
    )?;
    let msgs = watch(p, "w1", &s, 0)?;
    let (head, seen) = check_open(&msgs, "w1", &s, 0)?;
    if rids(&seen) != ["a", "b", "c"] {
        return Err(format!(
            "replay must list the records in order: {:?}",
            rids(&seen)
        ));
    }
    let (_, extra) = append(p, &s, turn, vec![rec("d", "4")])?;
    match extra.as_slice() {
        [Message::Item { id, pos, rid, .. }] if id == "w1" && rid == "d" && *pos == head + 1 => {}
        other => {
            return Err(format!(
                "expected one incremental item for d at {}: {other:?}",
                head + 1
            ));
        }
    }
    let (_, extra) = append(p, &s, turn, vec![rec("d", "4")])?;
    if !extra.is_empty() {
        return Err(format!(
            "a duplicate append must not reach watchers: {extra:?}"
        ));
    }
    let msgs = watch(p, "w2", &s, head)?;
    let (new_head, seen) = check_open(&msgs, "w2", &s, head)?;
    if new_head != head + 1 || rids(&seen) != ["d"] {
        return Err(format!("resume from {head} must replay only d: {seen:?}"));
    }
    let msgs = watch(p, "w3", &s, new_head + 5)?;
    if !matches!(
        msgs.as_slice(),
        [Message::Error {
            code: ErrorCode::BadRequest,
            ..
        }]
    ) {
        return Err(format!(
            "a cursor beyond head must be bad_request: {msgs:?}"
        ));
    }
    Ok(())
}

/// A child echoes its parent; the parent is fixed and must exist.
pub fn child_sessions(p: &mut dyn PluginUnderTest) -> Result<(), String> {
    let (parent, ..) = created(create(p, "suite", "main", None)?)?;
    let (child, new, echoed) = created(create(p, "suite", "sub", Some(&parent))?)?;
    if !new || echoed.as_deref() != Some(parent.as_str()) || child == parent {
        return Err("a child must be a new session that echoes its parent".into());
    }
    let (again, new, echoed) = created(create(p, "suite", "sub", Some(&parent))?)?;
    if again != child || new || echoed.as_deref() != Some(parent.as_str()) {
        return Err("repeating a child create must be idempotent".into());
    }
    match create(p, "suite", "sub", None)? {
        Message::Error {
            code: ErrorCode::Conflict,
            ..
        } => {}
        other => {
            return Err(format!(
                "a different parent must be conflict, got {other:?}"
            ));
        }
    }
    match create(p, "suite", "ghost", Some("nope"))? {
        Message::Error {
            code: ErrorCode::UnknownSession,
            ..
        } => {}
        other => {
            return Err(format!(
                "an unknown parent must be unknown_session: {other:?}"
            ));
        }
    }
    Ok(())
}

/// Refused lines get exactly one `error` and do not break the stream.
pub fn rejects_bad_lines(p: &mut dyn PluginUnderTest) -> Result<(), String> {
    let oversize = vec![b'x'; MAX_LINE_BYTES + 1];
    let cases: [(&[u8], ErrorCode); 5] = [
        (
            br#"{"v":2,"t":"turn.take","id":"r","session":"s1"}"#,
            ErrorCode::UnsupportedVersion,
        ),
        (
            br#"{"v":1,"t":"turn.take","id":"r","session":"s1","extra":1}"#,
            ErrorCode::BadRequest,
        ),
        (b"]", ErrorCode::BadRequest),
        (&[0xff, 0xfe, b'{'], ErrorCode::BadRequest),
        (&oversize, ErrorCode::TooLarge),
    ];
    for (line, want) in cases {
        let got = decode_all(p.send(line))?;
        match got.as_slice() {
            [Message::Error { code, .. }] if *code == want => {}
            other => {
                return Err(format!(
                    "bad line must give one {want:?} error, got {other:?}"
                ));
            }
        }
    }
    let (_, new, _) = created(create(p, "suite", "after-errors", None)?)?;
    if !new {
        return Err("the plugin must keep working after refusing lines".into());
    }
    Ok(())
}
