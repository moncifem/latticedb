#![no_main]
use lattice_sdk::mcp::{
    BackendError, Caller, Grant, Json, Server, Tool, ToolBackend, ToolCall, ToolOutput, parse_json,
};
use libfuzzer_sys::fuzz_target;

/// Fails the run if the scoping layer lets a call outside the grant reach the store.
struct Checked;

impl ToolBackend for Checked {
    fn call(&mut self, _: &Caller, call: &ToolCall) -> Result<ToolOutput, BackendError> {
        let s = call.scope();
        assert!(s == "a" || s.starts_with("a/"), "call escaped its grant: {s}");
        assert!(call.tool() != Tool::Write, "write reached the backend without a grant");
        Ok(ToolOutput::or_notice(String::from("{}")))
    }
}

const INIT: &[u8] = br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"f","version":"1"}}}"#;

fuzz_target!(|data: &[u8]| {
    let caller = Caller {
        id: String::from("fuzz"),
        grants: [Tool::Read, Tool::Subscribe]
            .into_iter()
            .filter_map(|t| Grant::new(t, "a"))
            .collect(),
    };
    // Once as a stateless (2026-07-28) connection, once as a connection that opened with initialize.
    let mut stateless = Server::new(Checked);
    let mut legacy = Server::new(Checked);
    assert!(legacy.handle(&caller, INIT).is_some());
    for server in [&mut stateless, &mut legacy] {
        // Whatever the bytes, the answer (if any) is itself well-formed bounded JSON.
        if let Some(out) = server.handle(&caller, data) {
            assert!(matches!(parse_json(out.as_bytes()), Ok(Json::Obj(_))));
        }
    }
    // The reader alone never panics either.
    let _ = parse_json(data);
});
