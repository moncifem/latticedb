//! `latticed` as a service, through its real binary: a stop signal ends it cleanly, stdin control can
//! be switched off so a supervisor that gives it `/dev/null` or a closed pipe does not stop it, and
//! the exit status says how the stop went.
//!
//! The forced-stop paths (second signal, stop timeout) need a worker that never returns from
//! shutdown, which the real server does not have; they are covered with an injected stuck shutdown
//! in the unit tests of `latticed::stop`.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("latticed-service-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
    fn arg(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Stdin {
    Null,
    Pipe,
}

/// A running `latticed`; dropping it kills the process so a failed test leaves nothing behind.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    address: SocketAddr,
}

fn start(stdin: Stdin, args: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_latticed"))
        .args(["--listen", "127.0.0.1:0"])
        .args(args)
        .stdin(match stdin {
            Stdin::Null => Stdio::null(),
            Stdin::Pipe => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read on a thread so a child that never prints cannot hang the test until the CI job limit.
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut banner = String::new();
        let _ = BufReader::new(stdout).read_line(&mut banner);
        let _ = sender.send(banner);
    });
    let banner = match receiver.recv_timeout(Duration::from_secs(30)) {
        Ok(banner) => banner,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("latticed printed no startup line within 30 s");
        }
    };
    assert!(banner.starts_with("latticed listening on "), "{banner:?}");
    let address = banner.split_whitespace().nth(3).unwrap().parse().unwrap();
    let stdin = child.stdin.take();
    Server {
        child,
        stdin,
        address,
    }
}

impl Server {
    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success(), "kill -{name} failed");
    }

    fn wait(&mut self, within: Duration) -> Option<ExitStatus> {
        let until = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    /// Sends `signal` and returns the stderr of a process that must exit with status 0 within 10 s.
    fn terminate_cleanly(mut self, signal: &str) -> String {
        self.signal(signal);
        let status = self
            .wait(Duration::from_secs(10))
            .unwrap_or_else(|| panic!("did not stop on SIG{signal}"));
        assert_eq!(status.code(), Some(0), "SIG{signal} must be a clean stop");
        self.stderr()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn refuses_to_start(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_latticed"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{args:?}");
    String::from_utf8(output.stderr).unwrap()
}

// --- a minimal PostgreSQL v3 client (own framing, no driver) -------------------------------------

fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0; 5];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    let mut body = vec![0; length - 4];
    stream.read_exact(&mut body).unwrap();
    (header[0], body)
}

fn until_ready(stream: &mut impl Read) {
    loop {
        let (tag, body) = message(stream);
        assert_ne!(
            tag,
            b'E',
            "error reply: {:?}",
            String::from_utf8_lossy(&body)
        );
        if tag == b'Z' {
            return;
        }
    }
}

fn connect(address: SocketAddr) -> TcpStream {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut startup = vec![0, 0, 0, 0, 0, 3, 0, 0];
    startup.extend_from_slice(b"user\0dev\0\0");
    let length = (startup.len() as u32).to_be_bytes();
    startup[..4].copy_from_slice(&length);
    socket.write_all(&startup).unwrap();
    until_ready(&mut socket);
    socket
}

fn run(stream: &mut TcpStream, sql: &str) {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    stream.write_all(&frame(b'Q', &body)).unwrap();
    until_ready(stream);
}

fn directory_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(directory_bytes(&path));
        } else {
            out.extend(std::fs::read(path).unwrap());
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// --- tests ---------------------------------------------------------------------------------------

#[test]
fn sigterm_stops_the_listener_cleanly_and_the_log_reopens_without_repair() {
    let scratch = Scratch::new("sigterm");
    let data = scratch.arg("data");
    let args = ["--data-dir", &data, "--development-policy", "allow-all"];

    // Default stdin control, stdin held open: a signal works in the interactive mode as well.
    let server = start(Stdin::Pipe, &args);
    let address = server.address;
    let mut client = connect(address);
    run(
        &mut client,
        "CREATE TABLE kept (id INT PRIMARY KEY, label TEXT)",
    );
    run(
        &mut client,
        "INSERT INTO kept VALUES (1, 'marker-sigterm-7d3a')",
    );
    // The client stays connected: the stop must interrupt it, not wait for it.
    let report = server.terminate_cleanly("TERM");
    assert_eq!(report, "latticed: stop signal received, shutting down\n");
    drop(client);
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_err(),
        "the listener is still accepting after the process exited"
    );
    assert!(
        contains(
            &directory_bytes(&scratch.path("data")),
            b"marker-sigterm-7d3a"
        ),
        "the committed row is not in the log"
    );

    // The data-directory lock was released and recovery has nothing to repair or report.
    let server = start(Stdin::Pipe, &args);
    let report = server.terminate_cleanly("TERM");
    assert_eq!(report, "latticed: stop signal received, shutting down\n");
    assert!(
        !report.contains("repair") && !report.contains("discard"),
        "{report:?}"
    );
}

#[test]
fn sigint_is_the_same_clean_stop() {
    let server = start(Stdin::Pipe, &[]);
    let report = server.terminate_cleanly("INT");
    assert_eq!(report, "latticed: stop signal received, shutting down\n");
}

#[test]
fn stdin_eof_does_not_stop_the_server_when_stdin_control_is_off() {
    for stdin in [Stdin::Null, Stdin::Pipe] {
        let mut server = start(stdin, &["--stdin-control", "off"]);
        // A pipe whose writer is gone reads as end-of-file, like /dev/null.
        drop(server.stdin.take());
        // Several supervisor polls pass and the server still serves.
        assert!(
            server.wait(Duration::from_millis(1500)).is_none(),
            "stopped on EOF"
        );
        let mut client = connect(server.address);
        run(&mut client, "SELECT 1");
        drop(client);
        let report = server.terminate_cleanly("TERM");
        assert_eq!(report, "latticed: stop signal received, shutting down\n");
    }
}

#[test]
fn stdin_control_off_ignores_a_shutdown_line_too() {
    let mut server = start(Stdin::Pipe, &["--stdin-control", "off"]);
    let mut stdin = server.stdin.take().unwrap();
    stdin.write_all(b"shutdown\n").unwrap();
    stdin.flush().unwrap();
    assert!(
        server.wait(Duration::from_millis(1500)).is_none(),
        "stopped on a shutdown line with stdin control off"
    );
    server.stdin = Some(stdin);
    server.terminate_cleanly("TERM");
}

#[test]
fn by_default_stdin_end_of_file_still_stops_and_says_how_to_run_under_a_supervisor() {
    let mut server = start(Stdin::Null, &[]);
    let status = server
        .wait(Duration::from_secs(10))
        .expect("did not stop on EOF");
    assert_eq!(status.code(), Some(0));
    let report = server.stderr();
    assert!(report.contains("--stdin-control off"), "{report:?}");
}

#[test]
fn the_shutdown_line_still_stops_with_status_zero_and_no_signal_message() {
    let mut server = start(Stdin::Pipe, &["--stdin-control", "on"]);
    let mut stdin = server.stdin.take().unwrap();
    stdin.write_all(b"shutdown\n").unwrap();
    stdin.flush().unwrap();
    let status = server.wait(Duration::from_secs(10)).expect("did not stop");
    assert_eq!(status.code(), Some(0));
    assert_eq!(server.stderr(), "");
}

#[test]
fn the_service_options_are_validated() {
    assert_eq!(
        refuses_to_start(&["--stdin-control", "maybe"]),
        "latticed: --stdin-control must be on or off\n"
    );
    assert_eq!(
        refuses_to_start(&["--stop-timeout-ms", "0"]),
        "latticed: invalid stop timeout: must be positive\n"
    );
    assert!(
        refuses_to_start(&["--stop-timeout-ms", "soon"])
            .starts_with("latticed: invalid stop timeout")
    );
    assert_eq!(
        refuses_to_start(&["--stdin-control"]),
        "latticed: missing value for --stdin-control\n"
    );
}

/// Kills the child when dropped, so a failed assertion leaves no process behind.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The first directory below `dir` (itself included) that holds a log segment: the layout is not
/// fixed by the test, so it follows wherever the first start put its segment.
fn segment_directory(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = segment_directory(&path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|e| e == "seg") {
            return path.parent().map(Path::to_path_buf);
        }
    }
    None
}

#[test]
fn a_stop_signal_during_recovery_ends_the_process_with_status_2_and_the_directory_reopens() {
    // Covers an early abandon (recovery blocked reading, nothing written): the directory is startable
    // afterwards. It does not exercise abandoning a recovery that is in the middle of a write.
    let scratch = Scratch::new("recovery-stop");
    let data = scratch.arg("data");
    let args = ["--data-dir", &data, "--development-policy", "allow-all"];

    // Lay the directory out with a normal start and stop.
    start(Stdin::Pipe, &args).terminate_cleanly("TERM");

    // A named pipe where a later log segment belongs: recovery opens it to read the header and
    // blocks until a writer appears, which none does. That stalls recovery with no timing guess.
    let segments = segment_directory(&scratch.path("data")).expect("a log segment exists");
    let stall = segments.join("00000000000000000002.seg");
    let made = Command::new("mkfifo").arg(&stall).status().unwrap();
    assert!(made.success());

    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_latticed"))
            .args(["--listen", "127.0.0.1:0", "--stdin-control", "off"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = child.0.stderr.take().unwrap();
    let (lines, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
    // The slow-start notice is printed by the watcher after the handlers are installed, so once it
    // arrives a signal can no longer hit the default action.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(left) {
            Ok(line) if line.contains("still starting") => break,
            Ok(line) => seen.push(line),
            Err(_) => {
                panic!("no 'still starting' line within 30 s (recovery did not stall?): {seen:?}")
            }
        }
    }

    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.0.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let until = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = child.0.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() >= until {
            panic!("a SIGTERM during recovery did not end the process within 10 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(exit.code(), Some(2), "{exit:?}");
    // The reader thread ends when the pipe closes, which drops the sender and ends this loop.
    let mut rest = Vec::new();
    while let Ok(line) = receiver.recv_timeout(Duration::from_secs(10)) {
        rest.push(line);
    }
    assert!(
        rest.iter()
            .any(|l| l.contains("stop signal received during startup")),
        "{rest:?}"
    );

    // The abandoned recovery left nothing behind: without the pipe the directory starts and stops.
    std::fs::remove_file(&stall).unwrap();
    let report = start(Stdin::Pipe, &args).terminate_cleanly("TERM");
    assert_eq!(report, "latticed: stop signal received, shutting down\n");
}
