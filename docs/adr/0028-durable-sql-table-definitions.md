# ADR 0028: Durable SQL table definitions in a guarded compound change

Status: proposed

Requirement P2-01, P2-02. Design discussion: [RFC 0012](../rfc/0012-durable-sql-table-definitions.md).
This ADR is not accepted. The format is the owner's decision; nothing here is implemented.

## Context

The engine catalog persists table ids and names. The executor keeps columns, types, constraints and
defaults in memory and asks the host to re-register them after a restart (`Database::attach_table`),
checking only the id and name. A recovered table is unknown to SQL, a host can supply a weaker
definition without detection, and stored rows cannot be decoded without the definition that wrote them
(issue 379). The accepted formats (WAL v1, the `LATCAP02` guard of ADR 0022, LCAT v1, LROW v1, LCIX v1
of ADR 0019) cannot carry a definition. The typed-catalog proposal (RFC 0001 / ADR 0015) is draft,
describes LSL entities, and cannot express the SQL types or constraints.

## Decision

Proposed, not accepted: persist each SQL table definition in a new definition block of the compound
envelope (LCIX version 3), in the same change set as the catalog operation that creates the table.

- Capability bit 3 (value 8) is required for any segment that carries a definition; version 3 needs
  mask 9 (managed compound plus definitions). Header preflight refuses it in every reader that does not
  support the bit, before callbacks or repair. A change without definitions stays LCIX version 1.
- A definition is a strict, bounded, canonical record (version 1, layout rule 1): ordered columns with
  closed type tags, `NOT NULL`, constant defaults, primary key, `UNIQUE` constraints by position, and
  constraint names. It does not contain the table name. A table has at most one definition for the life
  of its id, and it is immutable; later schema changes are new generations (RFC 0004).
- The engine validates structure at commit and at replay without depending on SQL crates. The executor
  re-validates semantics when it builds its in-memory table from a recovered definition and refuses
  to open on failure.
- A row write to a defined table must find the table live in the post-change catalog of its own change
  set. Tables without a definition stay raw and not addressable by SQL; a definition is never guessed.
- Existing directories need an explicit format-upgrade setting before the first definition is written.
  Legacy tables can be adopted only through a verified, bounded streaming pass over every retained row
  version, which is stated to be unable to prove intent.
- Version 2 of LCAT and of LCIX and capability bit 2 stay reserved for RFC 0001. Option B (definition
  rows in a reserved table) and Option C (reuse the typed catalog) are rejected for the reasons in the
  RFC; B remains the fallback.

The decisions listed at the end of the RFC are the owner's.

## Consequences

SQL schemas survive a restart, the weaker-definition hazard disappears because the durable definition is
the comparison key, and the in-process fingerprint and the host's `attach_table` call leave normal
startup. The record is the atomic unit that transactional DDL (issue 382) needs and does not implement
it. Old readers refuse; there is no in-place downgrade, and rollback restores a backup taken before the
first definition. The catalog grows by one shared copy of each definition. Replay gains validation work
per definition.

Not decided or not claimed: a checkpoint format (RFC 0002; the RFC lists what a checkpoint would have to
carry), in-place layout versions, non-constant defaults, `numeric(p, s)`, sequences, online schema
change, a phase gate, or a release. No evidence exists yet: acceptance requires the independent golden
fixtures, planted-bug witnesses, the power-cut sweep, a frozen-reader hosted proof and an independent
review of the final diff, recorded in `docs/evidence/`.
