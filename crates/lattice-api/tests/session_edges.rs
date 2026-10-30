//! Session edges found by mutation testing: the startup and SASL phases as reported to the host, the
//! `Flush` message, a nested `BEGIN` after a failure, and rebinding a named portal.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::auth::{Accounts, DecoySecret};
use lattice_api::pgwire::Column;
use lattice_api::pgwire::{Backend, Frontend, SaslPhase, Startup};
use lattice_api::principal::QueryContext;
use lattice_api::session::{QueryError, QueryHandler, QueryOutput, Session};
use lattice_seams::Rng;

/// Answers every statement with a tag taken from its text; `FAIL` is an error.
struct Tagged;
impl QueryHandler for Tagged {
    fn execute(
        &mut self,
        _: &QueryContext<'_>,
        sql: &str,
        _: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        if sql == "FAIL" {
            return Err(QueryError::new("XX000", "failed on purpose"));
        }
        Ok(QueryOutput {
            columns: Vec::new(),
            rows: Vec::new(),
            tag: sql.to_string(),
        })
    }
    fn describe(&mut self, _: &QueryContext<'_>, _: &str) -> Result<Vec<Column>, QueryError> {
        Ok(Vec::new())
    }
}

fn ready() -> Session<Tagged> {
    let mut s = Session::local_trust(Tagged, 1, 2);
    s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
    s
}

#[test]
fn the_session_reports_waiting_for_startup_only_until_startup_arrives() {
    let mut s = Session::local_trust(Tagged, 1, 2);
    assert!(s.awaiting_startup());
    s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
    assert!(!s.awaiting_startup());
    assert!(!s.is_closed());
}

#[test]
fn flush_sends_nothing_and_keeps_the_session_usable() {
    let mut s = ready();
    let out = s.on_message(Frontend::Flush);
    assert!(out.messages.is_empty(), "{:?}", out.messages);
    assert!(!out.close);
    let after = s.on_message(Frontend::Query("SELECT".into()));
    assert!(matches!(
        after.messages.last(),
        Some(Backend::ReadyForQuery(_))
    ));
}

#[test]
fn a_begin_after_a_failure_does_not_reopen_the_failed_transaction() {
    let mut s = ready();
    s.on_message(Frontend::Query("BEGIN".into()));
    s.on_message(Frontend::Query("FAIL".into()));
    let failed = s.tx_status();
    s.on_message(Frontend::Query("BEGIN".into()));
    assert_eq!(
        s.tx_status(),
        failed,
        "BEGIN must not turn a failed transaction into an open one"
    );
    s.on_message(Frontend::Query("ROLLBACK".into()));
    assert_ne!(s.tx_status(), failed);
}

fn bind(s: &mut Session<Tagged>, portal: &str) -> Vec<Backend> {
    s.on_message(Frontend::Bind {
        portal: portal.into(),
        statement: "st".into(),
        param_formats: vec![],
        params: vec![],
        result_formats: vec![],
    })
    .messages
}

fn prepared() -> Session<Tagged> {
    let mut s = ready();
    s.on_message(Frontend::Parse {
        name: "st".into(),
        query: "SELECT".into(),
        param_types: vec![],
    });
    s
}

fn is_error(messages: &[Backend]) -> bool {
    matches!(messages.first(), Some(Backend::ErrorResponse { .. }))
}

#[test]
fn a_named_portal_cannot_be_bound_twice_but_the_unnamed_one_can() {
    let mut s = prepared();
    assert!(!is_error(&bind(&mut s, "p1")));
    let again = bind(&mut s, "p1");
    assert!(
        matches!(again.first(), Some(Backend::ErrorResponse { code, .. }) if code == "42P03"),
        "{again:?}"
    );
    let mut t = prepared();
    assert!(!is_error(&bind(&mut t, "")));
    assert!(!is_error(&bind(&mut t, "")));
}

struct Counter(u64);
impl Rng for Counter {
    fn next_u64(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[test]
fn the_sasl_phase_follows_the_exchange_from_initial_response_to_response() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let line = format!(
        "u\tSCRAM-SHA-256$4096:{}${}:{}",
        STANDARD.encode([1u8; 16]),
        STANDARD.encode([2u8; 32]),
        STANDARD.encode([3u8; 32])
    );
    let accounts = Accounts::parse(&line, DecoySecret::from_bytes(&[7u8; 32]).unwrap()).unwrap();
    let (exchange, _) = accounts.exchange("u", &mut Counter(10)).unwrap();
    let mut s = Session::with_scram(Tagged, 1, 2, exchange, None);
    assert_eq!(s.sasl_phase(), None);
    s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
    assert_eq!(s.sasl_phase(), Some(SaslPhase::InitialResponse));
    s.on_message(Frontend::SaslInitialResponse {
        mechanism: "SCRAM-SHA-256".into(),
        data: Some(b"n,,n=u,r=clientnonce".to_vec()),
    });
    assert_eq!(s.sasl_phase(), Some(SaslPhase::Response));
}
