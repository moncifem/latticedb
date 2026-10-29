`clients/typescript` (WP51, #253, first slice): an experimental TypeScript client with no runtime dependencies. It ports
the sync frame codec, the change feed cursor and event codec and the client state machine, and checks them against
`conformance/vectors.json`, which a new Rust test in `lattice-api` renders and requires to be current
(`tools/update-conformance-vectors.sh` rewrites it). A `clients-typescript` workflow builds and tests the package.
No transport, no Go or Python client and no generator yet; the formats stay unaccepted candidates.
