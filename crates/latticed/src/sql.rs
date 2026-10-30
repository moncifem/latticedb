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
    // database thread does.
    server: ServerHandle,
    host: Arc<ExecHost>,
}

fn create_data_dir(path: &Path) -> io::Result<()> {
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
    pub fn start(config: ServerConfig, sql: SqlConfig) -> io::Result<Self> {
        create_data_dir(&sql.data_dir)?;
        let host = Arc::new(
            ExecHost::start_dir(sql.data_dir, sql.policies, sql.limits)
                .map_err(io::Error::other)?,
        );
        let factory = Arc::clone(&host);
        let server = ServerHandle::start_with_handler(config, move || factory.handler())?;
        Ok(Self { server, host })
    }

    /// Actual bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    /// True once the listener has exited.
    pub fn is_finished(&self) -> bool {
        self.server.is_finished()
    }

    /// Names of recovered tables that SQL cannot address because column definitions are not
    /// persisted yet (#379).
    pub fn unattached_tables(&self) -> &[String] {
        self.host.unattached_tables()
    }

    /// Stops the listener, waits for its workers, then stops the database thread and releases the
    /// directory.
    pub fn shutdown(self) -> io::Result<()> {
        let Self { server, host } = self;
        let result = server.shutdown();
        host.shutdown();
        result
    }
}
