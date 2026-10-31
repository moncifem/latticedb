//! Author ids: who a committed change set is attributed to (issue 520).
//!
//! The change set already carries a 128 bit author (`formats/changeset-v1.md`), so recording who
//! wrote needs no byte change in the log or the change feed. What was missing is an id that is
//! **injective** per principal: two different accounts must never share one, because clients use the
//! author to ignore their own echoes and the CRDT node binding of the document work uses it as an
//! identity. Nothing here is derived from client input; the host passes the name of an account it
//! authenticated.
//!
//! * **Id space.** `0` is "unknown" (every commit made before this module, and every internal commit
//!   that has no actor). `1` is the fixed local-development principal. `2..=255` are reserved for future
//!   built-in actors. Accounts are numbered from [`FIRST_ACCOUNT_AUTHOR`] upward, in the order they
//!   were first seen, by a counter that only grows.
//! * **Registry.** [`AuthorRegistry`] keeps that assignment in an append-only file (`authors.reg`)
//!   beside the log, written and synced before an id is handed out, so a commit never carries an id the
//!   registry could lose. It is a local sidecar, not part of the WAL, the change set or the feed.
//!   See `crates/lattice-engines/README.md` for the line format. The format is experimental and not frozen.
//! * **Rename and delete.** [`AuthorRegistry::rename`] keeps the id under the new name, only by this
//!   explicit call. [`AuthorRegistry::retire`] ends an account; its id is never handed out again, and a
//!   later account with the same name is a new account with a new id. History keeps showing the old id.
//! * **Collisions.** Opening a registry whose file lists one live name twice, one id twice, or an id
//!   below an earlier one fails with [`AuthorError::Corrupt`], so a damaged or hand-edited file refuses
//!   startup instead of attributing two accounts to one author.

use lattice_core::bugs::{self, Bug};
use lattice_seams::Io;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The author of changes whose writer is not recorded: all history from before author ids existed,
/// and commits the engine makes for itself.
pub const AUTHOR_UNKNOWN: u128 = 0;
/// The fixed author of the local-development principal of loopback trust mode.
pub const AUTHOR_LOCAL_DEVELOPMENT: u128 = 1;
/// The first id an account can receive. Ids below it are reserved.
pub const FIRST_ACCOUNT_AUTHOR: u128 = 256;
/// Longest account name the registry stores, in bytes.
pub const MAX_ACCOUNT_NAME_BYTES: usize = 255;
/// Most ids a registry will ever assign (live and retired). A bound on file size and replay time.
pub const MAX_REGISTRY_IDS: usize = 20_000;
/// Largest registry file read at startup. Larger is treated as corrupt.
pub const MAX_REGISTRY_BYTES: u64 = 8 * 1024 * 1024;

const FILE: &str = "authors.reg";
const HEADER: &str = "lattice-authors v1\n";

/// Why the registry refused an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorError {
    /// The name is empty, longer than [`MAX_ACCOUNT_NAME_BYTES`] or contains a control character.
    InvalidName,
    /// The account is not in the registry (rename or retire of an unknown or retired name).
    Unknown,
    /// The target name already belongs to a live account.
    Taken,
    /// The registry already assigned [`MAX_REGISTRY_IDS`] ids.
    Full,
    /// The file is damaged or lists a collision. The 1-based line is given when one is at fault.
    /// The content is not echoed.
    Corrupt {
        /// The line that broke a rule, or 0 for a whole-file problem.
        line: usize,
    },
    /// The retained log carries an author id the registry does not list: the registry file is
    /// missing, restored from an older backup, or lost a line that a commit had already used.
    BehindLog,
    /// The storage failed. The registry refuses further changes until it is reopened, because the
    /// file may now end in bytes it did not intend.
    Io(String),
}

impl fmt::Display for AuthorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("invalid account name"),
            Self::Unknown => f.write_str("unknown account"),
            Self::Taken => f.write_str("an account with that name exists"),
            Self::Full => f.write_str("the author registry is full"),
            Self::BehindLog => f.write_str("the author registry is missing or older than the log"),
            Self::Corrupt { line } => write!(f, "the author registry is corrupt (line {line})"),
            Self::Io(e) => write!(f, "author registry storage failed: {e}"),
        }
    }
}

impl std::error::Error for AuthorError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    retired: bool,
}

/// The durable name-to-author assignment. See the module documentation.
pub struct AuthorRegistry {
    /// `None` keeps the assignment in memory only (tests and hosts without a data directory).
    io: Option<Box<dyn Io>>,
    by_id: BTreeMap<u128, Entry>,
    live: BTreeMap<String, u128>,
    /// The next id to hand out; strictly above every id ever assigned.
    next: u128,
    /// File length known to be a whole number of lines.
    len: u64,
    broken: bool,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_ACCOUNT_NAME_BYTES && !name.chars().any(char::is_control)
}

impl AuthorRegistry {
    /// A registry that keeps its assignment in memory. Ids are injective within the process only.
    pub fn in_memory() -> Self {
        Self {
            io: None,
            by_id: BTreeMap::new(),
            live: BTreeMap::new(),
            next: FIRST_ACCOUNT_AUTHOR,
            len: 0,
            broken: false,
        }
    }

    /// Opens the registry stored as `authors.reg` in `io`'s root, creating nothing until the first
    /// account is assigned. A final line without its newline is a torn append: it is removed.
    pub fn open(mut io: Box<dyn Io>) -> Result<Self, AuthorError> {
        let mut reg = Self::in_memory();
        if io.exists(FILE) {
            let size = io.len(FILE).map_err(|e| AuthorError::Io(e.to_string()))?;
            if size > MAX_REGISTRY_BYTES {
                return Err(AuthorError::Corrupt { line: 0 });
            }
            let bytes = io.read(FILE).map_err(|e| AuthorError::Io(e.to_string()))?;
            let whole = bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |i| i.saturating_add(1));
            let text = bytes
                .get(..whole)
                .and_then(|b| std::str::from_utf8(b).ok())
                .ok_or(AuthorError::Corrupt { line: 0 })?;
            reg.replay(text)?;
            reg.len = whole as u64;
            if (whole as u64) < size {
                io.truncate(FILE, whole as u64)
                    .and_then(|()| io.sync(FILE))
                    .map_err(|e| AuthorError::Io(e.to_string()))?;
            }
        }
        reg.io = Some(io);
        Ok(reg)
    }

    fn replay(&mut self, text: &str) -> Result<(), AuthorError> {
        let mut lines = text.lines().enumerate();
        match lines.next() {
            None => return Ok(()),
            Some((_, first)) if format!("{first}\n") == HEADER => {}
            Some(_) => return Err(AuthorError::Corrupt { line: 1 }),
        }
        for (index, line) in lines {
            let bad = AuthorError::Corrupt { line: index + 1 };
            let mut parts = line.splitn(3, '\t');
            let (op, id, name) = (parts.next(), parts.next(), parts.next());
            let id: u128 = id.and_then(|t| t.parse().ok()).ok_or(bad.clone())?;
            match (op, name) {
                (Some("A"), Some(name)) => {
                    // Strictly increasing: an id is never repeated or reused, retired or not.
                    if id < self.next || !valid_name(name) || self.live.contains_key(name) {
                        return Err(bad);
                    }
                    if self.by_id.len() >= MAX_REGISTRY_IDS {
                        return Err(bad);
                    }
                    self.by_id.insert(
                        id,
                        Entry {
                            name: name.to_string(),
                            retired: false,
                        },
                    );
                    self.live.insert(name.to_string(), id);
                    self.next = id.checked_add(1).ok_or(bad)?;
                }
                (Some("R"), Some(new)) => {
                    let entry = self.by_id.get_mut(&id).ok_or(bad.clone())?;
                    if entry.retired || !valid_name(new) || self.live.contains_key(new) {
                        return Err(bad);
                    }
                    self.live.remove(&entry.name);
                    entry.name = new.to_string();
                    self.live.insert(new.to_string(), id);
                }
                (Some("D"), None) => {
                    let entry = self.by_id.get_mut(&id).ok_or(bad.clone())?;
                    if entry.retired {
                        return Err(bad);
                    }
                    entry.retired = true;
                    self.live.remove(&entry.name);
                }
                _ => return Err(bad),
            }
        }
        Ok(())
    }

    /// Appends one whole line and makes it durable before returning, or leaves the file as it was.
    fn persist(&mut self, line: &str) -> Result<(), AuthorError> {
        if self.broken {
            return Err(AuthorError::Io("an earlier write failed".into()));
        }
        let Some(io) = self.io.as_mut() else {
            return Ok(());
        };
        let fresh = self.len == 0;
        let mut data = String::new();
        if fresh {
            data.push_str(HEADER);
        }
        data.push_str(line);
        data.push('\n');
        let result = io
            .append(FILE, data.as_bytes())
            .and_then(|()| io.sync(FILE))
            .and_then(|()| if fresh { io.sync_dir("") } else { Ok(()) });
        match result {
            Ok(()) => {
                self.len = self.len.saturating_add(data.len() as u64);
                Ok(())
            }
            Err(e) => {
                // The file may now end in a partial line that a later append would glue to its own.
                if io
                    .truncate(FILE, self.len)
                    .and_then(|()| io.sync(FILE))
                    .is_err()
                {
                    self.broken = true;
                }
                Err(AuthorError::Io(e.to_string()))
            }
        }
    }

    /// The author id of the account `name`, assigned and made durable on first use. The caller
    /// passes the name of an account it authenticated; nothing a client sent.
    pub fn author_for_account(&mut self, name: &str) -> Result<u128, AuthorError> {
        if let Some(id) = self.live.get(name) {
            return Ok(*id);
        }
        if !valid_name(name) {
            return Err(AuthorError::InvalidName);
        }
        if self.by_id.len() >= MAX_REGISTRY_IDS {
            return Err(AuthorError::Full);
        }
        if bugs::active(Bug::AuthorIdTruncatedHash) {
            // The planted defect: a hash cut to four bits, so unrelated names collide.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in name.bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
            let id = FIRST_ACCOUNT_AUTHOR.saturating_add(u128::from(h & 0xF));
            self.live.insert(name.to_string(), id);
            return Ok(id);
        }
        let id = self.next;
        self.persist(&format!("A\t{id}\t{name}"))?;
        self.by_id.insert(
            id,
            Entry {
                name: name.to_string(),
                retired: false,
            },
        );
        self.live.insert(name.to_string(), id);
        self.next = id.saturating_add(1);
        Ok(id)
    }

    /// Refuses a registry that does not cover the account author ids found in the retained log: every
    /// id must be one the registry assigned (live or retired). Without this a deleted or older file
    /// restarts the counter at ids history already carries and two accounts would share one. Ids that
    /// vacuum already removed from the retained log cannot be checked.
    pub fn check_covers(&self, ids: &BTreeSet<u128>) -> Result<(), AuthorError> {
        if bugs::active(Bug::AuthorRegistryLogCheckSkipped) {
            return Ok(());
        }
        if ids
            .iter()
            .any(|id| *id >= self.next || !self.by_id.contains_key(id))
        {
            return Err(AuthorError::BehindLog);
        }
        Ok(())
    }

    /// The id of a live account, without assigning one.
    pub fn id_of(&self, name: &str) -> Option<u128> {
        self.live.get(name).copied()
    }

    /// The name an id was last known by, retired or not. History keeps showing ids; this names them.
    pub fn name_of(&self, id: u128) -> Option<&str> {
        self.by_id.get(&id).map(|e| e.name.as_str())
    }

    /// Whether the id belonged to an account that was retired.
    pub fn is_retired(&self, id: u128) -> bool {
        self.by_id.get(&id).is_some_and(|e| e.retired)
    }

    /// Gives the account `old` the name `new`, keeping its id. The only way an id follows a rename.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<u128, AuthorError> {
        let id = *self.live.get(old).ok_or(AuthorError::Unknown)?;
        if !valid_name(new) {
            return Err(AuthorError::InvalidName);
        }
        if self.live.contains_key(new) {
            return Err(AuthorError::Taken);
        }
        self.persist(&format!("R\t{id}\t{new}"))?;
        if let Some(entry) = self.by_id.get_mut(&id) {
            entry.name = new.to_string();
        }
        self.live.remove(old);
        self.live.insert(new.to_string(), id);
        Ok(id)
    }

    /// Ends the account `name`. Its id stays in the registry forever and is never handed out again.
    pub fn retire(&mut self, name: &str) -> Result<u128, AuthorError> {
        let id = *self.live.get(name).ok_or(AuthorError::Unknown)?;
        self.persist(&format!("D\t{id}"))?;
        if let Some(entry) = self.by_id.get_mut(&id) {
            entry.retired = true;
        }
        self.live.remove(name);
        Ok(id)
    }

    /// Live accounts and their ids, ordered by name.
    pub fn accounts(&self) -> impl Iterator<Item = (&str, u128)> {
        self.live.iter().map(|(n, id)| (n.as_str(), *id))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use lattice_core::bugs;
    use lattice_sim::{FaultProfile, SimIo};
    use lattice_testkit::scenario::SharedIo;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn disk() -> SharedIo {
        SharedIo::new(SimIo::new(1, FaultProfile::clean()))
    }

    fn open(disk: &SharedIo) -> AuthorRegistry {
        AuthorRegistry::open(Box::new(disk.clone())).unwrap()
    }

    fn put(disk: &SharedIo, text: &str) {
        let mut io = disk.clone();
        io.append(FILE, text.as_bytes()).unwrap();
        io.sync(FILE).unwrap();
    }

    #[test]
    fn ids_are_assigned_in_order_from_the_first_account_id_and_survive_a_reopen() {
        let d = disk();
        let mut reg = open(&d);
        assert_eq!(reg.author_for_account("alice").unwrap(), 256);
        assert_eq!(reg.author_for_account("bob").unwrap(), 257);
        assert_eq!(reg.author_for_account("alice").unwrap(), 256);
        drop(reg);
        let mut again = open(&d);
        assert_eq!(again.id_of("bob"), Some(257));
        assert_eq!(again.author_for_account("carol").unwrap(), 258);
        assert_eq!(again.name_of(256), Some("alice"));
    }

    #[test]
    fn reserved_ids_are_below_the_first_account_id() {
        assert_eq!(AUTHOR_UNKNOWN, 0);
        assert_eq!(AUTHOR_LOCAL_DEVELOPMENT, 1);
        let mut reg = AuthorRegistry::in_memory();
        // An account named like the development principal is an ordinary account.
        let id = reg.author_for_account("local-development").unwrap();
        assert!(id >= FIRST_ACCOUNT_AUTHOR && id != AUTHOR_LOCAL_DEVELOPMENT);
    }

    #[test]
    fn invalid_names_get_no_id() {
        let mut reg = AuthorRegistry::in_memory();
        for bad in [
            "",
            "a\nb",
            "tab\there",
            &"x".repeat(MAX_ACCOUNT_NAME_BYTES + 1),
        ] {
            assert_eq!(reg.author_for_account(bad), Err(AuthorError::InvalidName));
        }
        assert!(
            reg.author_for_account(&"x".repeat(MAX_ACCOUNT_NAME_BYTES))
                .is_ok()
        );
        assert_eq!(reg.accounts().count(), 1);
    }

    #[test]
    fn a_torn_final_line_is_dropped_and_its_id_is_not_remembered() {
        let d = disk();
        let mut reg = open(&d);
        reg.author_for_account("alice").unwrap();
        drop(reg);
        put(&d, "A\t257\tbo");
        let mut again = open(&d);
        assert_eq!(again.id_of("bo"), None);
        // The torn line was removed, so the next account gets the id the torn line wanted and the
        // file stays parseable.
        assert_eq!(again.author_for_account("bob").unwrap(), 257);
        drop(again);
        assert_eq!(open(&d).id_of("bob"), Some(257));
    }

    #[test]
    fn a_collision_in_the_file_refuses_startup() {
        let cases = [
            // one live name twice
            "A\t256\talice\nA\t257\talice\n",
            // one id twice
            "A\t256\talice\nA\t256\tbob\n",
            // an id below an earlier one
            "A\t300\talice\nA\t299\tbob\n",
            // an id in the reserved range
            "A\t5\talice\n",
            // a rename onto a live name
            "A\t256\talice\nA\t257\tbob\nR\t257\talice\n",
            // an operation on an id never assigned
            "D\t999\n",
            // junk
            "A\tx\talice\n",
            "Z\t256\talice\n",
        ];
        for body in cases {
            let d = disk();
            put(&d, &format!("{HEADER}{body}"));
            let result = AuthorRegistry::open(Box::new(d.clone()));
            assert!(
                matches!(result, Err(AuthorError::Corrupt { .. })),
                "accepted {body:?}"
            );
        }
        let d = disk();
        put(&d, "not a registry\n");
        assert!(matches!(
            AuthorRegistry::open(Box::new(d)),
            Err(AuthorError::Corrupt { line: 1 })
        ));
    }

    #[test]
    fn a_deleted_account_id_is_not_reissued() {
        let d = disk();
        let mut reg = open(&d);
        let alice = reg.author_for_account("alice").unwrap();
        assert_eq!(reg.retire("alice").unwrap(), alice);
        assert!(reg.is_retired(alice));
        assert_eq!(reg.id_of("alice"), None);
        // The same name later is a new account.
        let again = reg.author_for_account("alice").unwrap();
        let bob = reg.author_for_account("bob").unwrap();
        assert!(again > alice && bob > again);
        drop(reg);
        let mut reopened = open(&d);
        assert!(reopened.is_retired(alice));
        assert_eq!(reopened.id_of("alice"), Some(again));
        let carol = reopened.author_for_account("carol").unwrap();
        assert!(carol > bob);
        assert_eq!(reopened.name_of(alice), Some("alice"));
        assert_eq!(reopened.retire("zed"), Err(AuthorError::Unknown));
    }

    #[test]
    fn a_rename_keeps_the_id_only_through_the_explicit_call() {
        let d = disk();
        let mut reg = open(&d);
        let alice = reg.author_for_account("alice").unwrap();
        let bob = reg.author_for_account("bob").unwrap();
        assert_eq!(reg.rename("alice", "bob"), Err(AuthorError::Taken));
        assert_eq!(reg.rename("alice", ""), Err(AuthorError::InvalidName));
        assert_eq!(reg.rename("nobody", "x"), Err(AuthorError::Unknown));
        assert_eq!(reg.rename("alice", "alicia").unwrap(), alice);
        // The old name is free but is not the renamed account any more.
        let fresh = reg.author_for_account("alice").unwrap();
        assert!(fresh != alice && fresh != bob);
        drop(reg);
        let reopened = open(&d);
        assert_eq!(reopened.id_of("alicia"), Some(alice));
        assert_eq!(reopened.id_of("alice"), Some(fresh));
        assert_eq!(reopened.name_of(alice), Some("alicia"));
    }

    #[test]
    fn the_registry_stops_at_its_bound() {
        let mut reg = AuthorRegistry::in_memory();
        for i in 0..MAX_REGISTRY_IDS {
            reg.author_for_account(&format!("a{i}")).unwrap();
        }
        assert_eq!(reg.author_for_account("one-more"), Err(AuthorError::Full));
        // Retiring does not free an id for counting: the bound is on ids ever assigned.
        reg.retire("a0").unwrap();
        assert_eq!(reg.author_for_account("one-more"), Err(AuthorError::Full));
    }

    /// Fails the next append after writing half of it, as a full disk or a dying process would.
    struct HalfAppend {
        inner: SharedIo,
        armed: std::rc::Rc<std::cell::Cell<bool>>,
    }

    impl Io for HalfAppend {
        fn exists(&self, p: &str) -> bool {
            self.inner.exists(p)
        }
        fn len(&self, p: &str) -> Result<u64, lattice_seams::IoError> {
            self.inner.len(p)
        }
        fn read(&self, p: &str) -> Result<Vec<u8>, lattice_seams::IoError> {
            self.inner.read(p)
        }
        fn read_prefix(&self, p: &str, max: usize) -> Result<Vec<u8>, lattice_seams::IoError> {
            self.inner.read_prefix(p, max)
        }
        fn append(&mut self, p: &str, data: &[u8]) -> Result<(), lattice_seams::IoError> {
            if self.armed.replace(false) {
                self.inner.append(p, &data[..data.len() / 2])?;
                return Err(lattice_seams::IoError::Other("half".into()));
            }
            self.inner.append(p, data)
        }
        fn truncate(&mut self, p: &str, len: u64) -> Result<(), lattice_seams::IoError> {
            self.inner.truncate(p, len)
        }
        fn sync(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.inner.sync(p)
        }
        fn sync_dir(&mut self, d: &str) -> Result<(), lattice_seams::IoError> {
            self.inner.sync_dir(d)
        }
        fn list(&self, d: &str) -> Result<Vec<String>, lattice_seams::IoError> {
            self.inner.list(d)
        }
        fn remove(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.inner.remove(p)
        }
        fn create_dir_all(&mut self, d: &str) -> Result<(), lattice_seams::IoError> {
            self.inner.create_dir_all(d)
        }
        fn lock(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.inner.lock(p)
        }
        fn unlock(&mut self, p: &str) {
            self.inner.unlock(p)
        }
    }

    #[test]
    fn a_failed_write_assigns_nothing_and_leaves_no_partial_line() {
        let d = disk();
        let armed = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut reg = AuthorRegistry::open(Box::new(HalfAppend {
            inner: d.clone(),
            armed: armed.clone(),
        }))
        .unwrap();
        reg.author_for_account("alice").unwrap();
        armed.set(true);
        assert!(matches!(
            reg.author_for_account("bob"),
            Err(AuthorError::Io(_))
        ));
        assert_eq!(reg.id_of("bob"), None);
        // The half line was cut off, so it cannot glue onto the next one.
        assert_eq!(reg.author_for_account("bob").unwrap(), 257);
        drop(reg);
        let again = open(&d);
        assert_eq!(again.id_of("alice"), Some(256));
        assert_eq!(again.id_of("bob"), Some(257));
    }

    #[test]
    fn an_assigned_id_survives_a_power_cut() {
        let d = disk();
        let mut reg = open(&d);
        let id = reg.author_for_account("alice").unwrap();
        drop(reg);
        d.crash();
        assert_eq!(open(&d).id_of("alice"), Some(id));
    }

    proptest! {
        // The default reads `PROPTEST_CASES` (256 when unset); `with_cases` would ignore the knob.
        #![proptest_config(ProptestConfig::default())]

        #[test]
        fn author_id_is_injective_over_generated_account_sets(
            names in proptest::collection::vec(
                prop_oneof![
                    "[a-z]{1,6}",
                    // near collisions: case, a trailing space, a lookalike and a shared prefix
                    "[a-zA-Z]{1,3}( |_|-)?",
                    "(alice|bob|alicia|alice2|Alice|alice )",
                    "[\u{0430}-\u{044f}a-c]{1,3}",
                ],
                1..40,
            ),
            reopen_at in 0usize..40,
        ) {
            let d = disk();
            let mut reg = open(&d);
            let mut seen: BTreeMap<String, u128> = BTreeMap::new();
            for (i, name) in names.iter().enumerate() {
                if i == reopen_at {
                    drop(reg);
                    reg = open(&d);
                }
                let id = reg.author_for_account(name).unwrap();
                prop_assert!(id >= FIRST_ACCOUNT_AUTHOR);
                let before = seen.insert(name.clone(), id);
                // The same name keeps its id, also across the reopen.
                prop_assert!(before.is_none() || before == Some(id));
            }
            let ids: BTreeSet<u128> = seen.values().copied().collect();
            prop_assert_eq!(ids.len(), seen.len(), "two names share an id");
        }
    }

    #[test]
    fn the_truncated_hash_bug_is_caught_by_the_injectivity_check() {
        bugs::clear();
        bugs::enable(Bug::AuthorIdTruncatedHash);
        let mut reg = AuthorRegistry::in_memory();
        let mut ids = BTreeSet::new();
        let mut names = 0;
        for i in 0..64 {
            ids.insert(reg.author_for_account(&format!("account-{i}")).unwrap());
            names += 1;
        }
        bugs::clear();
        assert!(ids.len() < names, "the planted truncation was not visible");
    }

    #[test]
    fn a_registry_that_does_not_cover_the_log_is_refused() {
        let d = disk();
        let mut reg = open(&d);
        reg.author_for_account("alice").unwrap();
        reg.author_for_account("bob").unwrap();
        let log: BTreeSet<u128> = [256, 257].into();
        assert_eq!(reg.check_covers(&log), Ok(()));
        assert_eq!(reg.check_covers(&BTreeSet::new()), Ok(()));
        // A fresh registry, and one that lost its last line, are both behind the log.
        assert_eq!(
            AuthorRegistry::in_memory().check_covers(&log),
            Err(AuthorError::BehindLog)
        );
        assert_eq!(
            AuthorRegistry::in_memory().check_covers(&[300].into()),
            Err(AuthorError::BehindLog)
        );
        // A retired account's id still counts as known.
        reg.retire("bob").unwrap();
        assert_eq!(reg.check_covers(&log), Ok(()));
        bugs::clear();
        bugs::enable(Bug::AuthorRegistryLogCheckSkipped);
        let skipped = AuthorRegistry::in_memory().check_covers(&log);
        bugs::clear();
        assert_eq!(
            skipped,
            Ok(()),
            "the planted defect must be visible to the test above"
        );
    }

    #[test]
    fn a_log_id_the_registry_does_not_list_is_refused_below_and_above_the_counter() {
        // Counter 401, ids 256 and 400 listed: 300 is below the counter but was never assigned.
        let d = disk();
        put(&d, &format!("{HEADER}A\t256\talice\nA\t400\tbob\n"));
        let reg = open(&d);
        assert_eq!(reg.check_covers(&[256, 400].into()), Ok(()));
        assert_eq!(reg.check_covers(&[300].into()), Err(AuthorError::BehindLog));
        assert_eq!(
            reg.check_covers(&[256, 300, 400].into()),
            Err(AuthorError::BehindLog)
        );
        // Counter 259, ids 256..=258 listed: 300 is above the counter.
        let mut reg = AuthorRegistry::in_memory();
        for n in ["a", "b", "c"] {
            reg.author_for_account(n).unwrap();
        }
        assert_eq!(reg.check_covers(&[258].into()), Ok(()));
        assert_eq!(reg.check_covers(&[300].into()), Err(AuthorError::BehindLog));
    }
}
