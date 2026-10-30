//! Storage-backed SQL for the listener: a durable data directory behind the SQL executor.
//!
//! The executor runs on one dedicated thread (see `lattice_api::exec_host`); every admitted
//! connection gets a proxy handler that forwards its statements to it. Without a data directory the
//! listener keeps its `SELECT 1` placeholder handler.
//!
//! What survives a restart: committed rows and table names, through the write-ahead log, which is
//! replayed when the server starts. Column definitions are kept in memory only (#379), so after a
//! restart every recovered table is unknown to SQL: `SELECT` answers `42P01` and `CREATE TABLE` of the
//! same name answers `42P07`. The server reports the number of such tables at startup.

use crate::{ServerConfig, ServerHandle};
use lattice_api::exec_host::{ExecHost, HostLimits, PolicyTable};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// SQL settings for [`SqlServer::start`].
#[derive(Debug, Clone)]
pub struct SqlConfig {
    /// Directory holding the write-ahead log. Created (mode 0700 on Unix) when missing; one server
    /// at a time may use it.
    pub data_dir: PathBuf,
    /// The row policy of the development principal and of every account. There is no default.
    pub policies: PolicyTable,
    /// Statement timeout ceiling and memory budget applied to every connection.
    pub limits: HostLimits,
}

/// A listener serving SQL from a data directory.
pub struct SqlServer {
    // Declared first so the listener stops (and its connections drop their proxies) before the
    // database thread does. `None` only while `shutdown` has taken it.
    server: Option<ServerHandle>,
    addr: SocketAddr,
    host: Arc<ExecHost>,
}

impl Drop for SqlServer {
    fn drop(&mut self) {
        // Connections waiting on a stuck database thread give up as soon as this is set, so
        // dropping the listener handle (which joins them) cannot hang on that thread.
        self.host.begin_shutdown();
    }
}

/// An existing directory must not be open to group or others: it holds every committed row.
#[cfg(unix)]
fn require_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() && metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{}: the data directory must not be accessible to group or others (mode 0700)",
            path.display()
        )));
    }
    Ok(())
}

fn create_data_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if path.exists() {
        return require_private_dir(path);
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

impl SqlServer {
    /// Opens (and recovers) the database, then binds the listener. A bind or recovery failure
    /// returns before any client is served, and a directory in use by another process is refused.
    pub fn start(config: ServerConfig, mut sql: SqlConfig) -> io::Result<Self> {
        create_data_dir(&sql.data_dir)?;
        // A connection has at most one request in the queue; the host adds one wake-up and one stop.
        // The rest is slack: nothing blocks when the queue is full, it answers `53300`.
        sql.limits.queue_depth = config.max_connections + crate::CONTROL_CONNECTION_RESERVE + 16;
        let host = Arc::new(
            ExecHost::start_dir(sql.data_dir, sql.policies, sql.limits)
                .map_err(io::Error::other)?,
        );
        let factory = Arc::clone(&host);
        let server = ServerHandle::start_with_handler(config, move || factory.handler())?;
        Ok(Self {
            addr: server.local_addr(),
            server: Some(server),
            host,
        })
    }

    /// Actual bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// True once the listener has exited or the database thread has ended. A supervisor should treat
    /// an unrequested end as a failed server; `shutdown` then reports why.
    pub fn is_finished(&self) -> bool {
        self.server.as_ref().is_none_or(ServerHandle::is_finished) || !self.host.is_alive()
    }

    /// Names of recovered tables that SQL cannot address because column definitions are not
    /// persisted yet (#379).
    pub fn unattached_tables(&self) -> &[String] {
        self.host.unattached_tables()
    }

    /// Stops the listener, waits for its workers, then stops the database thread and releases the
    /// directory. Connections waiting on the database thread are told to give up first, so a stuck
    /// thread delays this by at most `HostLimits::shutdown_grace_ms`, not forever.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.host.begin_shutdown();
        let result = match self.server.take() {
            Some(server) => server.shutdown(),
            None => Ok(()),
        };
        let database = self.host.shutdown().map_err(io::Error::other);
        result.and(database)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use lattice_api::exec_host::Grant;
    use std::time::{Duration, Instant};

    #[test]
    fn a_dead_database_thread_ends_the_server_with_an_error() {
        let path = std::env::temp_dir().join(format!("latticed-dead-sql-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let server = SqlServer::start(
            ServerConfig {
                address: SocketAddr::from(([127, 0, 0, 1], 0)),
                ..ServerConfig::default()
            },
            SqlConfig {
                data_dir: path.clone(),
                policies: PolicyTable::new(Grant::AllowAll),
                limits: HostLimits::default(),
            },
        )
        .unwrap();
        assert!(!server.is_finished());
        server.host.crash_for_test();
        let until = Instant::now() + Duration::from_secs(10);
        while !server.is_finished() {
            assert!(Instant::now() < until, "the server did not notice");
            std::thread::sleep(Duration::from_millis(10));
        }
        // main treats an unrequested end as a failure and exits non-zero on this error.
        let error = server.shutdown().unwrap_err();
        assert!(error.to_string().contains("SQL thread panicked"), "{error}");
        std::fs::remove_dir_all(path).unwrap();
    }
}
