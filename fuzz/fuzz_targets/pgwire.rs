#![no_main]
use lattice_api::pgwire::{Backend, Column, Frontend, decode_frontend, decode_startup};
use lattice_api::session::{QueryError, QueryHandler, QueryOutput, Session};
use libfuzzer_sys::fuzz_target;

/// A database that fails every statement with the text it was sent.
struct Refuse;

impl QueryHandler for Refuse {
    fn execute(&mut self, sql: &str, _: &[Option<Vec<u8>>]) -> Result<QueryOutput, QueryError> {
        if sql.len().is_multiple_of(3) {
            return Err(QueryError::new("42601", sql));
        }
        Ok(QueryOutput {
            tag: "OK".into(),
            ..QueryOutput::default()
        })
    }

    fn describe(&mut self, sql: &str) -> Result<Vec<Column>, QueryError> {
        if sql.len().is_multiple_of(5) {
            return Err(QueryError::new("42601", sql));
        }
        Ok(Vec::new())
    }
}

fuzz_target!(|data: &[u8]| {
    // Whatever the bytes decode to, a session fed with it never panics and every reply encodes.
    let mut session = Session::new(Refuse, 1, 2);
    let mut at = 0;
    if let Ok(Some((startup, used))) = decode_startup(data) {
        at = used;
        for m in session.on_startup(startup).messages {
            assert!(m.encode(&mut Vec::new()).is_ok());
        }
    }
    while let Some(rest) = data.get(at..) {
        let out = match decode_frontend(rest) {
            Ok(Some((msg, used))) => {
                at += used.max(1);
                session.on_message(msg)
            }
            Ok(None) => break,
            Err(e) => session.on_protocol_error(&e),
        };
        for m in out.messages {
            assert!(m.encode(&mut Vec::new()).is_ok());
        }
        if out.close {
            break;
        }
    }
    // Neither decoder may panic, and a decoded message never claims more bytes than it was given.
    if let Ok(Some((_, used))) = decode_frontend(data) {
        assert!(used <= data.len());
    }
    // A decoded Bind never has more parameters than bytes could hold.
    if let Ok(Some((Frontend::Bind { params, .. }, used))) = decode_frontend(data) {
        assert!(params.len() <= used);
    }
    if let Ok(Some((_, used))) = decode_startup(data) {
        assert!(used <= data.len());
    }
    // Whatever text arrives as a query can be echoed back in a command tag or an error without
    // breaking the encoder: it either encodes with a correct length or is refused cleanly.
    if let Ok(text) = std::str::from_utf8(data) {
        for msg in [
            Backend::CommandComplete(text.to_string()),
            Backend::ErrorResponse {
                severity: "ERROR".into(),
                code: "42601".into(),
                message: text.to_string(),
            },
        ] {
            let mut out = Vec::new();
            if msg.encode(&mut out).is_ok() {
                let declared = u32::from_be_bytes([out[1], out[2], out[3], out[4]]) as usize;
                assert_eq!(declared, out.len() - 1);
            }
        }
    }
});
