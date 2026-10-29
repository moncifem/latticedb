Dependency policy (#303 item 10): `docs/dependency-policy.md` sets the checklist for adding a
dependency (licence, maintainers, `unsafe`, build scripts, footprint) and the rules and audit-note
template for the security-critical set (`rustls`, `ring`, `subtle`, `sha2`, `pbkdf2`, `hmac`). The root
`Cargo.toml`, `Cargo.lock` and `deny.toml` are now code-owner paths and a `dependencies` risk area for the
`risk-gate` check. No audits are recorded yet.
