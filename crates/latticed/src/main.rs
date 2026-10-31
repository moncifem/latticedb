//! Composition root: the bounded development PostgreSQL TCP listener.
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use lattice_api::auth::{Accounts, DecoySecret, MAX_CREDENTIAL_BYTES};
use lattice_api::exec_host::{Grant, HostLimits, MAX_POLICY_BYTES, PolicyFileError, PolicyTable};
use latticed::stop::{self, DEFAULT_STOP_TIMEOUT, ForcedBy, StopCause, StopOutcome};
use latticed::ws::{
    AllowedHost, GatewayAuth, GatewayConfig, check_allowed_hosts, check_gateway_exposure,
};
use latticed::{
    Authentication, ServerConfig, ServerHandle, SqlConfig, SqlServer, TlsConfig, files,
};
use std::io::Read;
use std::io::{self};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// Exit status of a run that did not fail: the stop either finished or had to be abandoned.
enum Exit {
    Clean,
    Forced,
}

fn run() -> Result<Exit, String> {
    let mut config = ServerConfig::default();
    let mut auth_mode = String::from("development-trust");
    let mut certificates = None;
    let mut private_key = None;
    let mut credentials = None;
    let mut decoy_secret = None;
    let mut data_dir: Option<String> = None;
    let mut development_policy: Option<String> = None;
    let mut account_policies: Option<String> = None;
    let mut limits = HostLimits::default();
    let mut gateway = GatewayConfig::default();
    let mut gateway_listen = false;
    let mut gateway_tokens: Option<String> = None;
    let mut gateway_allowed_hosts: Vec<String> = Vec::new();
    let mut stdin_control = true;
    let mut allow_format_upgrade = false;
    let mut stop_timeout = DEFAULT_STOP_TIMEOUT;
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        if option == "--version" || option == "-V" {
            println!("latticed {}", env!("CARGO_PKG_VERSION"));
            return Ok(Exit::Clean);
        }
        if option == "--help" || option == "-h" {
            println!(
                "latticed [--listen IP:PORT] [--max-connections N] [--read-timeout-ms N] [--write-timeout-ms N] [--idle-timeout-ms N] [--response-timeout-ms N]\nLoopback development trust by default. Without --data-dir only SELECT 1 is answered. SQL: --data-dir DIR serves SQL from a durable data directory (statement ceiling --statement-timeout-max-ms N, default 30000; budget --statement-memory-bytes N, default 67108864; staged writes of one transaction --transaction-bytes N, default 268435456). Row policy is explicit: with development-trust pass --development-policy allow-all|deny-all; with scram-tls pass --account-policies FILE (one 'account<TAB>allow-all|deny-all' per line; unlisted accounts are denied). Committed rows, tables and their column definitions survive a restart. A data directory written before table definitions existed refuses its first CREATE TABLE until --allow-format-upgrade on (older builds cannot open the directory afterwards and there is no downgrade: take a verified backup first); a new directory needs nothing. SCRAM requires --auth scram-tls --tls-cert FILE --tls-key FILE --scram-verifiers FILE --scram-decoy-secret FILE. Sync gateway (WebSocket, protocol lattice.sync.v1, path /sync, and POST /mcp on the same port; requires --data-dir): --gateway-listen IP:PORT enables it; --gateway-tokens FILE makes it require 'Authorization: Bearer' tokens from a credential file (mode 0600), re-read within 5 seconds of a change and on SIGHUP (a file that does not load keeps the tokens in use; after 10 minutes of failed loads no token works; SCRAM verifiers and policies are read at start only), otherwise it serves the loopback development principal and only with development-trust; --tls-cert FILE and --tls-key FILE (the same files and key rules as scram-tls) serve the gateway over TLS. A non-loopback --gateway-listen needs both TLS files and --gateway-tokens, or it refuses to start; plaintext is for loopback development only (only 127.0.0.0/8 and ::1 are loopback; an IPv4-mapped address such as [::ffff:127.0.0.1] needs TLS and tokens). The gateway and the PostgreSQL listener share one certificate, so clients must trust a certificate whose name is valid for both ports. With development-trust the TLS files are accepted only with --gateway-listen, and then apply to the gateway alone (the PostgreSQL listener stays loopback trust in plaintext). POST /mcp answers to the bound address and to localhost on the bound port, and to each --gateway-allowed-host NAME[:PORT] given (repeatable, at most 16, needs --gateway-listen; the name is an ASCII DNS name or an IP literal, IPv6 in brackets, matched exactly and case-insensitively with no wildcards; an omitted port is the bound port; a malformed value stops the start); any other Host gets 403. /sync checks only that Host is present and well formed, and refuses any browser Origin. --gateway-max-connections N (default 32) and --gateway-handshake-timeout-ms N (default 5000) bound it, and the handshake timeout covers the TLS handshake. Optional --startup-timeout-ms N bounds startup/TLS/authentication. SIGTERM and SIGINT stop it cleanly; a second signal, or --stop-timeout-ms N (default 30000) elapsing, abandons a stop that does not finish. Stdin control is --stdin-control on|off (default on): with on, the line 'shutdown' on stdin stops the server and so does end-of-file, so run it with stdin held open; under a supervisor or container pass --stdin-control off, which never reads stdin. Exit status: 0 clean stop, 1 failure, 2 forced stop. Without --idle-timeout-ms the read timeout also disconnects idle sessions. The TLS key, SCRAM verifier, decoy secret and account policy files must not be accessible to group or others (mode 0600), and their directory must not be writable by group or others (mode 0700 or 0755); a symlink or a FIFO in their place is refused."
            );
            return Ok(Exit::Clean);
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {option}"))?;
        match option.as_str() {
            "--auth" => auth_mode = value,
            "--tls-cert" => certificates = Some(value),
            "--tls-key" => private_key = Some(value),
            "--scram-verifiers" => credentials = Some(value),
            "--scram-decoy-secret" => decoy_secret = Some(value),
            "--data-dir" => data_dir = Some(value),
            "--allow-format-upgrade" => {
                allow_format_upgrade = match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--allow-format-upgrade must be on or off".into()),
                }
            }
            "--stdin-control" => {
                stdin_control = match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--stdin-control must be on or off".into()),
                }
            }
            "--stop-timeout-ms" => {
                let ms: u64 = value
                    .parse()
                    .map_err(|e| format!("invalid stop timeout: {e}"))?;
                if ms == 0 {
                    return Err("invalid stop timeout: must be positive".into());
                }
                stop_timeout = Duration::from_millis(ms);
            }
            "--gateway-listen" => {
                gateway.address = value
                    .parse()
                    .map_err(|e| format!("invalid gateway address: {e}"))?;
                gateway_listen = true;
            }
            "--gateway-tokens" => gateway_tokens = Some(value),
            "--gateway-allowed-host" => gateway_allowed_hosts.push(value),
            "--gateway-max-connections" => {
                gateway.max_connections = value
                    .parse()
                    .map_err(|e| format!("invalid gateway connection limit: {e}"))?;
                gateway.max_handshakes = gateway.max_handshakes.min(gateway.max_connections);
            }
            "--gateway-handshake-timeout-ms" => {
                gateway.handshake_timeout = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid gateway handshake timeout: {e}"))?,
                )
            }
            "--development-policy" => development_policy = Some(value),
            "--account-policies" => account_policies = Some(value),
            "--statement-timeout-max-ms" => {
                let ms: u64 = value
                    .parse()
                    .map_err(|e| format!("invalid statement timeout ceiling: {e}"))?;
                if ms == 0 {
                    return Err("invalid statement timeout ceiling: must be positive".into());
                }
                limits.max_timeout_ms = Some(ms);
            }
            "--statement-memory-bytes" => {
                limits.memory_bytes = value
                    .parse()
                    .map_err(|e| format!("invalid statement memory budget: {e}"))?;
                if limits.memory_bytes == 0 {
                    return Err("invalid statement memory budget: must be positive".into());
                }
            }
            "--transaction-bytes" => {
                limits.transaction_bytes = value
                    .parse()
                    .map_err(|e| format!("invalid transaction budget: {e}"))?;
                if limits.transaction_bytes == 0 {
                    return Err("invalid transaction budget: must be positive".into());
                }
            }
            "--startup-timeout-ms" => {
                config.startup_timeout =
                    Duration::from_millis(value.parse().map_err(|_| "invalid startup timeout")?)
            }
            "--listen" => {
                config.address = value
                    .parse()
                    .map_err(|e| format!("invalid listen address: {e}"))?
            }
            "--max-connections" => {
                config.max_connections = value
                    .parse()
                    .map_err(|e| format!("invalid connection limit: {e}"))?
            }
            "--read-timeout-ms" => {
                config.read_timeout = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid read timeout: {e}"))?,
                )
            }
            "--idle-timeout-ms" => {
                config.idle_timeout = Some(Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid idle timeout: {e}"))?,
                ))
            }
            "--response-timeout-ms" => {
                config.response_timeout = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid response timeout: {e}"))?,
                )
            }
            "--write-timeout-ms" => {
                config.write_timeout = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid write timeout: {e}"))?,
                )
            }
            _ => return Err(format!("unknown option {option}")),
        }
    }
    if (gateway_listen || gateway_tokens.is_some()) && data_dir.is_none() {
        return Err("--gateway-listen and --gateway-tokens require --data-dir".into());
    }
    if gateway_tokens.is_some() && !gateway_listen {
        return Err("--gateway-tokens requires --gateway-listen".into());
    }
    if !gateway_allowed_hosts.is_empty() && !gateway_listen {
        return Err("--gateway-allowed-host requires --gateway-listen".into());
    }
    for value in &gateway_allowed_hosts {
        gateway.allowed_hosts.push(AllowedHost::parse(value)?);
    }
    check_allowed_hosts(gateway.address, &gateway.allowed_hosts)?;
    if gateway_listen {
        gateway.auth = match gateway_tokens.as_deref() {
            // The gateway loads the file at start (a failure stops the start) and then watches it.
            Some(path) => GatewayAuth::BearerFile(path.into()),
            None if auth_mode == "development-trust" => GatewayAuth::LocalDevelopment,
            None => {
                return Err("--gateway-listen needs --gateway-tokens with --auth scram-tls".into());
            }
        };
    }
    // The certificate and key serve PostgreSQL in scram-tls mode and the sync gateway in both modes.
    // In development-trust they are accepted only for the gateway, which is what keeps the PostgreSQL
    // listener's rules unchanged. The files are read inside each mode's arm, after that arm's own
    // checks, so the existing error order holds.
    let tls_given = certificates.is_some() || private_key.is_some();
    if tls_given && auth_mode == "development-trust" && !gateway_listen {
        return Err("TLS/credential options require --auth scram-tls".into());
    }
    if gateway_listen {
        check_gateway_exposure(
            gateway.address,
            certificates.is_some() && private_key.is_some(),
            gateway_tokens.is_some(),
        )
        .map_err(|why| format!("--gateway-listen {why}"))?;
    }
    let mut gateway_tls: Option<TlsConfig> = None;
    if data_dir.is_none() && (development_policy.is_some() || account_policies.is_some()) {
        return Err("--development-policy and --account-policies require --data-dir".into());
    }
    match auth_mode.as_str() {
        "development-trust" => {
            if account_policies.is_some() && gateway_tokens.is_none() {
                return Err("--account-policies requires --auth scram-tls".into());
            }
            if data_dir.is_some() && development_policy.is_none() {
                return Err(
                    "--development-policy required with --data-dir in development-trust mode"
                        .into(),
                );
            }
            if credentials.is_some() || decoy_secret.is_some() {
                return Err("TLS/credential options require --auth scram-tls".into());
            }
            if gateway_listen {
                gateway_tls = load_tls(certificates.as_deref(), private_key.as_deref())?;
            }
        }
        "scram-tls" => {
            if development_policy.is_some() {
                return Err("--development-policy applies only to development-trust".into());
            }
            if data_dir.is_some() && account_policies.is_none() {
                return Err("--account-policies required with --data-dir in scram-tls mode".into());
            }
            let decoy_path = decoy_secret
                .as_deref()
                .ok_or("--scram-decoy-secret required")?;
            let secret = bounded_secret_file(decoy_path, 32)?;
            let secret = DecoySecret::from_bytes(&secret)
                .map_err(|_| "decoy secret must contain exactly 32 binary bytes")?;
            let tls = load_tls(certificates.as_deref(), private_key.as_deref())?
                .ok_or("--tls-cert required")?;
            gateway_tls = Some(tls.clone());
            // Verifiers let an attacker guess passwords offline, so they get the key-file rule too.
            let verifiers = bounded_secret_file(
                credentials.as_deref().ok_or("--scram-verifiers required")?,
                MAX_CREDENTIAL_BYTES,
            )?;
            let text =
                std::str::from_utf8(&verifiers).map_err(|_| "invalid credential file encoding")?;
            let accounts =
                Accounts::parse(text, secret).map_err(|_| "invalid credential configuration")?;
            config.authentication = Authentication::ScramTls {
                accounts: Arc::new(accounts),
                tls,
            };
        }
        _ => return Err("--auth must be development-trust or scram-tls".into()),
    }
    if gateway_listen {
        gateway.tls = gateway_tls;
    }
    let mode = auth_mode;
    let server = match data_dir {
        None => Running::Placeholder(ServerHandle::start(config).map_err(|e| e.to_string())?),
        Some(dir) => {
            // The development grant is only reachable in trust mode; in SCRAM mode it is denied.
            let development = match development_policy.as_deref() {
                None => Grant::DenyAll,
                Some(name) => Grant::parse(name)
                    .ok_or("--development-policy must be allow-all or deny-all")?,
            };
            let mut policies = PolicyTable::new(development);
            if let Some(path) = account_policies.as_deref() {
                // Grants decide who may write, so the file gets the same mode check as key material.
                let bytes = bounded_secret_file(path, MAX_POLICY_BYTES)?;
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| "invalid policy file encoding")?;
                let accounts = PolicyTable::parse_accounts(text).map_err(|e| match e {
                    PolicyFileError::Malformed { line } => {
                        format!("invalid account policy configuration (line {line})")
                    }
                    PolicyFileError::BadGrant { line, reason } => {
                        format!("invalid account policy configuration (line {line}): {reason}")
                    }
                    PolicyFileError::OutOfBounds => {
                        "account policy configuration exceeds bounds".to_string()
                    }
                })?;
                policies = policies.with_accounts(accounts);
            }
            let server = SqlServer::start_with_gateway(
                config,
                SqlConfig {
                    data_dir: dir.into(),
                    policies,
                    limits,
                    allow_format_upgrade,
                },
                gateway_listen.then_some(gateway),
            )
            .map_err(|e| e.to_string())?;
            Running::Sql(server)
        }
    };
    let detail = if matches!(server, Running::Sql(_)) {
        "SQL"
    } else {
        "SELECT 1 only"
    };
    // Handlers go in after startup, so recovery stays killable by the default signal action, and
    // before the line below, which is what a supervisor or test waits for before signalling us.
    stop::install_stop_signal_handlers()
        .map_err(|e| format!("cannot install signal handlers: {e}"))?;
    if gateway_tokens.is_some() {
        // SIGHUP asks the token file to be read again. Without a token file SIGHUP keeps its default.
        stop::install_reload_signal_handler()
            .map_err(|e| format!("cannot install the reload handler: {e}"))?;
    }
    let sync = server
        .gateway_addr()
        .map(|a| format!("; sync gateway on {a}"))
        .unwrap_or_default();
    println!(
        "latticed listening on {} ({mode}; {detail}{sync})",
        server.local_addr()
    );
    // With stdin control on, a reader thread forwards the line `shutdown` and end-of-file. With it
    // off stdin is never read, so /dev/null or a closed pipe (docker run -d, systemd, launchd) does
    // not stop the server. Stop signals work in both modes.
    let control = stdin_control.then(stop::spawn_stdin_control);
    let cause = stop::wait_for_stop(control.as_ref(), stop::stop_signals_received, || {
        server.is_finished()
    });
    match &cause {
        StopCause::Signal => eprintln!("latticed: stop signal received, shutting down"),
        StopCause::StdinClosed => eprintln!(
            "latticed: stdin closed, shutting down (pass --stdin-control off to run under a supervisor)"
        ),
        StopCause::StdinCommand | StopCause::ListenerExited | StopCause::ControlFailed(_) => {}
    }
    let outcome = stop::stop_within(
        move || server.shutdown(),
        stop::stop_signals_received,
        stop_timeout,
    );
    match (cause, outcome) {
        (_, StopOutcome::Forced(by)) => {
            let why = match by {
                ForcedBy::SecondSignal => "second stop signal",
                ForcedBy::Timeout => "stop timeout",
            };
            eprintln!("latticed: shutdown abandoned ({why}); exiting without finishing it");
            Ok(Exit::Forced)
        }
        (_, StopOutcome::Failed(error)) => Err(error),
        (StopCause::ControlFailed(error), StopOutcome::Clean) => Err(error),
        // Surfaces an unexpected listener exit instead of reporting a clean stop.
        (StopCause::ListenerExited, StopOutcome::Clean) => {
            Err("listener stopped unexpectedly".into())
        }
        (_, StopOutcome::Clean) => Ok(Exit::Clean),
    }
}

/// Stat before open so a configured FIFO or device cannot block startup. This is a check, not an
/// atomic guarantee: a local attacker who can replace the path between the stat and the open can
/// still block startup (the handle check then rejects the file). Std offers no portable
/// `O_NONBLOCK | O_NOFOLLOW` open, and this crate takes no libc dependency for it.
fn require_regular(path: &str) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|_| "cannot open configured security file")?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(format!(
            "{path}: configured security path is not a regular file"
        ))
    }
}

/// Reads the certificate and key once, with the same bounds and key-file rules as the PostgreSQL
/// listener. Both must be given together.
fn load_tls(
    certificates: Option<&str>,
    private_key: Option<&str>,
) -> Result<Option<TlsConfig>, String> {
    let (cert_path, key_path) = match (certificates, private_key) {
        (None, None) => return Ok(None),
        (Some(cert), Some(key)) => (cert, key),
        (Some(_), None) => return Err("--tls-key required".into()),
        (None, Some(_)) => return Err("--tls-cert required".into()),
    };
    let cert = bounded_file(cert_path, 65536)?;
    let key = bounded_secret_file(key_path, 16384)?;
    TlsConfig::from_pem(&cert, &key)
        .map(Some)
        .map_err(|_| "invalid TLS certificate/key configuration".into())
}

enum Running {
    Placeholder(ServerHandle),
    Sql(SqlServer),
}

impl Running {
    fn local_addr(&self) -> std::net::SocketAddr {
        match self {
            Running::Placeholder(s) => s.local_addr(),
            Running::Sql(s) => s.local_addr(),
        }
    }

    fn gateway_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            Running::Placeholder(_) => None,
            Running::Sql(s) => s.gateway_addr(),
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Running::Placeholder(s) => s.is_finished(),
            Running::Sql(s) => s.is_finished(),
        }
    }

    fn shutdown(self) -> io::Result<()> {
        match self {
            Running::Placeholder(s) => s.shutdown(),
            Running::Sql(s) => s.shutdown(),
        }
    }
}

fn bounded_file(path: &str, limit: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    require_regular(path)?;
    let file = std::fs::File::open(path).map_err(|_| "cannot open configured security file")?;
    read_bounded(file, limit)
}

/// Startup secrets (TLS key, SCRAM verifiers, decoy secret, account policy) use the one private-file
/// rule of `files::read_private`: an owner-only regular file, in a directory that group and others
/// cannot write to, opened without following a symlink. The check runs on the open handle, so the
/// mode that was verified is the mode of the bytes read. Errors name the path, never the contents.
fn bounded_secret_file(path: &str, limit: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    files::read_private(Path::new(path), limit).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData => "configured security file exceeds bounds".to_string(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
            "cannot open configured security file".to_string()
        }
        _ => format!("{path}: {error}"),
    })
}

fn read_bounded(file: std::fs::File, limit: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read configured security file")?;
    if bytes.len() > limit {
        return Err("configured security file exceeds bounds".into());
    }
    Ok(bytes)
}

fn main() -> ExitCode {
    match run() {
        Ok(Exit::Clean) => ExitCode::SUCCESS,
        Ok(Exit::Forced) => ExitCode::from(2),
        Err(error) => {
            eprintln!("latticed: {error}");
            ExitCode::FAILURE
        }
    }
}
