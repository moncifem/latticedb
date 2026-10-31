//! A host for the SQL executor that connection threads can share.
//!
//! [`ExecHandler`] works on an `Rc<RefCell<Database>>`, so it lives on one thread. A listener with
//! one thread per connection cannot share that, so this module runs the database on one dedicated
//! thread (the *actor*). It keeps one [`ExecHandler`] per connection, created from the connection's
//! principal, and answers requests sent over a channel. A connection holds an [`ExecProxy`], which is
//! a `QueryHandler` that forwards each call and waits for the answer.
//!
//! What this is and is not:
//!
//! - **One statement at a time.** The actor runs statements serially, so a slow statement delays the
//!   others. The statement timeout (database clock) and its host ceiling bound that delay; set the
//!   ceiling. A cancel that arrives while a request is still queued is honoured before it starts.
//! - **Row policy follows the principal.** [`PolicyTable`] maps the development principal and each
//!   account name to a [`Grant`]. An account that the table does not list gets [`Grant::DenyAll`];
//!   there is no allow-all fallback anywhere.
//! - **The queue is bounded and nothing blocks on it.** A connection has at most one live request in
//!   the queue and waits for it to leave before sending the next. The wait to get in, to be answered
//!   and for room in a full queue share one budget (the ceiling plus slack, counted in waits that ran
//!   out, so it holds unless the actor keeps making progress); past it the connection gets `57014`,
//!   or `53300` if it never got in. Memory is bounded by `queue_depth`: a timed-out request stays
//!   queued until the actor takes it, so with a stuck actor and clients that time out and reconnect
//!   the queue can fill with stale entries, and overflow gets `53300`. A timed-out statement's abort
//!   and a closed connection are marks in shared state that the actor reads when it takes the
//!   connection's next request, not queue entries. That makes the abort deferred: an idle client
//!   that got `57014` keeps its executor block open until its next request or its close. The list
//!   of closed connections grows by one id per closed connection while the actor is stalled (it is
//!   emptied when the actor next takes a request), and the ids are only as many as the connections
//!   the listener admitted. Shutdown releases waiting connections and does not wait for a stuck
//!   actor for longer than [`HostLimits::shutdown_grace_ms`].
//! - **Connections own their transactions.** When a proxy is dropped the actor drops that
//!   connection's session; an open transaction block is abandoned without committing.
//! - **Persistence** is whatever the [`Database`] persists: see the `lattice-exec` README. Tables
//!   and their column definitions are recovered after a restart (#573).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lattice_core::Config;
use lattice_core::bugs::{self, Bug};
use lattice_engines::authors::AuthorRegistry;
use lattice_engines::grants::{GrantSpecError, TableGrants};
use lattice_engines::limits::{AllowAll, DenyAll};
use lattice_exec::{ActorPolicy, Database, ExecError, ExecLimits, TxStatus};
use lattice_seams::{Clock, Io, OsRng, Rng, StdIo, SystemClock};

use crate::cancel::CancellationToken;
use crate::exec_handler::ExecHandler;
use crate::notify::{CommitNotifier, DEFAULT_MAX_WAITERS};
use crate::pgwire::Column;
use crate::principal::{Principal, PrincipalKind, QueryContext};
use crate::session::{QueryError, QueryHandler, QueryOutput};
use crate::subscribe::SourceError;

/// Most account policy entries a policy file may hold (the credential store's own bound).
pub const MAX_POLICY_ENTRIES: usize = 1024;
/// Largest policy file in bytes.
pub const MAX_POLICY_BYTES: usize = 64 * 1024;

/// The permission granted to a principal. Per-table grants are interim: they stand in until the
/// Phase 4 policy language lands, and they have no row predicates, roles or groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Every row may be read and written, and the catalog may be changed.
    AllowAll,
    /// No row may be read or written, the catalog may not be changed, and no table is visible.
    DenyAll,
    /// Read, write and ddl bits on named tables and, optionally, on every table (`*`). A table the
    /// grants give no bit on is invisible: it reads as one that does not exist.
    Tables(TableGrants),
}

impl Grant {
    pub(crate) fn policy(&self) -> ActorPolicy {
        match self {
            Grant::AllowAll => ActorPolicy::new(Rc::new(AllowAll)),
            // A denied actor also gets an empty catalog, so it cannot probe names or columns.
            Grant::DenyAll => ActorPolicy::new(Rc::new(DenyAll)).hiding_catalog(),
            Grant::Tables(g) => ActorPolicy::with_table_grants(g.clone()),
        }
    }

    /// `allow-all`, `deny-all` or `tables`.
    pub fn name(&self) -> &'static str {
        match self {
            Grant::AllowAll => "allow-all",
            Grant::DenyAll => "deny-all",
            Grant::Tables(_) => "tables",
        }
    }

    /// Parses `allow-all` or `deny-all`: the two words the `--development-policy` flag takes.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allow-all" => Some(Grant::AllowAll),
            "deny-all" => Some(Grant::DenyAll),
            _ => None,
        }
    }

    /// Parses the policy column of an account line: `allow-all`, `deny-all` or
    /// `tables: name=bits, ...` (see [`TableGrants::parse`]).
    pub fn parse_account(text: &str) -> Result<Self, GrantParseError> {
        match text.strip_prefix(TABLES_PREFIX) {
            Some(spec) => TableGrants::parse(spec)
                .map(Grant::Tables)
                .map_err(GrantParseError::Tables),
            None => Grant::parse(text).ok_or(GrantParseError::Unknown),
        }
    }
}

/// The word that starts a table grant list in an account policy line.
pub const TABLES_PREFIX: &str = "tables:";

/// Why the policy column of an account line was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantParseError {
    /// Not `allow-all`, `deny-all` or a `tables:` list.
    Unknown,
    /// A `tables:` list that is not valid.
    Tables(GrantSpecError),
}

impl std::fmt::Display for GrantParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("the policy is not allow-all, deny-all or tables: ..."),
            Self::Tables(e) => e.fmt(f),
        }
    }
}

/// Why a policy file was refused. The message never echoes the offending line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyFileError {
    /// The file is larger than [`MAX_POLICY_BYTES`] or lists more than [`MAX_POLICY_ENTRIES`] accounts.
    OutOfBounds,
    /// The 1-based line is not `account<TAB>policy`, names an invalid account, has a policy that is
    /// not `allow-all`, `deny-all` or a `tables:` list, or repeats an account. The content is never
    /// echoed.
    Malformed {
        /// 1-based line number.
        line: usize,
    },
    /// The 1-based line has a `tables:` list that is not valid.
    BadGrant {
        /// 1-based line number.
        line: usize,
        /// What is wrong with it.
        reason: GrantSpecError,
    },
}

/// Which grant each principal gets. There is no default grant: the development principal is chosen
/// by the host, and an account missing from the table is denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyTable {
    development: Grant,
    accounts: BTreeMap<String, Grant>,
}

impl PolicyTable {
    /// A table whose development principal gets `development` and whose accounts are all denied
    /// until [`PolicyTable::with_accounts`] lists them.
    pub fn new(development: Grant) -> Self {
        PolicyTable {
            development,
            accounts: BTreeMap::new(),
        }
    }

    /// Sets the grants of the listed accounts.
    #[must_use]
    pub fn with_accounts(mut self, accounts: BTreeMap<String, Grant>) -> Self {
        self.accounts = accounts;
        self
    }

    /// The grant of the development principal.
    pub fn development(&self) -> Grant {
        self.development.clone()
    }

    /// The grant of a principal: the development grant for the local-development principal, the
    /// account's listed grant otherwise, [`Grant::DenyAll`] for an unlisted account. The same
    /// mapping the SQL handler uses; there is no allow-all fallback.
    pub fn grant_for(&self, principal: &Principal) -> Grant {
        match principal.kind() {
            PrincipalKind::LocalDevelopment => self.development(),
            PrincipalKind::Account => self.account(principal.name()),
        }
    }

    /// The grant of an account: the listed one, or [`Grant::DenyAll`].
    pub fn account(&self, name: &str) -> Grant {
        self.accounts.get(name).cloned().unwrap_or(Grant::DenyAll)
    }

    /// Parses `account<TAB>policy` lines, where the policy is `allow-all`, `deny-all` or
    /// `tables: name=bits, ...`. Blank lines are skipped. Duplicate accounts, unknown policies,
    /// malformed table grants, names over 63 bytes or with control characters, and oversize files are
    /// refused, each with the 1-based line it is on; nothing partial is returned.
    pub fn parse_accounts(text: &str) -> Result<BTreeMap<String, Grant>, PolicyFileError> {
        if text.len() > MAX_POLICY_BYTES {
            return Err(PolicyFileError::OutOfBounds);
        }
        let mut accounts = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let line_no = index + 1;
            let bad = PolicyFileError::Malformed { line: line_no };
            let (name, grant) = line.split_once('\t').ok_or(bad)?;
            if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
                return Err(bad);
            }
            let grant = Grant::parse_account(grant).map_err(|e| match e {
                GrantParseError::Unknown => bad,
                GrantParseError::Tables(reason) => PolicyFileError::BadGrant {
                    line: line_no,
                    reason,
                },
            })?;
            if accounts.len() == MAX_POLICY_ENTRIES {
                return Err(PolicyFileError::OutOfBounds);
            }
            if accounts.insert(name.to_string(), grant).is_some() {
                return Err(bad);
            }
        }
        Ok(accounts)
    }
}

/// Statement limits the host applies to every connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    /// Ceiling of the statement timeout in milliseconds of the database clock. It is also the
    /// default timeout, and a client's `SET statement_timeout` cannot lift it. `None` leaves
    /// statements unbounded, which lets one statement stall every connection.
    pub max_timeout_ms: Option<u64>,
    /// Bytes one statement may hold in result and working rows.
    pub memory_bytes: u64,
    /// Bytes the writes staged by one transaction may hold (`ExecLimits::transaction_bytes`).
    pub transaction_bytes: u64,
    /// Requests that may wait for the database thread; memory held by the queue is bounded by it.
    /// A connection keeps at most one live request queued, but a request that timed out stays queued
    /// until the thread takes it, even after its connection closed, so while the thread is stuck
    /// and clients time out and reconnect the queue can fill with stale entries. A sender that finds
    /// it full waits within its budget and then gets `53300`; no send blocks without a limit.
    pub queue_depth: usize,
    /// Milliseconds a connection waits for its answer beyond the statement ceiling. The wait covers
    /// getting into the queue as well as the answer.
    pub wait_slack_ms: u64,
    /// Milliseconds [`ExecHost::shutdown`] waits for the database thread to end before it gives up
    /// and reports it, leaving the thread behind.
    pub shutdown_grace_ms: u64,
    /// Threads that may sleep on the commit notifier at once ([`ExecHost::commit_notifier`]). The
    /// next waiter is refused with [`crate::notify::WaitError::Full`] and polls on a timer instead.
    pub commit_waiters: usize,
}

impl Default for HostLimits {
    fn default() -> Self {
        HostLimits {
            max_timeout_ms: Some(30_000),
            memory_bytes: 64 << 20,
            transaction_bytes: ExecLimits::default().transaction_bytes,
            queue_depth: 256,
            wait_slack_ms: 5_000,
            shutdown_grace_ms: 10_000,
            commit_waiters: DEFAULT_MAX_WAITERS,
        }
    }
}

impl HostLimits {
    fn exec(self) -> ExecLimits {
        ExecLimits {
            timeout_ms: self.max_timeout_ms,
            memory_bytes: self.memory_bytes,
            transaction_bytes: self.transaction_bytes,
            max_timeout_ms: self.max_timeout_ms,
        }
    }
}

enum Request {
    Execute {
        conn: u64,
        /// The connection's request number, rising by one per call.
        seq: u64,
        shared: Arc<ConnShared>,
        principal: Principal,
        sql: String,
        params: Vec<Option<Vec<u8>>>,
        token: CancellationToken,
        reply: SyncSender<Result<QueryOutput, QueryError>>,
        #[cfg(any(test, feature = "test-hooks"))]
        hold: Option<Hold>,
    },
    Describe {
        conn: u64,
        seq: u64,
        shared: Arc<ConnShared>,
        principal: Principal,
        sql: String,
        token: CancellationToken,
        reply: SyncSender<Result<Vec<Column>, QueryError>>,
    },
    /// One bounded read of the change feed for a principal (see [`crate::feed_source`]).
    FeedRead {
        principal: Principal,
        scope: String,
        after: Vec<u8>,
        limit: usize,
        scan: usize,
        /// Whether to decode each upsert's row into columns on the actor thread.
        decode: bool,
        token: CancellationToken,
        reply: SyncSender<Result<crate::feed_source::Chunk, SourceError>>,
    },
    /// Makes the actor look at the list of closed connections. At most one is queued at a time.
    Wake,
    Stop,
    #[cfg(any(test, feature = "test-hooks"))]
    Stall {
        entered: SyncSender<()>,
        release: Receiver<()>,
    },
    /// Panics the actor, so tests can observe a dead database thread.
    #[cfg(any(test, feature = "test-hooks"))]
    Crash,
}

/// A statement held between its dequeue and its run (tests only): the actor reports on `.0`, waits
/// for `.1` to close or receive, then runs the statement as if no cancel had been seen.
#[cfg(any(test, feature = "test-hooks"))]
type Hold = (SyncSender<()>, Receiver<()>);

/// Keeps the database thread busy until dropped (tests only).
#[cfg(any(test, feature = "test-hooks"))]
pub struct Stall(#[allow(dead_code)] SyncSender<()>);

/// State every connection and the actor share. Nothing in it can block a sender.
struct HostShared {
    /// The newest committed LSN, published by the actor after every statement. Closed when the
    /// actor ends or shutdown begins, so no waiter outlives the host.
    notifier: Arc<CommitNotifier>,
    /// Set when shutdown begins: waits and sends give up, and the actor stops at its next request.
    stopping: AtomicBool,
    /// Requests in the channel. Signed because the actor can take a request before its sender has
    /// counted it.
    queued: AtomicIsize,
    /// The most requests the channel held at once (tests read it).
    peak: AtomicIsize,
    /// Connections whose proxy was dropped; the actor removes their sessions.
    closed: Mutex<Vec<u64>>,
    closed_pending: AtomicBool,
    wake_queued: AtomicBool,
    /// Senders that found the queue full wait here; the actor signals after every request it takes.
    space: Mutex<()>,
    space_cv: Condvar,
    /// Set when the actor thread has ended, however it ended.
    exited: Mutex<bool>,
    exited_cv: Condvar,
}

impl HostShared {
    fn new(commit_waiters: usize) -> Self {
        HostShared {
            notifier: Arc::new(CommitNotifier::new(commit_waiters)),
            stopping: AtomicBool::new(false),
            queued: AtomicIsize::new(0),
            peak: AtomicIsize::new(0),
            closed: Mutex::new(Vec::new()),
            closed_pending: AtomicBool::new(false),
            wake_queued: AtomicBool::new(false),
            space: Mutex::new(()),
            space_cv: Condvar::new(),
            exited: Mutex::new(false),
            exited_cv: Condvar::new(),
        }
    }
}

/// How long a wait may last, counted in the waits that ran out. The server code may not read a
/// clock, so a wait that is cut short by a signal costs nothing and one that times out costs its
/// whole slice. Every signal that ends a wait early is progress by the thread being waited on, so
/// the bound holds unless the actor keeps progressing (and then the wait is making headway).
struct Budget(Option<Duration>);

impl Budget {
    fn new(total: Option<Duration>) -> Self {
        Budget(total)
    }

    fn spent() -> Self {
        Budget(Some(Duration::ZERO))
    }

    /// The length of the next wait, or `None` when the budget is used up.
    fn next(&self) -> Option<Duration> {
        match self.0 {
            None => Some(WAIT_SLICE),
            Some(left) if left.is_zero() => None,
            Some(left) => Some(left.min(WAIT_SLICE)),
        }
    }

    fn timed_out(&mut self, waited: Duration) {
        if let Some(left) = &mut self.0 {
            *left = left.saturating_sub(waited);
        }
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// State one connection shares with the actor. It carries what used to be separate `Abort` and
/// `Close` requests, so a timed-out statement queues nothing but itself.
#[derive(Default)]
struct ConnShared {
    /// Requests of this connection still in the channel (0 or 1).
    queued: Mutex<u32>,
    left: Condvar,
    /// Highest request number whose timeout the client was told about. The actor fails or discards
    /// the open block before it runs any later request of this connection, which is what the queued
    /// `Abort` request did.
    abort_through: AtomicU64,
    /// Highest request number that never reached the queue (`53300`). The client's block failed, so
    /// the executor's block must too.
    fail_through: AtomicU64,
    closed: AtomicBool,
}

impl ConnShared {
    fn enter(&self) {
        *locked(&self.queued) += 1;
    }

    fn leave(&self) {
        let mut queued = locked(&self.queued);
        *queued = queued.saturating_sub(1);
        self.left.notify_all();
    }

    /// Waits until the connection's earlier request has left the queue, so a client that keeps
    /// retrying while the actor is busy cannot stack entries up.
    fn wait_clear(&self, budget: &mut Budget, host: &HostShared) -> Result<(), QueryError> {
        let mut queued = locked(&self.queued);
        while *queued > 0 {
            // A dead actor never takes the entry: answer 57P01, not a wait that ends in 53300 (or
            // never ends, with an unbounded budget).
            if host.stopping.load(Ordering::Acquire) || *locked(&host.exited) {
                return Err(gone());
            }
            let Some(slice) = budget.next() else {
                return Err(busy(
                    "an earlier request of this connection is still queued",
                ));
            };
            let (guard, result) = self
                .left
                .wait_timeout(queued, slice)
                .unwrap_or_else(PoisonError::into_inner);
            queued = guard;
            if result.timed_out() {
                budget.timed_out(slice);
            }
        }
        Ok(())
    }
}

/// How often a waiting thread looks at the shutdown flag.
const WAIT_SLICE: Duration = Duration::from_millis(50);

fn busy(why: &str) -> QueryError {
    QueryError::new("53300", format!("the SQL engine is busy: {why}"))
}

/// Puts a request in the channel. When the queue is full it waits for the actor to take something,
/// as long as `budget` allows (until shutdown, for an unbounded budget). `Full` is not a reason to
/// block for good: the actor may be stuck.
fn enqueue(
    requests: &SyncSender<Request>,
    host: &HostShared,
    request: Request,
    budget: &mut Budget,
) -> Result<(), QueryError> {
    let mut request = request;
    loop {
        match requests.try_send(request) {
            Ok(()) => {
                let now = host.queued.fetch_add(1, Ordering::AcqRel) + 1;
                host.peak.fetch_max(now, Ordering::AcqRel);
                return Ok(());
            }
            Err(TrySendError::Disconnected(_)) => return Err(gone()),
            Err(TrySendError::Full(back)) => request = back,
        }
        if host.stopping.load(Ordering::Acquire) {
            return Err(gone());
        }
        let Some(slice) = budget.next() else {
            return Err(busy("the request queue is full"));
        };
        // The actor signals while holding this lock after every request it takes, so a take between
        // the failed send above and the wait below is seen by the retry made under the lock.
        let guard = locked(&host.space);
        match requests.try_send(request) {
            Ok(()) => {
                let now = host.queued.fetch_add(1, Ordering::AcqRel) + 1;
                host.peak.fetch_max(now, Ordering::AcqRel);
                return Ok(());
            }
            Err(TrySendError::Disconnected(_)) => return Err(gone()),
            Err(TrySendError::Full(back)) => request = back,
        }
        let (_guard, result) = host
            .space_cv
            .wait_timeout(guard, slice)
            .unwrap_or_else(PoisonError::into_inner);
        if result.timed_out() {
            budget.timed_out(slice);
        }
    }
}

/// Marks the actor as gone however its thread ends (normal stop, error or panic).
struct AliveGuard(Arc<AtomicBool>, Arc<HostShared>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
        // Waiters on the notifier must not sleep for a database that is gone.
        self.1.notifier.close();
        *locked(&self.1.exited) = true;
        self.1.exited_cv.notify_all();
    }
}

/// Makes sure `authors.reg` exists and is not open to group or others: it maps account names to the
/// ids that appear in history. It is created empty (the `Io` seam has no mode argument) and narrowed to 0600
/// at once, inside the 0700 data directory; a symlink in its place is refused; an existing file with wider permissions is refused, as `latticed` refuses a wide directory.
fn private_registry_file(dir: &std::path::Path) -> Result<(), String> {
    let path = dir.join("authors.reg");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "{}: the author registry must not be a symbolic link",
            path.display()
        ));
    }
    if !path.exists() {
        let mut io = StdIo::new(dir.to_path_buf());
        io.append("authors.reg", b"")
            .map_err(|e| format!("cannot create the author registry: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .map_err(|e| format!("cannot read the author registry: {e}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            let empty = std::fs::metadata(&path).is_ok_and(|m| m.len() == 0);
            if !empty {
                return Err(format!(
                    "{}: the author registry must not be accessible to group or others (mode 0600)",
                    path.display()
                ));
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("cannot restrict the author registry: {e}"))?;
        }
    }
    Ok(())
}

/// The running database actor. Share it behind an `Arc`; every method takes `&self`.
pub struct ExecHost {
    requests: SyncSender<Request>,
    next_connection: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
    alive: Arc<AtomicBool>,
    shared: Arc<HostShared>,
    /// How long a connection waits to be queued and answered: the statement ceiling plus slack, or
    /// until shutdown.
    wait: Option<Duration>,
    shutdown_grace: Duration,
}

impl ExecHost {
    /// Opens the database in `directory` (which must exist) with the real file system, clock and
    /// randomness, recovers it from its log, and starts the actor. Fails if the log cannot be
    /// opened or another process holds the directory.
    pub fn start_dir(
        directory: PathBuf,
        policies: PolicyTable,
        limits: HostLimits,
    ) -> Result<Self, String> {
        Self::start_dir_with(directory, policies, limits, false)
    }

    /// Like [`ExecHost::start_dir`], and `allow_format_upgrade` lets the first `CREATE TABLE` of a
    /// data directory that earlier builds can still open write the table definition (RFC 0012). That
    /// adds a required capability to the log, so builds without it refuse the directory afterwards
    /// and there is no downgrade: the caller must have a verified backup. Without it such a
    /// directory refuses `CREATE TABLE` (SQLSTATE 55000). A new, empty directory never needs it.
    pub fn start_dir_with(
        directory: PathBuf,
        policies: PolicyTable,
        limits: HostLimits,
        allow_format_upgrade: bool,
    ) -> Result<Self, String> {
        // The executor reports every storage failure as an opaque internal error, so name the likely
        // causes here: an unreadable or corrupt log, or another process holding the directory.
        let hint = |e: String| {
            format!(
                "cannot open or recover the data directory ({e}); the log may be damaged or another server may be using the directory"
            )
        };
        let authors_dir = directory.clone();
        Self::start_with_authors(
            move || {
                let mut db = Database::open(
                    StdIo::new(directory),
                    SystemClock,
                    OsRng,
                    &Config::default(),
                )?;
                db.set_allow_format_upgrade(allow_format_upgrade);
                Ok(db)
            },
            // The registry of author ids lives beside the log. It is opened after the log, so the
            // directory lock is already held, and a damaged or colliding registry refuses startup.
            move || {
                private_registry_file(&authors_dir)?;
                AuthorRegistry::open(Box::new(StdIo::new(authors_dir)))
                    .map_err(|e| format!("cannot open the author registry: {e}"))
            },
            policies,
            limits,
        )
        .map_err(hint)
    }

    /// Starts the actor with a database built on the actor's own thread, because the executor is
    /// single-threaded. Returns once the database is open (or failed to open).
    pub fn start<I, C, R>(
        open: impl FnOnce() -> Result<Database<I, C, R>, ExecError> + Send + 'static,
        policies: PolicyTable,
        limits: HostLimits,
    ) -> Result<Self, String>
    where
        I: Io + 'static,
        C: Clock + Clone + 'static,
        R: Rng + 'static,
    {
        Self::start_with_authors(open, || Ok(AuthorRegistry::in_memory()), policies, limits)
    }

    /// Like [`ExecHost::start`], with the registry that assigns account author ids opened on the
    /// actor's thread after the database. [`ExecHost::start`] keeps the ids in memory, so they are
    /// injective for the life of the process only; `start_dir` stores them beside the log.
    pub fn start_with_authors<I, C, R>(
        open: impl FnOnce() -> Result<Database<I, C, R>, ExecError> + Send + 'static,
        open_authors: impl FnOnce() -> Result<AuthorRegistry, String> + Send + 'static,
        policies: PolicyTable,
        limits: HostLimits,
    ) -> Result<Self, String>
    where
        I: Io + 'static,
        C: Clock + Clone + 'static,
        R: Rng + 'static,
    {
        let (requests, inbox) = mpsc::sync_channel(limits.queue_depth.max(1));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let shared = Arc::new(HostShared::new(limits.commit_waiters));
        let (thread_alive, thread_shared) = (alive.clone(), shared.clone());
        let thread = thread::Builder::new()
            .name("sql-actor".into())
            .spawn(move || {
                let _guard = AliveGuard(thread_alive, thread_shared.clone());
                let db = match open() {
                    Ok(db) => db,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let authors = match open_authors() {
                    Ok(authors) => authors,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // A registry that is missing or older than the log would hand out ids that history
                // already carries. Refuse before serving anyone.
                if let Err(e) = authors.check_covers(&db.store().account_authors()) {
                    let _ = ready_tx.send(Err(format!("author registry: {e}")));
                    return;
                }
                let authors = Rc::new(RefCell::new(authors));
                // A restarted database starts with its recovered head, so a waiter that has seen
                // nothing is told at once that there is something to read.
                thread_shared.notifier.publish(committed_lsn(&db));
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                serve(db, authors, inbox, &policies, limits, &thread_shared);
            })
            .map_err(|e| format!("cannot start the SQL thread: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(ExecHost {
                requests,
                next_connection: AtomicU64::new(1),
                thread: Mutex::new(Some(thread)),
                alive,
                shared,
                wait: limits
                    .max_timeout_ms
                    .map(|ms| Duration::from_millis(ms.saturating_add(limits.wait_slack_ms))),
                shutdown_grace: Duration::from_millis(limits.shutdown_grace_ms),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the SQL thread stopped while opening".to_string())
            }
        }
    }

    /// A handler for one connection. Cheap; the connection's session starts with its first statement.
    pub fn handler(&self) -> ExecProxy {
        ExecProxy {
            requests: self.requests.clone(),
            host: Arc::clone(&self.shared),
            state: Arc::new(ConnShared::default()),
            conn: self.next_connection.fetch_add(1, Ordering::Relaxed),
            seq: 0,
            used: false,
            wait: self.wait,
            #[cfg(any(test, feature = "test-hooks"))]
            hold: None,
        }
    }

    /// A change-feed reader that asks the actor on behalf of principals. `max_events` bounds how many
    /// events one read returns (and so how long one read holds the actor with the returned work);
    /// it is raised to 1.
    pub fn feed_source(&self, max_events: usize) -> crate::feed_source::FeedSource {
        crate::feed_source::FeedSource::new(
            FeedHandle {
                requests: self.requests.clone(),
                host: Arc::clone(&self.shared),
                wait: self.wait,
            },
            max_events,
        )
    }

    /// The commit notifier: wait on it for "something committed past LSN `x`" instead of polling
    /// the feed source on a timer. It carries only an LSN; see [`crate::notify`].
    pub fn commit_notifier(&self) -> Arc<CommitNotifier> {
        Arc::clone(&self.shared.notifier)
    }

    /// False once the database thread has ended for any reason, including a panic. A supervisor
    /// should treat an unrequested end as a failed server.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Blocks the database thread until the returned guard is dropped; returns once it is blocked
    /// (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn stall_for_test(&self) -> Stall {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let _ = enqueue(
            &self.requests,
            &self.shared,
            Request::Stall {
                entered: entered_tx,
                release: release_rx,
            },
            &mut Budget::new(None),
        );
        let _ = entered_rx.recv();
        Stall(release_tx)
    }

    /// The most requests the queue has held at once (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn peak_queue_for_test(&self) -> usize {
        usize::try_from(self.shared.peak.load(Ordering::Acquire)).unwrap_or(0)
    }

    /// Panics the database thread (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn crash_for_test(&self) {
        let _ = enqueue(
            &self.requests,
            &self.shared,
            Request::Crash,
            &mut Budget::new(None),
        );
    }

    /// Tells every waiting connection to give up (`57P01`) and the actor to stop at its next
    /// request. It returns at once; call it before the listener joins its connection threads, so a
    /// thread waiting on a stuck actor does not hold the shutdown up.
    pub fn begin_shutdown(&self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.notifier.close();
    }

    /// Stops the actor and drops the database, which releases the directory lock. Handlers created
    /// earlier answer `57P01` afterwards. Call it after the listener has stopped.
    ///
    /// Requests still queued when this is called are answered `57P01` without running, so shutdown
    /// waits for at most the statement that is running, and for at most
    /// [`HostLimits::shutdown_grace_ms`]: if the database thread is stuck past that, this returns an
    /// error and leaves the thread behind rather than hanging. Returns an error if the database
    /// thread panicked.
    pub fn shutdown(&self) -> Result<(), String> {
        self.begin_shutdown();
        // The stop flag already makes the actor leave at its next request; the request only wakes
        // an idle actor, so a full queue is not worth waiting long for.
        let _ = enqueue(
            &self.requests,
            &self.shared,
            Request::Stop,
            &mut Budget::new(Some(self.shutdown_grace / 4)),
        );
        let thread = self.thread.lock().ok().and_then(|mut t| t.take());
        let Some(thread) = thread else { return Ok(()) };
        let mut budget = Budget::new(Some(self.shutdown_grace));
        let mut exited = locked(&self.shared.exited);
        while !*exited {
            let Some(slice) = budget.next() else {
                return Err(format!(
                    "the SQL thread did not stop within {} ms; not waiting for it",
                    self.shutdown_grace.as_millis()
                ));
            };
            let (guard, result) = self
                .shared
                .exited_cv
                .wait_timeout(exited, slice)
                .unwrap_or_else(PoisonError::into_inner);
            exited = guard;
            if result.timed_out() {
                budget.timed_out(slice);
            }
        }
        drop(exited);
        thread
            .join()
            .map_err(|_| "the SQL thread panicked".to_string())
    }
}

impl Drop for ExecHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The newest commit of any kind the database has applied (row writes and catalog changes), which
/// is what a subscriber can find by reading. Catalog commits move the catalog LSN, so both count.
fn committed_lsn<I, C, R>(db: &Database<I, C, R>) -> u64
where
    I: Io + 'static,
    C: Clock + Clone + 'static,
    R: Rng + 'static,
{
    db.store()
        .applied_lsn()
        .max(db.store().catalog().applied_lsn())
}

fn serve<I, C, R>(
    db: Database<I, C, R>,
    authors: Rc<RefCell<AuthorRegistry>>,
    inbox: Receiver<Request>,
    policies: &PolicyTable,
    limits: HostLimits,
    host: &HostShared,
) where
    I: Io + 'static,
    C: Clock + Clone + 'static,
    R: Rng + 'static,
{
    let db = Rc::new(RefCell::new(db));
    let mut sessions: BTreeMap<u64, ExecHandler<I, C, R>> = BTreeMap::new();
    // Whether each connection was inside a block when its latest statement was dequeued. A timed-out
    // statement from outside a block is a `BEGIN` or an autocommit statement: the client is outside any
    // block, so a block it left behind is discarded, not failed.
    let mut was_in_block: BTreeMap<u64, bool> = BTreeMap::new();
    // The abort and fail marks already applied per connection.
    let mut applied: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    let make = |db: &Rc<RefCell<Database<I, C, R>>>| {
        let accounts = policies.clone();
        let mut handler =
            ExecHandler::new(Rc::clone(db), policies.development().policy(), move |n| {
                accounts.account(n).policy()
            });
        handler.set_limits(limits.exec());
        handler.set_authors(Rc::clone(&authors));
        handler
    };
    while let Ok(request) = inbox.recv() {
        host.queued.fetch_sub(1, Ordering::AcqRel);
        drop(locked(&host.space));
        host.space_cv.notify_all();
        if host.stopping.load(Ordering::Acquire) {
            // Queued requests are dropped unanswered; their connections read that as 57P01.
            break;
        }
        if matches!(request, Request::Wake) {
            host.wake_queued.store(false, Ordering::Release);
        }
        if host.closed_pending.swap(false, Ordering::AcqRel) {
            for conn in std::mem::take(&mut *locked(&host.closed)) {
                sessions.remove(&conn);
                was_in_block.remove(&conn);
                applied.remove(&conn);
            }
        }
        match request {
            Request::Execute {
                conn,
                seq,
                shared,
                principal,
                sql,
                params,
                token,
                reply,
                #[cfg(any(test, feature = "test-hooks"))]
                hold,
            } => {
                #[cfg(any(test, feature = "test-hooks"))]
                let mut token = token;
                shared.leave();
                if shared.closed.load(Ordering::Acquire) {
                    let _ = reply.send(Err(gone()));
                    continue;
                }
                apply_marks(
                    &shared,
                    conn,
                    seq,
                    &mut sessions,
                    &was_in_block,
                    &mut applied,
                );
                was_in_block.insert(
                    conn,
                    sessions
                        .get(&conn)
                        .is_some_and(|h| h.tx_status() != TxStatus::Idle),
                );
                let result = if token.is_cancelled() {
                    // The client is told 57014, so a block it is in must fail here too.
                    if let Some(handler) = sessions.get_mut(&conn) {
                        handler.abort_transaction();
                    }
                    Err(cancelled())
                } else {
                    if bugs::active(Bug::NotifyBeforeDurable) {
                        // The planted defect announces the commit this statement is about to make.
                        host.notifier.publish(committed_lsn(&db.borrow()) + 1);
                    }
                    #[cfg(any(test, feature = "test-hooks"))]
                    if let Some((entered, release)) = hold {
                        let _ = entered.send(());
                        let _ = release.recv();
                        // The statement goes on as if its cancel had not been seen.
                        token = CancellationToken::default();
                    }
                    let handler = sessions.entry(conn).or_insert_with(|| make(&db));
                    handler.execute_cancellable(
                        &QueryContext::new(&principal),
                        &sql,
                        &params,
                        &token,
                    )
                };
                // Whatever the statement committed is durable by now (the kernel acknowledged it
                // before the row store applied it), including a statement whose client already
                // gave up. Publish before answering, so a commit is never announced late relative
                // to its acknowledgement.
                host.notifier.publish(committed_lsn(&db.borrow()));
                let _ = reply.send(result);
            }
            Request::Describe {
                conn,
                seq,
                shared,
                principal,
                sql,
                token,
                reply,
            } => {
                shared.leave();
                if shared.closed.load(Ordering::Acquire) {
                    let _ = reply.send(Err(gone()));
                    continue;
                }
                apply_marks(
                    &shared,
                    conn,
                    seq,
                    &mut sessions,
                    &was_in_block,
                    &mut applied,
                );
                was_in_block.insert(
                    conn,
                    sessions
                        .get(&conn)
                        .is_some_and(|h| h.tx_status() != TxStatus::Idle),
                );
                let result = if token.is_cancelled() {
                    if let Some(handler) = sessions.get_mut(&conn) {
                        handler.abort_transaction();
                    }
                    Err(cancelled())
                } else {
                    let handler = sessions.entry(conn).or_insert_with(|| make(&db));
                    handler.describe_cancellable(&QueryContext::new(&principal), &sql, &token)
                };
                let _ = reply.send(result);
            }
            Request::FeedRead {
                principal,
                scope,
                after,
                limit,
                scan,
                decode,
                token,
                reply,
            } => {
                let result = if token.is_cancelled() {
                    // The reader gave up before the read started; nothing is read.
                    Err(SourceError::Unavailable)
                } else {
                    let guard = db.borrow();
                    crate::feed_source::read_feed_chunk(
                        &guard, policies, &principal, &scope, &after, limit, scan,
                    )
                    .map(|mut chunk| {
                        if decode {
                            chunk.columns =
                                crate::feed_source::decode_columns(guard.schema(), &chunk.events);
                        }
                        chunk
                    })
                };
                let _ = reply.send(result);
            }
            Request::Wake => {}
            #[cfg(any(test, feature = "test-hooks"))]
            Request::Stall { entered, release } => {
                let _ = entered.send(());
                let _ = release.recv();
            }
            Request::Stop => break,
            #[cfg(any(test, feature = "test-hooks"))]
            #[allow(clippy::panic)]
            Request::Crash => panic!("planted failure of the SQL thread"),
        }
    }
}

/// Applies the failure marks a connection left since its previous request, before its next one
/// runs. A timed-out statement that began outside a block was a `BEGIN` or an autocommit statement:
/// the client believes it is outside any block, so a block it left behind is discarded. Inside a
/// block, the block fails so a later COMMIT rolls back. A request that never reached the queue
/// always fails the block.
fn apply_marks<I, C, R>(
    shared: &ConnShared,
    conn: u64,
    seq: u64,
    sessions: &mut BTreeMap<u64, ExecHandler<I, C, R>>,
    was_in_block: &BTreeMap<u64, bool>,
    applied: &mut BTreeMap<u64, (u64, u64)>,
) where
    I: Io + 'static,
    C: Clock + Clone + 'static,
    R: Rng + 'static,
{
    let abort = shared.abort_through.load(Ordering::Acquire);
    let fail = shared.fail_through.load(Ordering::Acquire);
    let (done_abort, done_fail) = applied.get(&conn).copied().unwrap_or((0, 0));
    let abort_due = abort > done_abort && abort < seq;
    let fail_due = fail > done_fail && fail < seq;
    if !abort_due && !fail_due {
        return;
    }
    if let Some(handler) = sessions.get_mut(&conn) {
        if abort_due {
            if was_in_block.get(&conn).copied().unwrap_or(false) {
                handler.abort_transaction();
            } else {
                handler.discard_transaction();
            }
        }
        if fail_due {
            handler.abort_transaction();
        }
    }
    applied.insert(
        conn,
        (
            if abort_due { abort } else { done_abort },
            if fail_due { fail } else { done_fail },
        ),
    );
}

fn cancelled() -> QueryError {
    QueryError::new("57014", "canceling statement due to user request")
}

fn gone() -> QueryError {
    QueryError::new("57P01", "the SQL engine is not running")
}

/// The `QueryHandler` of one connection: forwards to the [`ExecHost`] actor and waits.
pub struct ExecProxy {
    requests: SyncSender<Request>,
    host: Arc<HostShared>,
    state: Arc<ConnShared>,
    wait: Option<Duration>,
    conn: u64,
    /// Number of the latest request.
    seq: u64,
    /// Set once a request was sent, so a connection that never ran SQL costs the actor nothing.
    used: bool,
    #[cfg(any(test, feature = "test-hooks"))]
    hold: Option<Hold>,
}

impl ExecProxy {
    /// Makes this proxy's next statement stop between dequeue and run until `release` closes
    /// (tests only). `entered` is signalled when it has stopped.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn hold_next_for_test(&mut self, entered: SyncSender<()>, release: Receiver<()>) {
        self.hold = Some((entered, release));
    }

    /// Replaces how long this connection waits for an answer (tests only). A test arms a short wait
    /// just before the one statement that must time out and restores a generous one straight after,
    /// so set-up statements and the release of a held statement never race the wall clock.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_wait_for_test(&mut self, wait: Option<Duration>) {
        self.wait = wait;
    }

    /// Sends a request and waits for its answer, at most the statement ceiling plus slack in all:
    /// the wait for the connection's earlier request to leave the queue, for room in the queue, and
    /// for the answer share that one deadline. Nothing here blocks without it.
    ///
    /// On timeout the statement's token is cancelled: the actor skips the request if it has not
    /// started and stops it at its next row if it has, and this connection gets `57014`. The actor
    /// is told through the connection's shared state, not through the queue, so the timeout adds no
    /// entry: it fails or discards the open block before the connection's next request runs. Without
    /// that a later COMMIT would commit the rest of the block. A request that cannot be queued in
    /// time gets `53300`, and marks the block failed the same way.
    fn call<T>(
        &mut self,
        token: &CancellationToken,
        build: impl FnOnce(SyncSender<Result<T, QueryError>>, u64, Arc<ConnShared>) -> Request,
    ) -> Result<T, QueryError> {
        let mut budget = Budget::new(self.wait);
        self.seq += 1;
        let seq = self.seq;
        let (reply, answer) = mpsc::sync_channel(1);
        let queued = self
            .state
            .wait_clear(&mut budget, &self.host)
            .and_then(|()| {
                self.state.enter();
                let request = build(reply, seq, Arc::clone(&self.state));
                enqueue(&self.requests, &self.host, request, &mut budget).inspect_err(|_| {
                    self.state.leave();
                })
            });
        if let Err(error) = queued {
            if error.code == "53300" {
                self.state.fail_through.fetch_max(seq, Ordering::AcqRel);
            }
            return Err(error);
        }
        self.used = true;
        loop {
            let Some(slice) = budget.next() else {
                return Err(self.timed_out(token, seq));
            };
            match answer.recv_timeout(slice) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {
                    budget.timed_out(slice);
                    if self.host.stopping.load(Ordering::Acquire) {
                        return Err(gone());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(gone()),
            }
        }
    }

    fn timed_out(&self, token: &CancellationToken, seq: u64) -> QueryError {
        token.request();
        self.state.abort_through.fetch_max(seq, Ordering::AcqRel);
        QueryError::new(
            "57014",
            "canceling statement: no answer from the database within the statement ceiling",
        )
    }
}

impl QueryHandler for ExecProxy {
    fn execute(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        self.execute_cancellable(ctx, sql, params, &CancellationToken::default())
    }

    fn describe(&mut self, ctx: &QueryContext<'_>, sql: &str) -> Result<Vec<Column>, QueryError> {
        self.describe_cancellable(ctx, sql, &CancellationToken::default())
    }

    fn execute_cancellable(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
        cancellation: &CancellationToken,
    ) -> Result<QueryOutput, QueryError> {
        let conn = self.conn;
        let principal = ctx.principal.clone();
        #[cfg(any(test, feature = "test-hooks"))]
        let hold = self.hold.take();
        self.call(cancellation, |reply, seq, shared| Request::Execute {
            #[cfg(any(test, feature = "test-hooks"))]
            hold,
            conn,
            seq,
            shared,
            principal,
            sql: sql.to_string(),
            params: params.to_vec(),
            token: cancellation.clone(),
            reply,
        })
    }

    fn abort_transaction(&mut self, _ctx: &QueryContext<'_>) {
        self.fail_block();
    }

    fn describe_cancellable(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Column>, QueryError> {
        let conn = self.conn;
        let principal = ctx.principal.clone();
        self.call(cancellation, |reply, seq, shared| Request::Describe {
            conn,
            seq,
            shared,
            principal,
            sql: sql.to_string(),
            token: cancellation.clone(),
            reply,
        })
    }
}

impl ExecProxy {
    /// Marks the connection's block failed without a queue entry: the actor applies the mark before
    /// it runs the connection's next request, and the actor takes requests in order, so every request
    /// already sent comes first.
    fn fail_block(&self) {
        self.state
            .fail_through
            .fetch_max(self.seq, Ordering::AcqRel);
    }
}

impl Drop for ExecProxy {
    fn drop(&mut self) {
        if !self.used {
            return;
        }
        // The actor removes the session when it next looks at the list. Waking it takes a queue
        // slot only if none is taken already; when the queue is full the actor is busy and will
        // look soon anyway.
        self.state.closed.store(true, Ordering::Release);
        locked(&self.host.closed).push(self.conn);
        self.host.closed_pending.store(true, Ordering::Release);
        if !self.host.wake_queued.swap(true, Ordering::AcqRel)
            && enqueue(
                &self.requests,
                &self.host,
                Request::Wake,
                &mut Budget::spent(),
            )
            .is_err()
        {
            self.host.wake_queued.store(false, Ordering::Release);
        }
    }
}

/// The sending half of a feed reader: queues one [`Request::FeedRead`] and waits for its answer
/// under the same budget a statement gets (ceiling plus slack). Every failure to get an answer is
/// [`SourceError::Unavailable`], which the subscription host reports as `internal`.
#[derive(Clone)]
pub(crate) struct FeedHandle {
    requests: SyncSender<Request>,
    host: Arc<HostShared>,
    wait: Option<Duration>,
}

impl FeedHandle {
    pub(crate) fn read(
        &self,
        principal: &Principal,
        scope: &str,
        after: &[u8],
        limit: usize,
        scan: usize,
        decode: bool,
    ) -> Result<crate::feed_source::Chunk, SourceError> {
        let mut budget = Budget::new(self.wait);
        let token = CancellationToken::default();
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Request::FeedRead {
            principal: principal.clone(),
            scope: scope.to_string(),
            after: after.to_vec(),
            limit,
            scan,
            decode,
            token: token.clone(),
            reply,
        };
        enqueue(&self.requests, &self.host, request, &mut budget)
            .map_err(|_| SourceError::Unavailable)?;
        loop {
            let Some(slice) = budget.next() else {
                // Not started yet: the actor skips it. Started: it finishes and the answer is dropped.
                token.request();
                return Err(SourceError::Unavailable);
            };
            match answer.recv_timeout(slice) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {
                    budget.timed_out(slice);
                    if self.host.stopping.load(Ordering::Acquire) {
                        return Err(SourceError::Unavailable);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(SourceError::Unavailable),
            }
        }
    }
}

// Connection threads share the host and own their proxies; keep both facts compiled.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ExecHost>();
    fn assert_send<T: Send>() {}
    assert_send::<ExecProxy>();
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::pgwire::{Backend, Frontend, Startup};
    use crate::session::Session;

    fn dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lattice-exec-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new().create(&path).unwrap();
        path
    }

    fn run(host: &ExecHost, sql: &str) -> Vec<Backend> {
        let mut s = Session::local_trust(host.handler(), 1, 2);
        s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        s.on_message(Frontend::Query(sql.into())).messages
    }

    fn code(messages: &[Backend]) -> Option<&str> {
        messages.iter().find_map(|m| match m {
            Backend::ErrorResponse { code, .. } => Some(code.as_str()),
            _ => None,
        })
    }

    #[test]
    fn policy_file_is_strict() {
        let ok = PolicyTable::parse_accounts("a\tallow-all\n\nb\tdeny-all\n").unwrap();
        assert_eq!(ok.get("a"), Some(&Grant::AllowAll));
        assert_eq!(ok.get("b"), Some(&Grant::DenyAll));
        for bad in [
            "a allow-all",
            "a\tallow",
            "a\tallow-all\na\tdeny-all",
            "\tallow-all",
            "a\tallow-all\t",
            "bad\rname\tallow-all",
        ] {
            assert!(
                matches!(
                    PolicyTable::parse_accounts(bad),
                    Err(PolicyFileError::Malformed { .. })
                ),
                "{bad:?}"
            );
        }
        // The error names the line, not its content.
        assert_eq!(
            PolicyTable::parse_accounts("a\tallow-all\n\nb\tnope\n"),
            Err(PolicyFileError::Malformed { line: 3 })
        );
        assert_eq!(
            PolicyTable::parse_accounts("a\tallow-all\na\tdeny-all"),
            Err(PolicyFileError::Malformed { line: 2 })
        );
        assert_eq!(
            PolicyTable::parse_accounts(&format!("{}\tallow-all", "x".repeat(64))),
            Err(PolicyFileError::Malformed { line: 1 })
        );
        assert_eq!(
            PolicyTable::parse_accounts(&"a".repeat(MAX_POLICY_BYTES + 1)),
            Err(PolicyFileError::OutOfBounds)
        );
        let many: String = (0..=MAX_POLICY_ENTRIES)
            .map(|i| format!("n{i}\tdeny-all\n"))
            .collect();
        assert_eq!(
            PolicyTable::parse_accounts(&many),
            Err(PolicyFileError::OutOfBounds)
        );
        // An unlisted account is denied; nothing falls back to allow-all.
        let table = PolicyTable::new(Grant::AllowAll).with_accounts(ok);
        assert_eq!(table.account("a"), Grant::AllowAll);
        assert_eq!(table.account("stranger"), Grant::DenyAll);
        assert_eq!(table.development(), Grant::AllowAll);
    }

    #[test]
    fn connections_share_one_actor_and_a_closed_one_is_forgotten() {
        let path = dir("share");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        assert_eq!(
            code(&run(&host, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")),
            None
        );
        let out = run(&host, "INSERT INTO t VALUES (1, 'a')");
        assert!(out.contains(&Backend::CommandComplete("INSERT 0 1".into())));
        // A second connection sees the row the first committed.
        let out = run(&host, "SELECT v FROM t");
        assert!(out.contains(&Backend::DataRow(vec![Some(b"a".to_vec())])));
        host.shutdown().unwrap();
        // After shutdown a handler answers 57P01 instead of hanging.
        assert_eq!(code(&run(&host, "SELECT 1")), Some("57P01"));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_directory_held_by_another_host_is_refused() {
        let path = dir("lock");
        let first = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        assert!(
            ExecHost::start_dir(
                path.clone(),
                PolicyTable::new(Grant::AllowAll),
                HostLimits::default()
            )
            .is_err()
        );
        first.shutdown().unwrap();
        ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_restart_recovers_tables_with_their_columns() {
        let path = dir("restart");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        run(
            &host,
            "CREATE TABLE t (id INT PRIMARY KEY, v TEXT NOT NULL)",
        );
        run(&host, "INSERT INTO t VALUES (1, 'a')");
        host.shutdown().unwrap();
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        let out = run(&host, "SELECT v FROM t WHERE id = 1");
        assert!(out.contains(&Backend::DataRow(vec![Some(b"a".to_vec())])));
        assert_eq!(
            code(&run(&host, "INSERT INTO t VALUES (1, 'b')")),
            Some("23505")
        );
        assert_eq!(
            code(&run(&host, "INSERT INTO t VALUES (2, NULL)")),
            Some("23502")
        );
        assert_eq!(
            code(&run(&host, "CREATE TABLE t (id INT PRIMARY KEY)")),
            Some("42P07")
        );
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_request_cancelled_before_it_runs_does_not_run() {
        let path = dir("cancel");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut proxy = host.handler();
        let token = CancellationToken::default();
        proxy
            .execute_cancellable(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[], &token)
            .unwrap();
        let token = CancellationToken::default();
        token.request();
        let error = proxy
            .execute_cancellable(&ctx, "INSERT INTO t VALUES (1)", &[], &token)
            .unwrap_err();
        assert_eq!(error.code, "57014");
        let rows = proxy.execute(&ctx, "SELECT id FROM t", &[]).unwrap();
        assert_eq!(rows.tag, "SELECT 0");
        drop(proxy);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_denied_account_sees_no_catalog() {
        let path = dir("hidden");
        let accounts = PolicyTable::parse_accounts(
            "denied	deny-all
reader	allow-all
",
        )
        .unwrap();
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll).with_accounts(accounts),
            HostLimits::default(),
        )
        .unwrap();
        run(
            &host,
            "CREATE TABLE secret (id INT PRIMARY KEY, hidden_col TEXT)",
        );
        // (Parse/Describe result, execution result) of a statement for an account.
        let probe = |name: &str, sql: &str| {
            let principal = Principal::account(name);
            let ctx = QueryContext::new(&principal);
            let mut proxy = host.handler();
            let described = proxy
                .describe(&ctx, sql)
                .map(|c| format!("{} columns", c.len()))
                .map_err(|e| (e.code, e.message));
            let ran = proxy
                .execute(&ctx, sql, &[])
                .map(|o| o.tag)
                .map_err(|e| (e.code, e.message));
            (described, ran)
        };
        // The allowed account sees the table, so the probe is meaningful.
        assert!(probe("reader", "SELECT hidden_col FROM secret").0.is_ok());
        for sql in [
            "SELECT * FROM secret",
            "SELECT hidden_col FROM secret",
            "SELECT nope FROM secret",
            "INSERT INTO secret VALUES (1, 'x')",
            "UPDATE secret SET hidden_col = 'y'",
            "DELETE FROM secret",
        ] {
            let (described, ran) = probe("denied", sql);
            let (missing_described, missing_ran) =
                probe("denied", &sql.replace("secret", "absent"));
            // An existing table behaves exactly like one that does not exist, at Parse and at run time,
            // down to the message once the name is normalised.
            let norm = |r: Result<_, (String, String)>, name: &str| {
                r.map_err(|(c, m): (String, String)| (c, m.replace(name, "X")))
            };
            assert_eq!(
                described.clone().err().map(|e| e.0),
                Some("42P01".to_string()),
                "{sql}"
            );
            assert_eq!(
                norm(described, "secret"),
                norm(missing_described, "absent"),
                "{sql}"
            );
            assert_eq!(norm(ran, "secret"), norm(missing_ran, "absent"), "{sql}");
        }
        // DDL is refused before any name is looked up, so existing and absent names answer alike.
        for name in ["secret", "absent"] {
            assert_eq!(
                probe("denied", &format!("DROP TABLE {name}"))
                    .1
                    .unwrap_err()
                    .0,
                "42501"
            );
            assert_eq!(
                probe(
                    "denied",
                    &format!("CREATE TABLE {name} (id INT PRIMARY KEY)")
                )
                .1
                .unwrap_err()
                .0,
                "42501"
            );
        }
        // An account the table does not list is denied the same way.
        assert_eq!(
            probe("stranger", "SELECT * FROM secret").1.unwrap_err().0,
            "42P01"
        );
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_dead_database_thread_is_visible_and_answers_57p01() {
        let path = dir("crash");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        assert!(host.is_alive());
        let mut survivor = host.handler();
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        survivor
            .execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        host.crash_for_test();
        // Whether the next call races the unwinding or follows it, the answer is 57P01, not a hang.
        let error = survivor.execute(&ctx, "SELECT id FROM t", &[]).unwrap_err();
        assert_eq!(error.code, "57P01");
        assert_eq!(
            host.handler()
                .execute(&ctx, "SELECT id FROM t", &[])
                .unwrap_err()
                .code,
            "57P01"
        );
        // Joining waits out the unwinding, after which the liveness flag is down.
        assert!(host.shutdown().unwrap_err().contains("panicked"));
        assert!(!host.is_alive());
        // The directory lock went with the thread.
        ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn shutdown_answers_queued_work_without_running_it() {
        let path = dir("stopping");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        let mut proxy = host.handler();
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        proxy
            .execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        host.shutdown().unwrap();
        assert_eq!(
            proxy
                .execute(&ctx, "SELECT id FROM t", &[])
                .unwrap_err()
                .code,
            "57P01"
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    /// A host with the default limits. Set-up statements (a CREATE TABLE is a WAL commit with a real
    /// fsync) must never run under a short ceiling, so the timeout tests arm one on a single proxy
    /// with `arm` only around the statement that is held, and lift it again with `disarm`.
    fn quick_host(name: &str) -> (ExecHost, PathBuf) {
        let path = dir(name);
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        (host, path)
    }

    /// The wait that expires for a statement the test holds. It cannot finish early, so a loaded
    /// runner only makes the test slower, never wrong.
    fn arm(p: &mut ExecProxy) {
        p.set_wait_for_test(Some(Duration::from_millis(200)));
    }

    /// A wait no loaded runner reaches, for every statement that is expected to complete.
    fn disarm(p: &mut ExecProxy) {
        p.set_wait_for_test(Some(Duration::from_secs(120)));
    }

    fn rows_of(host: &ExecHost, sql: &str) -> usize {
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        host.handler().execute(&ctx, sql, &[]).unwrap().rows.len()
    }

    #[test]
    fn a_statement_that_timed_out_in_the_queue_fails_the_block_so_commit_rolls_back() {
        let (host, path) = quick_host("timeout-queued");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        p.execute(&ctx, "BEGIN", &[]).unwrap();
        p.execute(&ctx, "INSERT INTO t VALUES (1)", &[]).unwrap();
        // A statement queued behind a stalled thread times out without ever starting.
        let stall = host.stall_for_test();
        arm(&mut p);
        let error = p
            .execute(&ctx, "INSERT INTO t VALUES (2)", &[])
            .unwrap_err();
        disarm(&mut p);
        assert_eq!(error.code, "57014");
        drop(stall);
        // The client saw an error in its block, so the block is aborted: statements are refused and
        // COMMIT answers ROLLBACK, as PostgreSQL does.
        assert_eq!(
            p.execute(&ctx, "SELECT id FROM t", &[]).unwrap_err().code,
            "25P02"
        );
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        // Neither insert is visible: the earlier statement of the block was rolled back too.
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        // The connection is usable again.
        assert_eq!(
            p.execute(&ctx, "INSERT INTO t VALUES (3)", &[])
                .unwrap()
                .tag,
            "INSERT 0 1"
        );
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 1);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_statement_that_finishes_after_its_timeout_is_not_committed_by_a_later_commit() {
        let (host, path) = quick_host("timeout-running");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        p.execute(&ctx, "BEGIN", &[]).unwrap();
        p.execute(&ctx, "INSERT INTO t VALUES (1)", &[]).unwrap();
        // The second insert starts and then outlives the connection's wait; its cancel is not seen.
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
        p.hold_next_for_test(entered_tx, release_rx);
        arm(&mut p);
        let worker = thread::Builder::new()
            .spawn(move || {
                let principal = Principal::local_development();
                let ctx = QueryContext::new(&principal);
                let error = p
                    .execute(&ctx, "INSERT INTO t VALUES (2)", &[])
                    .unwrap_err();
                (p, error.code)
            })
            .unwrap();
        entered_rx.recv().unwrap();
        let (mut p, code) = worker.join().unwrap();
        disarm(&mut p);
        assert_eq!(code, "57014");
        // Now the statement finishes and stages its row; the abort queued behind it then fails the block.
        drop(release_tx);
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_timeout_outside_a_block_changes_nothing_for_the_next_statement() {
        let (host, path) = quick_host("timeout-idle");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        let stall = host.stall_for_test();
        arm(&mut p);
        assert_eq!(
            p.execute(&ctx, "INSERT INTO t VALUES (1)", &[])
                .unwrap_err()
                .code,
            "57014"
        );
        disarm(&mut p);
        drop(stall);
        // Autocommit: no block to fail, and the skipped insert never ran.
        assert_eq!(
            p.execute(&ctx, "INSERT INTO t VALUES (2)", &[])
                .unwrap()
                .tag,
            "INSERT 0 1"
        );
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 1);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// An actor request sent without a proxy, so only the thread's own skip path can abort the block.
    fn send_cancelled_execute(host: &ExecHost, proxy_conn: u64, sql: &str) -> QueryError {
        let token = CancellationToken::default();
        token.request();
        let (reply, answer) = mpsc::sync_channel(1);
        host.requests
            .send(Request::Execute {
                conn: proxy_conn,
                seq: u64::MAX,
                shared: Arc::new(ConnShared::default()),
                principal: Principal::local_development(),
                sql: sql.to_string(),
                params: Vec::new(),
                token,
                reply,
                hold: None,
            })
            .unwrap();
        answer.recv().unwrap().unwrap_err()
    }

    fn send_cancelled_describe(host: &ExecHost, proxy_conn: u64, sql: &str) -> QueryError {
        let token = CancellationToken::default();
        token.request();
        let (reply, answer) = mpsc::sync_channel(1);
        host.requests
            .send(Request::Describe {
                conn: proxy_conn,
                seq: u64::MAX,
                shared: Arc::new(ConnShared::default()),
                principal: Principal::local_development(),
                sql: sql.to_string(),
                token,
                reply,
            })
            .unwrap();
        answer.recv().unwrap().unwrap_err()
    }

    fn open_block(host: &ExecHost) -> ExecProxy {
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        p.execute(&ctx, "BEGIN", &[]).unwrap();
        p.execute(&ctx, "INSERT INTO t VALUES (1)", &[]).unwrap();
        p
    }

    #[test]
    fn the_skip_path_alone_fails_the_block_of_a_cancelled_execute() {
        let (host, path) = quick_host("skip-execute");
        let mut p = open_block(&host);
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        // No proxy timeout here: the thread meets an already cancelled request inside the block.
        let error = send_cancelled_execute(&host, p.conn, "INSERT INTO t VALUES (2)");
        assert_eq!(error.code, "57014");
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn the_skip_path_alone_fails_the_block_of_a_cancelled_describe() {
        let (host, path) = quick_host("skip-describe");
        let mut p = open_block(&host);
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let error = send_cancelled_describe(&host, p.conn, "SELECT id FROM t");
        assert_eq!(error.code, "57014");
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_describe_that_times_out_in_a_block_fails_the_block() {
        let (host, path) = quick_host("timeout-describe");
        let mut p = open_block(&host);
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let stall = host.stall_for_test();
        arm(&mut p);
        assert_eq!(
            p.describe(&ctx, "SELECT id FROM t").unwrap_err().code,
            "57014"
        );
        disarm(&mut p);
        drop(stall);
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_describe_error_in_a_block_fails_the_block() {
        let (host, path) = quick_host("describe-error");
        let mut p = open_block(&host);
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        // A Parse error in a block aborts it in PostgreSQL; a later COMMIT must not commit it.
        assert_eq!(
            p.describe(&ctx, "SELECT nope FROM t").unwrap_err().code,
            "42703"
        );
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_begin_that_runs_after_its_timeout_leaves_no_block_behind() {
        let (host, path) = quick_host("late-begin");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
        p.hold_next_for_test(entered_tx, release_rx);
        arm(&mut p);
        let worker = thread::Builder::new()
            .spawn(move || {
                let principal = Principal::local_development();
                let ctx = QueryContext::new(&principal);
                let error = p.execute(&ctx, "BEGIN", &[]).unwrap_err();
                (p, error.code)
            })
            .unwrap();
        entered_rx.recv().unwrap();
        let (mut p, code) = worker.join().unwrap();
        disarm(&mut p);
        assert_eq!(code, "57014");
        drop(release_tx);
        // The client saw BEGIN fail, so it is outside any block: the next statement is an ordinary
        // autocommit statement, not a refused statement of an aborted block, and it commits at once.
        assert_eq!(
            p.execute(&ctx, "INSERT INTO t VALUES (5)", &[])
                .unwrap()
                .tag,
            "INSERT 0 1"
        );
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 1);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// A statement from inside a block that times out and then runs late, as the block's last word.
    fn late_closing_statement(closing: &str) -> (usize, String) {
        let (host, path) = quick_host(&format!("late-{}", closing.to_lowercase()));
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        p.execute(&ctx, "BEGIN", &[]).unwrap();
        p.execute(&ctx, "INSERT INTO t VALUES (1)", &[]).unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
        p.hold_next_for_test(entered_tx, release_rx);
        arm(&mut p);
        let sql = closing.to_string();
        let worker = thread::Builder::new()
            .spawn(move || {
                let principal = Principal::local_development();
                let ctx = QueryContext::new(&principal);
                let error = p.execute(&ctx, &sql, &[]).unwrap_err();
                (p, error.code)
            })
            .unwrap();
        entered_rx.recv().unwrap();
        let (mut p, code) = worker.join().unwrap();
        disarm(&mut p);
        assert_eq!(code, "57014");
        drop(release_tx);
        // The closing statement ran late and ended the block, so the abort queued behind it finds no
        // block and does nothing: the next statement is an ordinary autocommit statement.
        let tag = p
            .execute(&ctx, "INSERT INTO t VALUES (2)", &[])
            .unwrap()
            .tag;
        let rows = rows_of(&host, "SELECT id FROM t");
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
        (rows, tag)
    }

    #[test]
    fn the_abort_after_a_late_commit_or_rollback_finds_no_block() {
        // Late COMMIT: the block's row is committed, and the next statement is not refused.
        assert_eq!(
            late_closing_statement("COMMIT"),
            (2, "INSERT 0 1".to_string())
        );
        // Late ROLLBACK: the block's row is gone, and the next statement is not refused.
        assert_eq!(
            late_closing_statement("ROLLBACK"),
            (1, "INSERT 0 1".to_string())
        );
    }

    #[test]
    fn a_begin_retried_straight_after_a_timeout_queues_behind_the_abort() {
        let (host, path) = quick_host("retry-begin");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
        p.hold_next_for_test(entered_tx, release_rx);
        arm(&mut p);
        let worker = thread::Builder::new()
            .spawn(move || {
                let principal = Principal::local_development();
                let ctx = QueryContext::new(&principal);
                let error = p.execute(&ctx, "BEGIN", &[]).unwrap_err();
                (p, error.code)
            })
            .unwrap();
        entered_rx.recv().unwrap();
        let (mut p, code) = worker.join().unwrap();
        disarm(&mut p);
        assert_eq!(code, "57014");
        drop(release_tx);
        // The retry is queued after the abort of the first BEGIN, so the abort discards the first
        // block and does not touch the retry's.
        assert_eq!(p.execute(&ctx, "BEGIN", &[]).unwrap().tag, "BEGIN");
        p.execute(&ctx, "INSERT INTO t VALUES (7)", &[]).unwrap();
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "COMMIT");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 1);
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_wire_error_in_a_block_reaches_the_database_thread_through_the_proxy() {
        let (host, path) = quick_host("wire-error");
        let mut s = Session::local_trust(host.handler(), 1, 2);
        s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
        let run = |s: &mut Session<ExecProxy>, sql: &str| {
            s.on_message(Frontend::Query(sql.into())).messages
        };
        run(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
        run(&mut s, "BEGIN");
        run(&mut s, "INSERT INTO t VALUES (1)");
        // The wire session raises this itself; the database thread never sees the statement.
        let out = s
            .on_message(Frontend::Execute {
                portal: "missing".into(),
                max_rows: 0,
            })
            .messages;
        assert_eq!(code(&out), Some("34000"));
        s.on_message(Frontend::Sync);
        let out = run(&mut s, "COMMIT");
        assert!(
            out.contains(&Backend::CommandComplete("ROLLBACK".into())),
            "{out:?}"
        );
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(s);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Waits without a sleep call: a receive on a channel nobody sends to.
    fn pause(ms: u64) {
        let (tx, rx) = mpsc::channel::<()>();
        let _ = rx.recv_timeout(Duration::from_millis(ms));
        drop(tx);
    }

    fn limited_host(name: &str, limits: HostLimits) -> (ExecHost, PathBuf) {
        let path = dir(name);
        let host =
            ExecHost::start_dir(path.clone(), PolicyTable::new(Grant::AllowAll), limits).unwrap();
        (host, path)
    }

    /// `clients` connections each run `rounds` statements one after the other against a stalled
    /// actor. Returns every error code, once all of them have finished.
    fn hammer(host: &ExecHost, clients: usize, rounds: usize) -> Vec<String> {
        let (done, results) = mpsc::channel();
        for _ in 0..clients {
            let mut proxy = host.handler();
            let done = done.clone();
            thread::Builder::new()
                .spawn(move || {
                    let principal = Principal::local_development();
                    let ctx = QueryContext::new(&principal);
                    for _ in 0..rounds {
                        let code = match proxy.execute(&ctx, "SELECT 1", &[]) {
                            Ok(_) => "ok".to_string(),
                            Err(e) => e.code,
                        };
                        let _ = done.send(code);
                    }
                })
                .unwrap();
        }
        drop(done);
        let mut codes = Vec::new();
        // A regression that blocks a sender forever shows up here as a timeout, not a hung suite.
        while codes.len() < clients * rounds {
            codes.push(
                results
                    .recv_timeout(Duration::from_secs(30))
                    .expect("a client is stuck"),
            );
        }
        codes
    }

    #[test]
    fn a_timeout_queues_nothing_but_the_statement_itself() {
        // The host keeps its default ceiling, so the CREATE TABLE is never cut short by a slow fsync.
        let (host, path) = quick_host("one-entry");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        let stall = host.stall_for_test();
        // The first statement times out in the queue and stays there. The retries do not add entries
        // (the earlier one has not left), and neither does the abort the timeout owes.
        //
        // The outcome does not depend on the clock. The stall keeps the actor from answering, so the
        // first statement can only end in 57014 and each retry can only end in 53300 (its wait for
        // the earlier entry expires). Each regression below fails on every run, whatever the timing:
        // - a retry that queues its own SELECT instead of waiting for the earlier entry raises the
        //   peak to 2 (the entry stays queued until `drop(stall)`) and answers 57014, not 53300;
        // - a timeout that queues an abort entry raises the peak to 2 and fails the peak check.
        arm(&mut p);
        assert_eq!(p.execute(&ctx, "SELECT 1", &[]).unwrap_err().code, "57014");
        for _ in 0..3 {
            assert_eq!(p.execute(&ctx, "SELECT 1", &[]).unwrap_err().code, "53300");
        }
        disarm(&mut p);
        assert!(
            host.peak_queue_for_test() <= 1,
            "queue peaked at {}",
            host.peak_queue_for_test()
        );
        drop(stall);
        drop(p);
        drop(host);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn many_clients_timing_out_against_a_stalled_actor_never_exceed_one_entry_each() {
        let (host, path) = limited_host(
            "stalled-many",
            HostLimits {
                max_timeout_ms: Some(100),
                wait_slack_ms: 0,
                queue_depth: 256,
                shutdown_grace_ms: 400,
                ..HostLimits::default()
            },
        );
        let stall = host.stall_for_test();
        let clients = 24;
        let codes = hammer(&host, clients, 4);
        assert!(
            codes.iter().all(|c| c == "57014" || c == "53300"),
            "{codes:?}"
        );
        // Each client has a stale statement queued; the 3 extra are the stall, a wake-up and a stop.
        assert!(
            host.peak_queue_for_test() <= clients + 3,
            "queue peaked at {}",
            host.peak_queue_for_test()
        );
        // Shutdown does not wait for the stuck thread forever: it reports it and returns.
        let error = host.shutdown().unwrap_err();
        assert!(error.contains("did not stop"), "{error}");
        drop(stall);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_full_queue_answers_busy_instead_of_blocking_a_connection() {
        let (host, path) = limited_host(
            "full-queue",
            HostLimits {
                max_timeout_ms: Some(100),
                wait_slack_ms: 0,
                queue_depth: 4,
                shutdown_grace_ms: 400,
                ..HostLimits::default()
            },
        );
        let stall = host.stall_for_test();
        // 20 connections, a queue of 4: the rest cannot get in and must still be answered.
        let codes = hammer(&host, 20, 2);
        assert!(
            codes.iter().all(|c| c == "57014" || c == "53300"),
            "{codes:?}"
        );
        assert!(codes.iter().any(|c| c == "53300"));
        assert!(host.peak_queue_for_test() <= 4);
        assert!(host.shutdown().is_err());
        drop(stall);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn beginning_shutdown_releases_connections_waiting_on_a_stuck_actor() {
        // A long ceiling: only the shutdown signal can end these waits in time.
        let (host, path) = limited_host(
            "release-waiters",
            HostLimits {
                max_timeout_ms: Some(600_000),
                wait_slack_ms: 0,
                shutdown_grace_ms: 400,
                ..HostLimits::default()
            },
        );
        let stall = host.stall_for_test();
        let (done, results) = mpsc::channel();
        for _ in 0..6 {
            let mut proxy = host.handler();
            let done = done.clone();
            thread::Builder::new()
                .spawn(move || {
                    let principal = Principal::local_development();
                    let ctx = QueryContext::new(&principal);
                    let _ = done.send(proxy.execute(&ctx, "SELECT 1", &[]).unwrap_err().code);
                })
                .unwrap();
        }
        drop(done);
        while host.peak_queue_for_test() < 6 {
            pause(10);
        }
        host.begin_shutdown();
        for _ in 0..6 {
            assert_eq!(
                results.recv_timeout(Duration::from_secs(10)).unwrap(),
                "57P01"
            );
        }
        assert!(host.shutdown().is_err());
        drop(stall);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn closed_connections_are_forgotten_without_a_request_of_their_own() {
        let (host, path) = quick_host("sweep");
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        p.execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        p.execute(&ctx, "BEGIN", &[]).unwrap();
        p.execute(&ctx, "INSERT INTO t VALUES (1)", &[]).unwrap();
        // Closing mid-block abandons the block: nothing of it is visible to another connection.
        drop(p);
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        // With the actor stalled the wake-up cannot be queued behind a full queue forever: dropping
        // many proxies leaves at most one wake-up in the queue.
        let stall = host.stall_for_test();
        for _ in 0..50 {
            let mut q = host.handler();
            q.used = true;
            drop(q);
        }
        assert!(host.peak_queue_for_test() <= 2);
        drop(stall);
        drop(host);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_statement_that_never_got_into_the_queue_fails_the_block_so_commit_rolls_back() {
        // A queue of one: another connection's timed-out statement fills it while the thread is
        // stalled, so the block's next statement cannot get in and answers 53300. Nothing of it ever
        // reaches the thread, so only the failure mark can fail the executor's block.
        // Only the two statements that are meant to time out run under a short wait (`arm`). The
        // block's set-up, the COMMIT after the release and the final read use the default ceiling and
        // wait, so a slow disk cannot fail them.
        let (host, path) = limited_host(
            "never-queued",
            HostLimits {
                queue_depth: 1,
                ..HostLimits::default()
            },
        );
        let mut p = open_block(&host);
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let stall = host.stall_for_test();
        let mut filler = host.handler();
        arm(&mut filler);
        assert_eq!(
            filler.execute(&ctx, "SELECT 1", &[]).unwrap_err().code,
            "57014"
        );
        arm(&mut p);
        assert_eq!(
            p.execute(&ctx, "INSERT INTO t VALUES (2)", &[])
                .unwrap_err()
                .code,
            "53300"
        );
        disarm(&mut p);
        drop(stall);
        assert_eq!(p.execute(&ctx, "COMMIT", &[]).unwrap().tag, "ROLLBACK");
        assert_eq!(rows_of(&host, "SELECT id FROM t"), 0);
        drop(filler);
        drop(p);
        drop(host);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_dead_actor_answers_57p01_even_with_an_entry_stuck_in_the_queue() {
        let (host, path) = limited_host(
            "dead-stuck-entry",
            HostLimits {
                max_timeout_ms: Some(100),
                wait_slack_ms: 0,
                ..HostLimits::default()
            },
        );
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        let stall = host.stall_for_test();
        host.crash_for_test();
        // Queued behind the crash, so the thread never takes it.
        assert_eq!(p.execute(&ctx, "SELECT 1", &[]).unwrap_err().code, "57014");
        drop(stall);
        while host.is_alive() {
            pause(10);
        }
        assert_eq!(p.execute(&ctx, "SELECT 1", &[]).unwrap_err().code, "57P01");
        let _ = host.shutdown();
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn marks_apply_once_and_only_to_a_later_request() {
        let shared = ConnShared::default();
        let mut sessions: BTreeMap<u64, ExecHandler<StdIo, SystemClock, OsRng>> = BTreeMap::new();
        let was_in_block = BTreeMap::new();
        let mut applied = BTreeMap::new();
        let mut apply = |seq| {
            apply_marks(&shared, 1, seq, &mut sessions, &was_in_block, &mut applied);
            applied.get(&1).copied()
        };
        // Request 5 timed out. It is not "later" than itself.
        shared.abort_through.store(5, Ordering::Release);
        assert_eq!(apply(5), None);
        assert_eq!(apply(6), Some((5, 0)));
        // Applied: a request after that does nothing more.
        assert_eq!(apply(7), Some((5, 0)));
        // A second abort, for a later request, applies again; a failure mark is tracked apart.
        shared.abort_through.store(7, Ordering::Release);
        assert_eq!(apply(7), Some((5, 0)));
        assert_eq!(apply(8), Some((7, 0)));
        shared.fail_through.store(8, Ordering::Release);
        assert_eq!(apply(8), Some((7, 0)));
        assert_eq!(apply(9), Some((7, 8)));
    }

    // ----- provenance: who committed (issue 520) -----

    fn authors_in(dir: &std::path::Path) -> Vec<u128> {
        let db = Database::open(
            StdIo::new(dir.to_path_buf()),
            SystemClock,
            OsRng,
            &Config::default(),
        )
        .unwrap();
        db.store()
            .events_since(lattice_core::feed::Cursor::ORIGIN)
            .unwrap()
            .map(|e| e.author)
            .collect()
    }

    fn accounts(names: &[&str]) -> PolicyTable {
        PolicyTable::new(Grant::AllowAll).with_accounts(
            names
                .iter()
                .map(|n| ((*n).to_string(), Grant::AllowAll))
                .collect(),
        )
    }

    fn as_account(host: &ExecHost, name: &str, sql: &str) {
        let principal = Principal::account(name);
        host.handler()
            .execute(&QueryContext::new(&principal), sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    #[test]
    fn account_author_ids_are_durable_injective_and_stable_across_a_host_restart() {
        let path = dir("authors");
        let table = accounts(&["alice", "bob", "carol"]);
        let host = ExecHost::start_dir(path.clone(), table.clone(), HostLimits::default()).unwrap();
        as_account(&host, "alice", "CREATE TABLE t (id INT PRIMARY KEY)");
        as_account(&host, "bob", "INSERT INTO t VALUES (1)");
        as_account(&host, "alice", "INSERT INTO t VALUES (2)");
        let _ = run(&host, "INSERT INTO t VALUES (3)");
        host.shutdown().unwrap();
        drop(host);
        // Accounts are numbered in order of first use; the development principal is 1. The table
        // was created by alice, so she is first.
        assert_eq!(authors_in(&path), vec![257, 256, 1]);
        let host = ExecHost::start_dir(path.clone(), table, HostLimits::default()).unwrap();
        as_account(&host, "carol", "CREATE TABLE u (id INT PRIMARY KEY)");
        as_account(&host, "carol", "INSERT INTO u VALUES (1)");
        as_account(&host, "bob", "INSERT INTO u VALUES (2)");
        host.shutdown().unwrap();
        drop(host);
        assert_eq!(authors_in(&path), vec![257, 256, 1, 258, 257]);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn author_id_collision_refuses_startup() {
        let path = dir("authors-collision");
        // A hand-edited or damaged registry that gives two accounts one id.
        let mut io = StdIo::new(path.clone());
        io.append(
            "authors.reg",
            b"lattice-authors v1\nA\t256\talice\nA\t256\tbob\n",
        )
        .unwrap();
        io.sync("authors.reg").unwrap();
        let error = match ExecHost::start_dir(
            path.clone(),
            accounts(&["alice", "bob"]),
            HostLimits::default(),
        ) {
            Ok(_) => panic!("started on a colliding registry"),
            Err(e) => e,
        };
        assert!(error.contains("author registry"), "{error}");
        // The message names the line and never the account names.
        assert!(
            !error.contains("alice") && !error.contains("bob"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn registry_missing_or_older_than_log_refuses_startup() {
        let path = dir("authors-behind");
        let table = accounts(&["alice", "bob"]);
        let host = ExecHost::start_dir(path.clone(), table.clone(), HostLimits::default()).unwrap();
        as_account(&host, "alice", "CREATE TABLE t (id INT PRIMARY KEY)");
        as_account(&host, "alice", "INSERT INTO t VALUES (1)");
        as_account(&host, "bob", "INSERT INTO t VALUES (2)");
        host.shutdown().unwrap();
        drop(host);
        let full = StdIo::new(path.clone()).read("authors.reg").unwrap();
        let start = |p: &std::path::Path| {
            ExecHost::start_dir(p.to_path_buf(), table.clone(), HostLimits::default())
        };
        // Control: the intact registry starts.
        start(&path).unwrap().shutdown().unwrap();
        // Older than the log: the second account's line is gone.
        let header_and_first = {
            let text = String::from_utf8(full.clone()).unwrap();
            let end = text.match_indices('\n').nth(1).unwrap().0 + 1;
            text.as_bytes()[..end].to_vec()
        };
        let mut io = StdIo::new(path.clone());
        io.truncate("authors.reg", header_and_first.len() as u64)
            .unwrap();
        let error = start(&path).err().expect("started on an older registry");
        assert!(error.contains("author registry"), "{error}");
        // Missing altogether.
        StdIo::new(path.clone()).remove("authors.reg").unwrap();
        let error = start(&path).err().expect("started without a registry");
        assert!(error.contains("older than the log"), "{error}");
        let _ = std::fs::remove_dir_all(path);
    }

    #[cfg(unix)]
    #[test]
    fn the_registry_file_is_private_and_a_wide_one_with_data_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let path = dir("authors-mode");
        let table = accounts(&["alice"]);
        let host = ExecHost::start_dir(path.clone(), table.clone(), HostLimits::default()).unwrap();
        as_account(&host, "alice", "CREATE TABLE t (id INT PRIMARY KEY)");
        host.shutdown().unwrap();
        drop(host);
        let file = path.join("authors.reg");
        let mode = |f: &std::path::Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = ExecHost::start_dir(path.clone(), table, HostLimits::default())
            .err()
            .expect("started on a world-readable registry");
        assert!(error.contains("0600"), "{error}");
        let _ = std::fs::remove_dir_all(path);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_registry_is_refused() {
        let path = dir("authors-symlink");
        std::os::unix::fs::symlink(path.join("elsewhere"), path.join("authors.reg")).unwrap();
        let error = ExecHost::start_dir(path.clone(), accounts(&["alice"]), HostLimits::default())
            .err()
            .expect("started on a symlinked registry");
        assert!(error.contains("symbolic link"), "{error}");
        let _ = std::fs::remove_dir_all(path);
    }

    // ---- commit notifier (issue 523) ----

    use crate::notify::WaitError;
    use crate::subscribe::EventSource;

    /// A host whose database thread has the planted defect `bug` switched on (or none). The
    /// defect switch is per thread, and the database is opened on the actor's own thread.
    fn notifier_host(name: &str, bug: Option<Bug>) -> (ExecHost, PathBuf) {
        let path = dir(name);
        let data = path.clone();
        let host = ExecHost::start(
            move || {
                if let Some(bug) = bug {
                    bugs::enable(bug);
                }
                Database::open(StdIo::new(data), SystemClock, OsRng, &Config::default())
            },
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        (host, path)
    }

    fn spawn_thread<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> thread::JoinHandle<T> {
        thread::Builder::new().spawn(f).unwrap()
    }

    fn lsn_of(cursor: &[u8]) -> u64 {
        u64::from_be_bytes(cursor[..8].try_into().unwrap())
    }

    /// The newest LSN the feed source can show for table `t`, read independently of the notifier.
    fn newest_readable(host: &ExecHost) -> u64 {
        let who = Principal::local_development();
        let events = host
            .feed_source(64)
            .read_after(&who, "table/t", &[], 64)
            .unwrap();
        events.last().map_or(0, |e| lsn_of(&e.cursor))
    }

    /// Runs `INSERT INTO t VALUES (1)` on a thread and holds it between its dequeue and its run.
    fn held_insert(host: &ExecHost) -> (thread::JoinHandle<()>, SyncSender<()>, Receiver<()>) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
        let mut p = host.handler();
        p.hold_next_for_test(entered_tx, release_rx);
        let worker = spawn_thread(move || {
            let principal = Principal::local_development();
            let ctx = QueryContext::new(&principal);
            p.execute(&ctx, "INSERT INTO t VALUES (1)", &[]).unwrap();
        });
        (worker, release_tx, entered_rx)
    }

    /// Creates table `t` and returns the LSN the notifier shows afterwards.
    fn table_t(host: &ExecHost) -> u64 {
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        host.handler()
            .execute(&ctx, "CREATE TABLE t (id INT PRIMARY KEY)", &[])
            .unwrap();
        host.commit_notifier().latest()
    }

    fn wake_is_published_only_after_the_commit_is_durable(bug: Option<Bug>) -> Result<(), String> {
        let (host, path) = notifier_host(
            if bug.is_some() {
                "notify-durable-planted"
            } else {
                "notify-durable"
            },
            bug,
        );
        let notifier = host.commit_notifier();
        let head = table_t(&host);
        assert!(head > 0, "a committed statement must have been published");
        let (worker, release, entered) = held_insert(&host);
        entered.recv().unwrap();
        // The statement is dequeued and not yet run: nothing new may be announced.
        let early = notifier.wait_after(head, Duration::ZERO);
        release.send(()).unwrap();
        worker.join().unwrap();
        if early != Err(WaitError::TimedOut) {
            drop(host);
            std::fs::remove_dir_all(path).unwrap();
            return Err(format!("announced before the commit ran: {early:?}"));
        }
        let woken = notifier.wait_after(head, Duration::from_secs(60)).unwrap();
        assert!(woken > head);
        // The LSN a waiter is woken with is readable through the feed source.
        assert_eq!(newest_readable(&host), woken);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
        Ok(())
    }

    #[test]
    fn wake_is_published_only_after_the_commit_is_durable_for_the_real_host() {
        assert_eq!(
            wake_is_published_only_after_the_commit_is_durable(None),
            Ok(())
        );
    }

    #[test]
    fn a_planted_wake_before_the_commit_is_caught() {
        let caught =
            wake_is_published_only_after_the_commit_is_durable(Some(Bug::NotifyBeforeDurable))
                .unwrap_err();
        // The early announcement is the next LSN, one past the head the host had published.
        assert!(
            caught.starts_with("announced before the commit ran: Ok("),
            "{caught}"
        );
    }

    #[test]
    fn a_lost_wake_is_recovered_by_the_timeout() {
        let (host, path) = notifier_host("notify-lost", None);
        let notifier = host.commit_notifier();
        let head = table_t(&host);
        // A waiter that was never told: its "seen" is past anything published, as if the wake
        // had been dropped. Its timeout ends the wait and a poll still finds the event.
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        host.handler()
            .execute(&ctx, "INSERT INTO t VALUES (7)", &[])
            .unwrap();
        assert_eq!(
            notifier.wait_after(u64::MAX, Duration::from_millis(60)),
            Err(WaitError::TimedOut)
        );
        assert!(newest_readable(&host) > head);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_restart_publishes_the_recovered_head() {
        let path = dir("notify-restart");
        let open = |path: &PathBuf| {
            let data = path.clone();
            ExecHost::start(
                move || Database::open(StdIo::new(data), SystemClock, OsRng, &Config::default()),
                PolicyTable::new(Grant::AllowAll),
                HostLimits::default(),
            )
            .unwrap()
        };
        let host = open(&path);
        let head = table_t(&host);
        host.shutdown().unwrap();
        drop(host);
        let host = open(&path);
        // A subscriber that has seen nothing is told at once; one that has seen the head is not.
        assert_eq!(host.commit_notifier().latest(), head);
        assert_eq!(
            host.commit_notifier().wait_after(0, Duration::ZERO),
            Ok(head)
        );
        assert_eq!(
            host.commit_notifier().wait_after(head, Duration::ZERO),
            Err(WaitError::TimedOut)
        );
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_stalled_subscriber_cannot_delay_a_commit() {
        let (host, path) = notifier_host("notify-stalled", None);
        let notifier = host.commit_notifier();
        let head = table_t(&host);
        // Eight subscribers sleep and never come back for their wake; commits still go through.
        let sleepers: Vec<_> = (0..8)
            .map(|_| {
                let n = notifier.clone();
                spawn_thread(move || n.wait_after(head, Duration::from_secs(60)))
            })
            .collect();
        while notifier.sleepers() < 8 {
            pause(1);
        }
        let principal = Principal::local_development();
        let ctx = QueryContext::new(&principal);
        let mut p = host.handler();
        for i in 0..20 {
            p.execute(&ctx, &format!("INSERT INTO t VALUES ({i})"), &[])
                .unwrap();
        }
        for s in sleepers {
            assert!(matches!(s.join().unwrap(), Ok(l) if l > head));
        }
        assert_eq!(newest_readable(&host), notifier.latest());
        drop(p);
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn shutdown_wakes_every_waiter_with_closed() {
        let (host, path) = notifier_host("notify-shutdown", None);
        let notifier = host.commit_notifier();
        let head = table_t(&host);
        let sleepers: Vec<_> = (0..4)
            .map(|_| {
                let n = notifier.clone();
                spawn_thread(move || n.wait_after(head, Duration::from_secs(60)))
            })
            .collect();
        while notifier.sleepers() < 4 {
            pause(1);
        }
        host.begin_shutdown();
        for s in sleepers {
            assert_eq!(s.join().unwrap(), Err(WaitError::Closed));
        }
        assert_eq!(
            notifier.wait_after(0, Duration::ZERO),
            Err(WaitError::Closed)
        );
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_dead_database_thread_wakes_every_waiter_with_closed() {
        let (host, path) = notifier_host("notify-crash", None);
        let notifier = host.commit_notifier();
        let head = table_t(&host);
        let sleepers: Vec<_> = (0..4)
            .map(|_| {
                let n = notifier.clone();
                spawn_thread(move || n.wait_after(head, Duration::from_secs(60)))
            })
            .collect();
        while notifier.sleepers() < 4 {
            pause(1);
        }
        host.crash_for_test();
        for s in sleepers {
            assert_eq!(s.join().unwrap(), Err(WaitError::Closed));
        }
        assert!(!host.is_alive());
        let _ = host.shutdown();
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_sleeper_cap_comes_from_the_host_limits() {
        let path = dir("notify-cap");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits {
                commit_waiters: 3,
                ..HostLimits::default()
            },
        )
        .unwrap();
        let notifier = host.commit_notifier();
        let sleepers: Vec<_> = (0..3)
            .map(|_| {
                let n = notifier.clone();
                spawn_thread(move || n.wait_after(0, Duration::from_secs(60)))
            })
            .collect();
        while notifier.sleepers() < 3 {
            pause(1);
        }
        assert_eq!(
            notifier.wait_after(0, Duration::from_secs(60)),
            Err(WaitError::Full)
        );
        host.begin_shutdown();
        for s in sleepers {
            assert_eq!(s.join().unwrap(), Err(WaitError::Closed));
        }
        drop(host);
        std::fs::remove_dir_all(path).unwrap();
    }
}
