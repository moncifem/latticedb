//! Session edges found by mutation testing: the startup and SASL phases as reported to the host, the
//! `Flush` message, a nested `BEGIN` after a failure, and rebinding a named portal.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::auth::{Accounts, DecoySecret};
use lattice_api::pgwire::Column;
use lattice_api::pgwire::{Backend, Frontend, SaslPhase, Startup, Target, TxStatus};
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

/// The largest value one DataRow holds in a single column. The length field (4) counts itself, the
/// column count (2) and the value's length word (4) come before the bytes: 4 + 2 + 4 + n <= 1 MiB.
const MAX_ONE_CELL: usize = (1 << 20) - 4 - 2 - 4;
/// The longest column name whose RowDescription fits one message: length field (4), column count
/// (2), then the name, its terminator (1) and 18 fixed bytes (4 + 2 + 4 + 2 + 4 + 2).
const MAX_ONE_NAME: usize = (1 << 20) - 4 - 2 - 1 - 18;

/// Answers `BIG` with one row of `value` bytes and `SMALL` with a one-byte row, both in one column
/// whose name is `name` bytes long. Other statements return their own text as the command tag.
struct Wide {
    value: usize,
    name: usize,
}

impl Wide {
    fn column(&self) -> Column {
        Column {
            name: "v".repeat(self.name),
            table_oid: 0,
            column_attr: 0,
            type_oid: 25,
            type_size: -1,
            type_modifier: -1,
            format: 0,
        }
    }
}

impl QueryHandler for Wide {
    fn execute(
        &mut self,
        _: &QueryContext<'_>,
        sql: &str,
        _: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        let rows = match sql {
            "BIG" => vec![vec![Some(vec![b'x'; self.value])]],
            "SMALL" => vec![vec![Some(b"1".to_vec())]],
            other => {
                return Ok(QueryOutput {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    tag: other.to_string(),
                });
            }
        };
        Ok(QueryOutput {
            columns: vec![self.column()],
            rows,
            tag: "SELECT 1".into(),
        })
    }
    fn describe(&mut self, _: &QueryContext<'_>, sql: &str) -> Result<Vec<Column>, QueryError> {
        Ok(match sql {
            "BIG" | "SMALL" => vec![self.column()],
            _ => Vec::new(),
        })
    }
}

fn wide(value: usize, name: usize) -> Session<Wide> {
    let mut s = Session::local_trust(Wide { value, name }, 1, 2);
    s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
    s
}

fn is_too_large(message: &Backend) -> bool {
    matches!(message, Backend::ErrorResponse { code, .. } if code == "54000")
}

fn sends_no_rows(messages: &[Backend]) -> bool {
    !messages
        .iter()
        .any(|m| matches!(m, Backend::RowDescription(_) | Backend::DataRow(_)))
}

#[test]
fn a_row_one_byte_over_one_message_fails_the_statement_and_the_connection_stays_usable() {
    let mut s = wide(MAX_ONE_CELL + 1, 1);
    let out = s.on_message(Frontend::Query("BIG".into()));
    assert!(sends_no_rows(&out.messages), "{:?}", out.messages);
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Idle)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );
    assert!(!out.close);
    let next = s.on_message(Frontend::Query("SMALL".into()));
    assert!(
        matches!(
            next.messages.as_slice(),
            [
                Backend::RowDescription(_),
                Backend::DataRow(_),
                Backend::CommandComplete(_),
                Backend::ReadyForQuery(TxStatus::Idle)
            ]
        ),
        "{:?}",
        next.messages
    );
}

#[test]
fn a_row_exactly_at_the_limit_is_sent_and_the_encoder_agrees_on_the_boundary() {
    // The encoder is the reference: the largest DataRow it accepts is the largest the session sends.
    let fits = Backend::DataRow(vec![Some(vec![b'x'; MAX_ONE_CELL])]);
    let mut bytes = Vec::new();
    fits.encode(&mut bytes).unwrap();
    assert_eq!(bytes.get(1..5), Some(&(1u32 << 20).to_be_bytes()[..]));
    let one_more = Backend::DataRow(vec![Some(vec![b'x'; MAX_ONE_CELL + 1])]);
    assert!(one_more.encode(&mut Vec::new()).is_err());

    let mut s = wide(MAX_ONE_CELL, 1);
    let out = s.on_message(Frontend::Query("BIG".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [
                Backend::RowDescription(_),
                Backend::DataRow(_),
                Backend::CommandComplete(_),
                Backend::ReadyForQuery(_)
            ]
        ),
        "{:?}",
        out.messages
    );
}

#[test]
fn an_oversize_row_inside_a_transaction_block_fails_the_block() {
    let mut s = wide(MAX_ONE_CELL + 1, 1);
    s.on_message(Frontend::Query("BEGIN".into()));
    let out = s.on_message(Frontend::Query("BIG".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Failed)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );
    assert_eq!(s.tx_status(), TxStatus::Failed);
    let blocked = s.on_message(Frontend::Query("SMALL".into()));
    assert!(
        matches!(blocked.messages.first(), Some(Backend::ErrorResponse { code, .. }) if code == "25P02"),
        "{:?}",
        blocked.messages
    );
    s.on_message(Frontend::Query("ROLLBACK".into()));
    assert_eq!(s.tx_status(), TxStatus::Idle);
}

#[test]
fn an_oversize_row_on_execute_is_refused_and_the_portal_is_not_run_again() {
    let mut s = wide(MAX_ONE_CELL + 1, 1);
    s.on_message(Frontend::Parse {
        name: "st".into(),
        query: "BIG".into(),
        param_types: vec![],
    });
    s.on_message(Frontend::Bind {
        portal: "p".into(),
        statement: "st".into(),
        param_formats: vec![],
        params: vec![],
        result_formats: vec![],
    });
    let out = s.on_message(Frontend::Execute {
        portal: "p".into(),
        max_rows: 0,
    });
    assert!(
        matches!(out.messages.as_slice(), [e] if is_too_large(e)),
        "{:?}",
        out.messages
    );
    let synced = s.on_message(Frontend::Sync);
    assert!(
        matches!(
            synced.messages.as_slice(),
            [Backend::ReadyForQuery(TxStatus::Idle)]
        ),
        "{:?}",
        synced.messages
    );
    // The portal went with the error, so a retry does not run the statement a second time.
    let again = s.on_message(Frontend::Execute {
        portal: "p".into(),
        max_rows: 0,
    });
    assert!(
        matches!(again.messages.first(), Some(Backend::ErrorResponse { code, .. }) if code == "34000"),
        "{:?}",
        again.messages
    );
}

#[test]
fn an_oversize_row_on_execute_inside_a_block_leaves_the_block_failed_at_sync() {
    let mut s = wide(MAX_ONE_CELL + 1, 1);
    s.on_message(Frontend::Query("BEGIN".into()));
    s.on_message(Frontend::Parse {
        name: "st".into(),
        query: "BIG".into(),
        param_types: vec![],
    });
    s.on_message(Frontend::Bind {
        portal: "p".into(),
        statement: "st".into(),
        param_formats: vec![],
        params: vec![],
        result_formats: vec![],
    });
    let out = s.on_message(Frontend::Execute {
        portal: "p".into(),
        max_rows: 0,
    });
    assert!(
        matches!(out.messages.as_slice(), [e] if is_too_large(e)),
        "{:?}",
        out.messages
    );
    let synced = s.on_message(Frontend::Sync);
    assert!(
        matches!(
            synced.messages.as_slice(),
            [Backend::ReadyForQuery(TxStatus::Failed)]
        ),
        "{:?}",
        synced.messages
    );
}

#[test]
fn a_row_description_one_byte_over_one_message_is_refused_before_anything_is_sent() {
    let column = |n: usize| Column {
        name: "v".repeat(n),
        table_oid: 0,
        column_attr: 0,
        type_oid: 25,
        type_size: -1,
        type_modifier: -1,
        format: 0,
    };
    assert!(
        Backend::RowDescription(vec![column(MAX_ONE_NAME)])
            .encode(&mut Vec::new())
            .is_ok()
    );
    assert!(
        Backend::RowDescription(vec![column(MAX_ONE_NAME + 1)])
            .encode(&mut Vec::new())
            .is_err()
    );

    let mut refused = wide(1, MAX_ONE_NAME + 1);
    let out = refused.on_message(Frontend::Query("SMALL".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Idle)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );

    let mut sent = wide(1, MAX_ONE_NAME);
    let out = sent.on_message(Frontend::Query("SMALL".into()));
    assert!(
        matches!(out.messages.first(), Some(Backend::RowDescription(_))),
        "{:?}",
        out.messages
    );
}

#[test]
fn a_describe_whose_row_description_is_too_large_sends_no_parameter_description() {
    let mut s = wide(1, MAX_ONE_NAME + 1);
    let parsed = s.on_message(Frontend::Parse {
        name: "st".into(),
        query: "SMALL".into(),
        param_types: vec![],
    });
    assert!(
        matches!(parsed.messages.as_slice(), [Backend::ParseComplete]),
        "{:?}",
        parsed.messages
    );
    let out = s.on_message(Frontend::Describe(Target::Statement("st".into())));
    assert!(
        matches!(out.messages.as_slice(), [e] if is_too_large(e)),
        "{:?}",
        out.messages
    );
}

/// Answers `ROW` with `columns` empty-named text columns and one row of `row` cells: `Some(n)` is a
/// value of `n` bytes, `None` is NULL. The two can differ, which is how a row longer than its column
/// list reaches the session. Other statements return their text as the command tag.
struct Cells {
    columns: usize,
    row: Vec<Option<usize>>,
}

impl QueryHandler for Cells {
    fn execute(
        &mut self,
        _: &QueryContext<'_>,
        sql: &str,
        _: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        if sql != "ROW" {
            return Ok(QueryOutput {
                columns: Vec::new(),
                rows: Vec::new(),
                tag: sql.to_string(),
            });
        }
        let columns = (0..self.columns)
            .map(|_| Column {
                name: String::new(),
                table_oid: 0,
                column_attr: 0,
                type_oid: 25,
                type_size: -1,
                type_modifier: -1,
                format: 0,
            })
            .collect();
        let row = self
            .row
            .iter()
            .map(|cell| cell.map(|n| vec![b'x'; n]))
            .collect();
        Ok(QueryOutput {
            columns,
            rows: vec![row],
            tag: "SELECT 1".into(),
        })
    }
    fn describe(&mut self, _: &QueryContext<'_>, _: &str) -> Result<Vec<Column>, QueryError> {
        Ok(Vec::new())
    }
}

fn cells(columns: usize, row: Vec<Option<usize>>) -> Session<Cells> {
    let mut s = Session::local_trust(Cells { columns, row }, 1, 2);
    s.on_startup(Startup::Params(vec![("user".into(), "u".into())]));
    s
}

#[test]
fn two_cells_that_each_fit_but_sum_past_the_limit_are_refused() {
    // Each cell alone is within one message. The DataRow is 4 + 2 + (4 + n) per cell, so two cells of
    // n bytes take 14 + 2n: 2 x 524,284 is 1,048,582 bytes, past 1 MiB (1,048,576).
    let mut over = cells(2, vec![Some(524_284), Some(524_284)]);
    let out = over.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Idle)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );
    // 2 x 524,281 is 14 + 1,048,562 = 1,048,576: exactly the limit, so it is sent.
    let fits = Backend::DataRow(vec![Some(vec![b'x'; 524_281]), Some(vec![b'x'; 524_281])]);
    assert!(fits.encode(&mut Vec::new()).is_ok());
    let one_more = Backend::DataRow(vec![Some(vec![b'x'; 524_282]), Some(vec![b'x'; 524_281])]);
    assert!(one_more.encode(&mut Vec::new()).is_err());
    let mut at = cells(2, vec![Some(524_281), Some(524_281)]);
    let out = at.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [
                Backend::RowDescription(_),
                Backend::DataRow(_),
                Backend::CommandComplete(_),
                Backend::ReadyForQuery(_)
            ]
        ),
        "{:?}",
        out.messages
    );
}

#[test]
fn a_null_cell_costs_its_length_word_and_no_value_bytes() {
    // A NULL is a length word of -1 and nothing after it. A NULL plus one value of n bytes is
    // 6 + 4 + (4 + n) = 14 + n, so n = 1,048,562 is exactly the limit.
    let fits = Backend::DataRow(vec![None, Some(vec![b'x'; 1_048_562])]);
    let mut bytes = Vec::new();
    fits.encode(&mut bytes).unwrap();
    assert_eq!(bytes.get(1..5), Some(&(1u32 << 20).to_be_bytes()[..]));

    let mut at = cells(2, vec![None, Some(1_048_562)]);
    let out = at.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [
                Backend::RowDescription(_),
                Backend::DataRow(_),
                Backend::CommandComplete(_),
                Backend::ReadyForQuery(_)
            ]
        ),
        "{:?}",
        out.messages
    );

    let mut over = cells(2, vec![None, Some(1_048_563)]);
    let out = over.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Idle)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );
}

#[test]
fn a_row_of_more_than_i16_max_columns_is_refused_and_i16_max_is_sent() {
    // The column count is an i16 on the wire, so 32,767 is the most one message can say. The
    // RowDescription guard refuses 32,768 columns before the row is looked at, and the DataRow guard
    // refuses a 32,768-cell row whose column list is shorter.
    let mut at_limit = cells(32_767, vec![None; 32_767]);
    let out = at_limit.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [
                Backend::RowDescription(_),
                Backend::DataRow(_),
                Backend::CommandComplete(_),
                Backend::ReadyForQuery(_)
            ]
        ),
        "{:?}",
        out.messages
    );

    let mut too_many_columns = cells(32_768, vec![None; 32_768]);
    let out = too_many_columns.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Idle)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );

    let mut too_many_cells = cells(1, vec![None; 32_768]);
    let out = too_many_cells.on_message(Frontend::Query("ROW".into()));
    assert!(
        matches!(
            out.messages.as_slice(),
            [e, Backend::ReadyForQuery(TxStatus::Idle)] if is_too_large(e)
        ),
        "{:?}",
        out.messages
    );

    // The encoder refuses a 32,768-value DataRow, so the session is right to refuse it.
    assert!(
        Backend::DataRow(vec![None; 32_768])
            .encode(&mut Vec::new())
            .is_err()
    );
}

// Parameter counts. The protocol document (PostgreSQL, "Message Formats") says ParameterDescription
// carries one OID per parameter of the statement, and that Parse may declare fewer types than the
// statement has parameters; OID 0 means the type is unspecified. The expected counts below come
// from reading each statement text by hand, not from running the scanner.

fn describe_params(sql: &str, declared: Vec<i32>) -> Vec<i32> {
    let mut s = ready();
    let parsed = s.on_message(Frontend::Parse {
        name: "d".into(),
        query: sql.into(),
        param_types: declared,
    });
    assert_eq!(parsed.messages, vec![Backend::ParseComplete], "{sql}");
    let out = s.on_message(Frontend::Describe(Target::Statement("d".into())));
    match out.messages.first() {
        Some(Backend::ParameterDescription(types)) => types.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_statement_with_undeclared_placeholders_is_described_with_the_real_count() {
    let cases: &[(&str, usize)] = &[
        ("SELECT 1", 0),
        ("INSERT INTO t VALUES ($1, $2, $3, $4)", 4),
        ("SELECT id FROM t WHERE id = $1 OR name = $2", 2),
        ("SELECT $3", 3),
        ("SELECT $1, $1", 1),
        ("SELECT $1::int, $2::text", 2),
        ("SELECT '$5'", 0),
        ("SELECT 'it''s $5', $1", 1),
        ("SELECT E'a\\'$5', $2", 2),
        ("SELECT \"c$5\", $1", 1),
        ("SELECT $$ $7 $$, $1", 1),
        ("SELECT $q$ $$ $7 $q$, $2", 2),
        ("-- $9\nSELECT $1", 1),
        ("/* $9 /* $8 */ $7 */ SELECT $2", 2),
        ("SELECT a$5", 0),
        ("SELECT $a$ x $a$ $1", 1),
        ("SELECT $32767", 32_767),
        ("SELECT $32768", 0),
        ("SELECT $99999999999999999999999999", 0),
        ("SELECT $0", 0),
        ("SELECT 'x $5", 0),
        ("SELECT $a$ $5", 0),
        ("SELECT /* $5", 0),
        ("SELECT \"x $5", 0),
        ("SELECT $", 0),
        ("SELECT $01", 1),
        ("SELECT $1a", 1),
        ("SELECT 1 -- $9\r, $1", 1),
        ("SELECT U&'\\0041$5', $1", 1),
        ("SELECT $1$2", 2),
        ("SELECT $1$$ $7 $$", 1),
    ];
    for &(sql, count) in cases {
        assert_eq!(describe_params(sql, vec![]), vec![0; count], "{sql}");
    }
}

#[test]
fn declared_types_are_kept_and_only_the_missing_ones_are_unspecified() {
    assert_eq!(describe_params("SELECT $1, $2", vec![25]), [25, 0]);
    assert_eq!(describe_params("SELECT $1", vec![25, 23, 20]), [25, 23, 20]);
    assert_eq!(describe_params("SELECT $3", vec![0, 23]), [0, 23, 0]);
}

#[test]
fn bind_must_supply_the_count_the_statement_text_implies() {
    let mut s = ready();
    s.on_message(Frontend::Parse {
        name: "two".into(),
        query: "SELECT $1, $2".into(),
        param_types: vec![],
    });
    let bind_with = |s: &mut Session<Tagged>, n: usize| {
        s.on_message(Frontend::Bind {
            portal: String::new(),
            statement: "two".into(),
            param_formats: vec![],
            params: vec![Some(b"1".to_vec()); n],
            result_formats: vec![],
        })
        .messages
    };
    for wrong in [1, 3] {
        let out = bind_with(&mut s, wrong);
        assert!(
            matches!(out.first(), Some(Backend::ErrorResponse { code, .. }) if code == "08P01"),
            "{wrong}: {out:?}"
        );
        s.on_message(Frontend::Sync);
    }
    assert_eq!(bind_with(&mut s, 2), vec![Backend::BindComplete]);
}

#[test]
fn bind_to_a_statement_without_placeholders_must_supply_no_parameters() {
    let mut s = ready();
    s.on_message(Frontend::Parse {
        name: "none".into(),
        query: "SELECT 1".into(),
        param_types: vec![],
    });
    let bind_with = |s: &mut Session<Tagged>, n: usize| {
        s.on_message(Frontend::Bind {
            portal: String::new(),
            statement: "none".into(),
            param_formats: vec![],
            params: vec![Some(b"1".to_vec()); n],
            result_formats: vec![],
        })
        .messages
    };
    let out = bind_with(&mut s, 1);
    assert!(
        matches!(out.first(), Some(Backend::ErrorResponse { code, .. }) if code == "08P01"),
        "{out:?}"
    );
    s.on_message(Frontend::Sync);
    assert_eq!(bind_with(&mut s, 0), vec![Backend::BindComplete]);
}

#[test]
fn a_statement_with_the_most_placeholders_is_charged_and_refused_over_a_small_budget() {
    // $32767 pads the type list to 32,767 four-byte entries (131,068 bytes), past this budget.
    let mut s = ready().with_portal_budget(100_000);
    let parse = |s: &mut Session<Tagged>, sql: &str| {
        s.on_message(Frontend::Parse {
            name: String::new(),
            query: sql.into(),
            param_types: vec![],
        })
        .messages
    };
    assert_eq!(parse(&mut s, "SELECT $1"), vec![Backend::ParseComplete]);
    let out = parse(&mut s, "SELECT $32767");
    assert!(
        matches!(out.first(), Some(Backend::ErrorResponse { code, .. }) if code == "53200"),
        "{out:?}"
    );
    s.on_message(Frontend::Sync);
    // The refused Parse left the old unnamed statement in place.
    let out = s.on_message(Frontend::Describe(Target::Statement(String::new())));
    assert_eq!(
        out.messages.first(),
        Some(&Backend::ParameterDescription(vec![0]))
    );
}

#[test]
fn hostile_statements_are_counted_exactly_at_every_cut() {
    // Each text is read by hand. A scanner that reads a quoted region wrongly moves a count.
    let count_of = |sql: &str| describe_params(sql, vec![]).len();

    // Text 1: the E-string opened at `E'` swallows the backslash-escaped quote and everything
    // after it, because no closing quote follows. Every cut counts 0.
    let t1 = "SELECT E'a\\' $$ $q$ /* /* -- \" $1 '' $9 $a$ */ */ $2 \"\" $$ $q$ $3 ";
    for end in 0..=t1.len() {
        assert_eq!(count_of(t1.get(..end).unwrap()), 0, "{end}");
    }

    // Text 2: `$$ ... $$` is a dollar quote that hides $1 and $9; `$2` after it is real; the
    // `$q$` that follows never closes, so $3 and $4 are inert. Count: 0 before `$2`, 2 from there.
    let t2 = "SELECT $$ $q$ /* /* -- \" $1 '' $9 $a$ */ */ $$ $2 $q$ $3 $$ $4 ";
    let at2 = t2.find("$2").unwrap();
    for end in 0..=t2.len() {
        let want = if end > at2 + 1 { 2 } else { 0 };
        assert_eq!(count_of(t2.get(..end).unwrap()), want, "{end}");
    }

    // Text 3: the nested comment closes, the line comment ends at the newline, `$5` is real, the
    // quoted identifier and the string hide $6 and $7, and `$8` is real.
    let t3 = "SELECT 1 /* /* */ */ -- x\n, $5 \"q\"\"$6\" 'a''$7' $8";
    let at5 = t3.find("$5").unwrap();
    let at8 = t3.find("$8").unwrap();
    for end in 0..=t3.len() {
        let want = if end > at8 + 1 {
            8
        } else if end > at5 + 1 {
            5
        } else {
            0
        };
        assert_eq!(count_of(t3.get(..end).unwrap()), want, "{end}");
    }

    assert_eq!(count_of(&"$".repeat(100_000)), 0);
    assert_eq!(count_of(&"/*".repeat(100_000)), 0);
}
