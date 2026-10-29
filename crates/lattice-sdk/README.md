# lattice-sdk

Layer L6.

**Responsibility.** Generated and handwritten client SDKs (Rust first).

**Must not.** Hold schema knowledge not generated from LSL.

Allowed workspace dependencies: lattice-abi, lattice-lsl.

## Modules

- `contract`: strict, bounded codec for the agent plugin contract v1 (`formats/plugin-contract-v1.md`).
  `decode_line` refuses unknown fields and versions, duplicate keys, floats, lone surrogates, invalid UTF-8
  and lines over 1 MiB; `encode_line` writes the canonical form; `Framer` splits a byte stream into lines
  with bounded memory.

## Conformance suite

`tests/support/` holds the checks any plugin can run: implement `PluginUnderTest` (send one line, return
the lines produced) and call `suite::run_all`. `tests/support/reference.rs` is an in-memory plugin that
passes them, with switchable faults; `tests/plugin_contract.rs` shows that each check fails for the fault
that breaks its rule.
