`clients/python` (WP51, #253, second slice): an experimental Python client (Python 3.10 or later, standard library only, typed). It ports
the sync frame codec, the change feed cursor and event codec and the client state machine, and replays every vector in
`conformance/vectors.json` (shared with the TypeScript client, not regenerated) byte for byte with the same error kinds as Rust.
A `clients-python` workflow runs the tests on Python 3.10 and 3.13. No transport, no packaging, no Go client and no generator yet;
the formats stay unaccepted candidates.
