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
//! - **Connections own their transactions.** When a proxy is dropped the actor drops that
//!   connection's session; an open transaction block is abandoned without committing.
//! - **Persistence** is whatever the [`Database`] persists: see the `lattice-exec` README. Column
//!   definitions are not recovered after a restart (#379).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lattice_core::Config;
use lattice_engines::limits::{AllowAll, DenyAll};
use lattice_exec::{ActorPolicy, Database, ExecError, ExecLimits};
use lattice_seams::{Clock, Io, OsRng, Rng, StdIo, SystemClock};

use crate::cancel::CancellationToken;
use crate::exec_handler::ExecHandler;
use crate::pgwire::Column;
use crate::principal::{Principal, QueryContext};
use crate::session::{QueryError, QueryHandler, QueryOutput};

/// Most account policy entries a policy file may hold (the credential store's own bound).
pub const MAX_POLICY_ENTRIES: usize = 1024;
/// Largest policy file in bytes.
pub const MAX_POLICY_BYTES: usize = 64 * 1024;

/// The row policy granted to a principal. Only two exist until the Phase 4 policy language lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Every row may be read and written, and the catalog may be changed.
    AllowAll,
    /// No row may be read or written, and the catalog may not be changed.
    DenyAll,
}

impl Grant {
    fn policy(self) -> ActorPolicy {
        match self {
            Grant::AllowAll => ActorPolicy::new(Rc::new(AllowAll)),
            // A denied actor also gets an empty catalog, so it cannot probe names or columns.
            Grant::DenyAll => ActorPolicy::new(Rc::new(DenyAll)).hiding_catalog(),
        }
    }

    /// `allow-all` or `deny-all`.
    pub fn name(self) -> &'static str {
        match self {
            Grant::AllowAll => "allow-all",
            Grant::DenyAll => "deny-all",
        }
    }

    /// Parses `allow-all` or `deny-all`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allow-all" => Some(Grant::AllowAll),
            "deny-all" => Some(Grant::DenyAll),
            _ => None,
        }
    }
}

/// Why a policy file was refused. The message never echoes the offending line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyFileError {
    /// The file is larger than [`MAX_POLICY_BYTES`] or lists more than [`MAX_POLICY_ENTRIES`] accounts.
    OutOfBounds,
    /// The 1-based line is not `account<TAB>allow-all|deny-all`, or repeats an account. The content
    /// is never echoed.
    Malformed {
        /// 1-based line number.
        line: usize,
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
        self.development
    }

    /// The grant of an account: the listed one, or [`Grant::DenyAll`].
    pub fn account(&self, name: &str) -> Grant {
        self.accounts.get(name).copied().unwrap_or(Grant::DenyAll)
    }

    /// Parses `account<TAB>allow-all|deny-all` lines. Blank lines are skipped; duplicates, unknown
    /// grants, names over 63 bytes or with control characters, and oversize files are refused.
    pub fn parse_accounts(text: &str) -> Result<BTreeMap<String, Grant>, PolicyFileError> {
        if text.len() > MAX_POLICY_BYTES {
            return Err(PolicyFileError::OutOfBounds);
        }
        let mut accounts = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let bad = PolicyFileError::Malformed { line: index + 1 };
            let (name, grant) = line.split_once('\t').ok_or(bad)?;
            let grant = Grant::parse(grant).ok_or(bad)?;
            if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
                return Err(bad);
            }
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
    /// Requests that may wait for the database thread. Size it above the number of connections that
    /// can have a request outstanding (one each, plus one close each); a sender blocks when it is full.
    pub queue_depth: usize,
}

impl Default for HostLimits {
    fn default() -> Self {
        HostLimits {
            max_timeout_ms: Some(30_000),
            memory_bytes: 64 << 20,
            queue_depth: 256,
        }
    }
}

impl HostLimits {
    fn exec(self) -> ExecLimits {
        ExecLimits {
            timeout_ms: self.max_timeout_ms,
            memory_bytes: self.memory_bytes,
            max_timeout_ms: self.max_timeout_ms,
        }
    }
}

enum Request {
    Execute {
        conn: u64,
        principal: Principal,
        sql: String,
        params: Vec<Option<Vec<u8>>>,
        token: CancellationToken,
        reply: SyncSender<Result<QueryOutput, QueryError>>,
    },
    Describe {
        conn: u64,
        principal: Principal,
        sql: String,
        token: CancellationToken,
        reply: SyncSender<Result<Vec<Column>, QueryError>>,
    },
    Close(u64),
    Stop,
    /// Panics the actor, so tests can observe a dead database thread.
    #[cfg(any(test, feature = "test-hooks"))]
    Crash,
}

/// How long a connection waits for its answer beyond the statement ceiling.
const WAIT_SLACK: Duration = Duration::from_secs(5);

/// Marks the actor as gone however its thread ends (normal stop, error or panic).
struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The running database actor. Share it behind an `Arc`; every method takes `&self`.
pub struct ExecHost {
    requests: SyncSender<Request>,
    next_connection: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
    unattached: Vec<String>,
    alive: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    /// How long a connection waits for an answer: the statement ceiling plus slack, or forever.
    wait: Option<Duration>,
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
        // The executor reports every storage failure as an opaque internal error, so name the likely
        // causes here: an unreadable or corrupt log, or another process holding the directory.
        let hint = |e: String| {
            format!(
                "cannot open or recover the data directory ({e}); the log may be damaged or another server may be using the directory"
            )
        };
        Self::start(
            move || {
                Database::open(
                    StdIo::new(directory),
                    SystemClock,
                    OsRng,
                    &Config::default(),
                )
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
        let (requests, inbox) = mpsc::sync_channel(limits.queue_depth.max(1));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let stopping = Arc::new(AtomicBool::new(false));
        let (thread_alive, thread_stopping) = (alive.clone(), stopping.clone());
        let thread = thread::Builder::new()
            .name("sql-actor".into())
            .spawn(move || {
                let _guard = AliveGuard(thread_alive);
                let db = match open() {
                    Ok(db) => db,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let names = db.store().catalog().current();
                let unattached: Vec<String> = names
                    .values()
                    .filter(|n| db.schema().table(n).is_none())
                    .cloned()
                    .collect();
                if ready_tx.send(Ok(unattached)).is_err() {
                    return;
                }
                serve(db, inbox, &policies, limits, &thread_stopping);
            })
            .map_err(|e| format!("cannot start the SQL thread: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(unattached)) => Ok(ExecHost {
                requests,
                next_connection: AtomicU64::new(1),
                thread: Mutex::new(Some(thread)),
                unattached,
                alive,
                stopping,
                wait: limits
                    .max_timeout_ms
                    .map(|ms| Duration::from_millis(ms).saturating_add(WAIT_SLACK)),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e.to_string())
            }
            Err(_) => {
                let _ = thread.join();
                Err("the SQL thread stopped while opening".to_string())
            }
        }
    }

    /// Tables the engine catalog recovered whose column definitions are not known to SQL. Until
    /// definitions are persisted (#379) every table recovered from the log is in this list: its
    /// rows are in the log, but SQL cannot address it.
    pub fn unattached_tables(&self) -> &[String] {
        &self.unattached
    }

    /// A handler for one connection. Cheap; the connection's session starts with its first statement.
    pub fn handler(&self) -> ExecProxy {
        ExecProxy {
            requests: self.requests.clone(),
            conn: self.next_connection.fetch_add(1, Ordering::Relaxed),
            used: false,
            wait: self.wait,
        }
    }

    /// False once the database thread has ended for any reason, including a panic. A supervisor
    /// should treat an unrequested end as a failed server.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Panics the database thread (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn crash_for_test(&self) {
        let _ = self.requests.send(Request::Crash);
    }

    /// Stops the actor after the statements already queued and drops the database, which releases
    /// the directory lock. Handlers created earlier answer `57P01` afterwards. Call it after the
    /// listener has stopped.
    ///
    /// Requests still queued when this is called are answered `57P01` without running, so shutdown
    /// waits for at most the statement that is running. Returns an error if the database thread
    /// panicked.
    pub fn shutdown(&self) -> Result<(), String> {
        self.stopping.store(true, Ordering::Release);
        let _ = self.requests.send(Request::Stop);
        let thread = self.thread.lock().ok().and_then(|mut t| t.take());
        match thread {
            Some(thread) => thread
                .join()
                .map_err(|_| "the SQL thread panicked".to_string()),
            None => Ok(()),
        }
    }
}

impl Drop for ExecHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn serve<I, C, R>(
    db: Database<I, C, R>,
    inbox: Receiver<Request>,
    policies: &PolicyTable,
    limits: HostLimits,
    stopping: &AtomicBool,
) where
    I: Io + 'static,
    C: Clock + Clone + 'static,
    R: Rng + 'static,
{
    let db = Rc::new(RefCell::new(db));
    let mut sessions: BTreeMap<u64, ExecHandler<I, C, R>> = BTreeMap::new();
    let make = |db: &Rc<RefCell<Database<I, C, R>>>| {
        let accounts = policies.clone();
        let mut handler =
            ExecHandler::new(Rc::clone(db), policies.development().policy(), move |n| {
                accounts.account(n).policy()
            });
        handler.set_limits(limits.exec());
        handler
    };
    while let Ok(request) = inbox.recv() {
        match request {
            Request::Execute {
                conn,
                principal,
                sql,
                params,
                token,
                reply,
            } => {
                let result = if stopping.load(Ordering::Acquire) {
                    Err(gone())
                } else if token.is_cancelled() {
                    Err(cancelled())
                } else {
                    let handler = sessions.entry(conn).or_insert_with(|| make(&db));
                    handler.execute_cancellable(
                        &QueryContext::new(&principal),
                        &sql,
                        &params,
                        &token,
                    )
                };
                let _ = reply.send(result);
            }
            Request::Describe {
                conn,
                principal,
                sql,
                token,
                reply,
            } => {
                let result = if stopping.load(Ordering::Acquire) {
                    Err(gone())
                } else if token.is_cancelled() {
                    Err(cancelled())
                } else {
                    let handler = sessions.entry(conn).or_insert_with(|| make(&db));
                    handler.describe_cancellable(&QueryContext::new(&principal), &sql, &token)
                };
                let _ = reply.send(result);
            }
            Request::Close(conn) => {
                sessions.remove(&conn);
            }
            Request::Stop => break,
            #[cfg(any(test, feature = "test-hooks"))]
            #[allow(clippy::panic)]
            Request::Crash => panic!("planted failure of the SQL thread"),
        }
    }
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
    wait: Option<Duration>,
    conn: u64,
    /// Set once a request was sent, so a connection that never ran SQL costs the actor nothing.
    used: bool,
}

impl ExecProxy {
    /// Sends a request and waits for its answer, at most the statement ceiling plus slack. On
    /// timeout the statement's token is cancelled: the actor skips the request if it has not started
    /// and stops it at its next row if it has, and this connection gets `57014`.
    fn call<T>(
        &mut self,
        token: &CancellationToken,
        build: impl FnOnce(SyncSender<Result<T, QueryError>>) -> Request,
    ) -> Result<T, QueryError> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.used = true;
        self.requests.send(build(reply)).map_err(|_| gone())?;
        let Some(wait) = self.wait else {
            return answer.recv().map_err(|_| gone())?;
        };
        match answer.recv_timeout(wait) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                token.request();
                Err(QueryError::new(
                    "57014",
                    "canceling statement: no answer from the database within the statement ceiling",
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(gone()),
        }
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
        self.call(cancellation, |reply| Request::Execute {
            conn,
            principal,
            sql: sql.to_string(),
            params: params.to_vec(),
            token: cancellation.clone(),
            reply,
        })
    }

    fn describe_cancellable(
        &mut self,
        ctx: &QueryContext<'_>,
        sql: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Column>, QueryError> {
        let conn = self.conn;
        let principal = ctx.principal.clone();
        self.call(cancellation, |reply| Request::Describe {
            conn,
            principal,
            sql: sql.to_string(),
            token: cancellation.clone(),
            reply,
        })
    }
}

impl Drop for ExecProxy {
    fn drop(&mut self) {
        if self.used {
            let _ = self.requests.send(Request::Close(self.conn));
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
        assert!(host.unattached_tables().is_empty());
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
    fn a_restart_recovers_table_names_but_not_columns() {
        let path = dir("restart");
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        run(&host, "CREATE TABLE t (id INT PRIMARY KEY)");
        run(&host, "INSERT INTO t VALUES (1)");
        host.shutdown().unwrap();
        let host = ExecHost::start_dir(
            path.clone(),
            PolicyTable::new(Grant::AllowAll),
            HostLimits::default(),
        )
        .unwrap();
        assert_eq!(host.unattached_tables(), ["t".to_string()]);
        assert_eq!(code(&run(&host, "SELECT id FROM t")), Some("42P01"));
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
}
