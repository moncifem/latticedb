#![no_main]
use lattice_sdk::mcp::{
    Caller, Grant, Json, Server, Tool, ToolBackend, ToolCall, ToolOutput, BackendError, parse_json,
};
use libfuzzer_sys::fuzz_target;

/// Fails the run if the scoping layer lets a call outside the grant reach the store.
struct Checked;

impl ToolBackend for Checked {
    fn call(&mut self, _: &Caller, call: &ToolCall) -> Result<ToolOutput, BackendError> {
        let s = call.scope();
        assert!(s == "a" || s.starts_with("a/"), "call escaped its grant: {s}");
        assert!(call.tool() != Tool::Write, "write reached the backend without a grant");
        Ok(ToolOutput { text: String::from("{}") })
    }
}

fuzz_target!(|data: &[u8]| {
    let caller = Caller {
        id: String::from("fuzz"),
        grants: [Tool::Read, Tool::Subscribe]
            .into_iter()
            .filter_map(|t| Grant::new(t, "a"))
            .collect(),
    };
    let mut server = Server::new(Checked);
    // Whatever the bytes, the answer (if any) is itself well-formed bounded JSON.
    if let Some(out) = server.handle(&caller, data) {
        assert!(matches!(parse_json(out.as_bytes()), Ok(Json::Obj(_))));
    }
    // The reader alone never panics either.
    let _ = parse_json(data);
});
