# RFC 0012: Durable SQL table definitions

Status: draft
Comment period ends: 2026-11-06
Decision ADR: proposed ADR 0028 (not accepted)

## Motivation

Issue 379. The engine catalog persists table ids and names. The executor's column definitions
(`lattice_exec::TableDef`: columns, types, `NOT NULL`, constant defaults, primary key, `UNIQUE`
constraints) live in memory. After a restart the host must re-supply them with
`Database::attach_table`, which checks only the id and the name against the engine catalog. Three
consequences follow, and each one is a durability defect rather than a missing feature:

1. A recovered table is unknown to SQL until the host re-registers it. `latticed --data-dir` (PR 412)
   answers `42P01` for every recovered table and reports them as "unattached".
2. A host that supplies a weaker or different definition is accepted. The executor stops enforcing the
   dropped constraint and decodes stored rows with the wrong layout. The interim fingerprint added in
   PR 336 compares a definition with the first one registered in the same process; it does not survive a
   restart and cannot compare against what was written before it.
3. `UNIQUE` entries live in derived marker tables (`0x8000_0000 | table_id << 4 | k`). The index `k` is
   the position of the constraint in the definition. A definition that lists the constraints in another
   order points the executor at the wrong marker table. The mapping from a constraint to its marker table
   is therefore part of what has to be durable.

The executor's row layout is also a function of the definition: `TableDef::layout()` maps each SQL type
to a physical column type (`double precision` as an integer bit pattern, `numeric` and `jsonb` as text,
`date` and `timestamptz` as integers, `uuid` as bytes) and the typed-row codec uses the column order. A
stored row cannot be decoded without the definition that wrote it.

This RFC proposes a record that makes the definition part of the log, committed in the same change set
as the catalog entry that creates the table, guarded so that readers that do not understand it refuse
the directory. It implements nothing. It records the options considered, the one it recommends, and
what only the owner can decide.

Constraints that bound the design:

- Format changes need an RFC, an ADR and an independent review, and the owner decides acceptance
  (`AGENTS.md`, owner-gated list). The accepted formats are untouched: WAL v1, `LATCAP02` (RFC 0008),
  LCAT v1, LROW v1 and LCIX v1 (RFC 0005).
- Old readers must refuse, not skip. RFC 0008 / ADR 0022 is the mechanism; an operational-only upgrade
  was rejected by the owner.
- Transactional DDL (issue 382) and online schema change (RFC 0004 / ADR 0018) build on this record.
  The design must not make either impossible, and does not claim to implement them.
- The checkpoint format is undecided (RFC 0002). Nothing here selects it.

## What exists today (checked in the repository on 2026-10-30)

- `Catalog` (`lattice-engines/src/catalog.rs`) keeps `versions: Vec<(lsn, Tables)>` (a map from id to
  name), the set of every id ever created, and the managed index history. `CREATE TABLE` in the executor
  already commits through `RowStore::commit_with_catalog`, which writes an LCIX v1 change set with
  `MANAGED_COMPOUND` (bit 0). A directory created by the executor therefore already refuses readers that
  predate LCIX.
- `KNOWN_CAPABILITIES` is `MANAGED_COMPOUND` only. Bit 1 (value 2) is unknown: a unit test
  (`admission_tests.rs`, an append requiring mask 2) pins its refusal; it is not a corpus golden. The unaccepted typed-catalog proposal (RFC 0001, PR 291) proposes bit 2 (value 4),
  LCAT version 2 and LCIX version 2.
- Executor rows are typed-row-codec bytes with no writer-schema version in the value. Table ids are never
  reused; the executor allocates `max + 1` and retries on `IdUsed`.
- `RowStore::commit` does not consult the catalog: a row write to any table id is accepted.
- `lattice-engines` may depend on `lattice-abi`, `lattice-seams`, `lattice-core` and `lattice-lsl` only
  (`layers.toml`). `lattice-exec` is one layer above and owns `SqlType` (through `lattice-sql`) and
  `Datum`. The engine cannot name either.
- Defaults are constants only (`const_default`); there are no sequences, expressions or `ALTER TABLE`.
  `numeric(p, s)` is refused. At most 15 `UNIQUE` constraints per table, and table ids up to 2^26 - 1 so
  marker ids stay clear of ordinary ids.

## Options

### Option A (recommended): a definition block in a new compound envelope version, owned by the catalog

Add one new record kind, the SQL table definition, carried in a new version of the compound envelope
(LCIX version 3) next to the table operations it belongs to, and one new required capability bit. The
catalog keeps definitions beside the table history and the executor loads them at open.

Why: the definition sits in the same atomic change set as the catalog operation (and, for transactional
DDL, the rows); the engine can validate structure at replay and refuse early; `CatalogStore` can project
it; a future checkpoint has one place to read the complete schema history from; no existing byte
changes. It does not depend on the LSL type system, which cannot express the SQL types (Option C).

### Option B: definitions as rows in a reserved system table

Store each definition as a row in a reserved physical table id, written with LROW in the same LCIX
change set as the `Create`. No new envelope; only a capability bit, a reserved id range and a value
codec. Rows are versioned by MVCC, so `AS OF` and vacuum behave like any row.

Why not: the invariant "a SQL table has exactly one definition, written when the table is created and
never changed" would live in a row validator that has to look into the catalog (or the catalog into
rows) at replay. `CatalogStore`, a metadata-only projection that skips row payloads, could not see
definitions. A raw LROW writer, a policy mistake or a vacuum could change or delete a definition row like
any other row, so every write path (`RowStore::commit`, the raw API, vacuum, policy hooks) needs a special
case for the reserved range, and a checkpoint needs the same special case. It is the smallest format but
moves the complexity to places that are harder to review. It is the fallback if the owner prefers not to
add an envelope version.

### Option C: reuse the typed catalog proposal (RFC 0001 / ADR 0015)

Persist SQL tables as LSL schemas through LCAT v2 `DefineSchema`, with the `LTYP` writer-version
envelope on every row and capability bit 2.

Why not, with evidence from the preserved branch `review/typed-catalog-format` and PR 291:

- The language cannot hold the data. `lattice_lsl::ast::Ty` has `Bool`, `Int`, `Text`, `Bytes`, `Ref`,
  `List` and `Enum`. The grammar has no `numeric`, `uuid`, `date`, `timestamptz`, `jsonb`, `double
  precision`, no distinction between `smallint`, `integer` and `bigint`, no `varchar(n)`, no primary key,
  no `UNIQUE` and no default value. Persisting SQL tables as LSL means either extending the language (a
  Phase 1 deliverable with its own grammar document, goldens and checker) or collapsing the SQL types
  onto the physical ones. The executor already does that collapse for storage (`col_type` maps every
  integer width, `double precision`, `date` and `timestamptz` to `Int`), so an LSL `int` could not tell a
  bit pattern from a count. A second record would still be needed for everything LSL lacks, which gives
  two sources of truth.
- It changes the row bytes. `LTYP` adds a 13-byte envelope (magic, version, writer schema version) to
  every value; executor rows have none today. Adoption means rewriting rows or a dual decoder.
- It is not available. RFC 0001 is `draft` and its comment period ends 2026-11-05. PR 291 (the guarded
  integration of the six original commits) is a draft; its own description lists hosted execution
  evidence and independent review as pending, and on 2026-10-30 its `risk-gate` check is failing. Making
  issue 379 wait for a larger format aimed at LSL entities would block SQL durability on work it does not
  need.
- The shared-writer, validate-on-replay, id-reservation and writer-fencing ideas in that proposal are
  sound and are reused here as ideas (see "What this reuses"), not as bytes.

The proposals can coexist. The definition block below is a list of tagged entries, so a later decision to
store LSL typed schemas through the same block is possible without another envelope. This RFC does not
accept, reject or renumber RFC 0001.

A stopgap considered and rejected as the end state: persist only a digest of each definition in the
catalog and keep requiring the host to supply the columns. It removes the weaker-definition hazard
(item 2) and nothing else; recovery still depends on out-of-band state, which the issue says a real
database must not do. A digest is kept only in memory, as a comparison key.

## Proposal (Option A)

All integers are little endian. All lengths are bounded and checked before allocation. Decoders are
strict: unknown versions, kinds and tags, nonzero reserved bits, trailing bytes and non-canonical
spellings are errors, and a successful decode re-encodes to exactly the input.

### Capability

Assign bit 3 (value 8), `SQL_DEFINITIONS`. Bit 1 (value 2) stays unknown and bit 2 (value 4) stays
reserved for RFC 0001. `KNOWN_CAPABILITIES` becomes `1 | 8` in the implementation. The writer requests
the bit on the append that carries the first definition, and the cumulative mask is preserved across
rollover as RFC 0008 specifies; empty-tail promotion, the staged guard file, header preflight and the
capability-rollback checks are used unchanged. A directory with a segment that has bit 3 refuses, before
any callback or repair, in every reader that does not list bit 3 as supported. That includes every
binary built from current main.

This relies on the RFC 0008 empty-tail promotion as amended for issue 307 (a staged `.guard` file so a
crash cannot lose already durable bits). That amendment is still awaiting owner acceptance; this
proposal's guard installation depends on it, so its acceptance should precede or accompany the format
decision here.

Support is declared only by an engine that really validates:

| Consumer | Advertises bit 3 | Behaviour |
|---|---|---|
| Default `RowStore` | yes, only with the definition decoder and replay validators | recovers definitions |
| `CatalogStore` (metadata projection) | yes | recovers definitions, runs no rows |
| Callback mode (`open_with_indexes` with a nonempty list) | no | refuses guarded history before any callback |
| Core-only opens | no | refuse |

### Envelope: LCIX version 3

Version 3 is the version 1 layout with one more block. Version 2 is not used: it is the number RFC 0001
proposes for its own nesting, and this RFC does not take it.

```
"LCIX"                       4 bytes
version                      u8 = 3
table operations             u32 length + canonical LCAT v1 bytes (0 = none)         as v1
index operations             u32 count (<= 4096) + operations                         as v1
definition operations        u32 count (1..=1024) + operations                        new
row mutations                u32 length + canonical LROW v1 bytes (0 = none)         as v1
```

Definition operations are in ascending table id with no duplicate id. Version 3 is canonical only when the
definition count is at least one. The version 1 rule "at least one table or index operation" becomes
"at least one table, index or definition operation" for the shared encoder; a version 3 payload may
carry definitions for existing tables with no table operation (adoption). A change without definitions is
encoded as version 1, byte for byte as today, so a directory that never defines a SQL table never needs
the new capability and every accepted fixture is unchanged. Version 3 requires capabilities 1 and 8
together (mask 9) in its actual containing segment; a version 3 payload in a segment without both bits
refuses, as an LCIX payload in an unguarded segment already does.

Definition operation:

```
tag u8 = 1                   DefineTable
table id u32                 live after this change's table operations; <= 2^26 - 1
body length u32, then body   the SQL table definition below (<= 65,536 bytes)
```

Other tags refuse. The tag space is left for later kinds (views, sequences, a typed LSL schema).

### SQL table definition, version 1

```
definition version u8 = 1
layout rule u8 = 1           typed-row codec v1 with the type mapping below, columns in this order
column count u16             1..=1024
per column:
  name                       u32 length (1..=255) + UTF-8, no byte in 0x41..=0x5a
  type tag u8                see below
  varchar limit u32          0 = none; nonzero only for tag 8; counted in characters (Unicode scalar values)
  flags u8                   bit 0 NOT NULL, bit 1 has default, bits 2..7 must be zero
  default                    only when bit 1: value tag u8 + payload, checked against the type tag
primary key                  u16 count (0..=column count) + that many u16 column positions, no repeats
primary key name             only when count > 0: u32 length (1..=255) + UTF-8
unique count u8              0..=15
per unique constraint k:     u16 count (1..=column count) + u16 positions, no repeats; u32 length (1..=255) + UTF-8 name
```

Type tags: 1 boolean, 2 smallint, 3 integer, 4 bigint, 5 double precision, 6 numeric (no modifiers),
7 text, 8 varchar, 9 bytea, 10 uuid, 11 date, 12 timestamptz, 13 jsonb. Anything else refuses. A type
that needs a modifier (`numeric(p, s)`) or a new type gets a new definition version, not a new meaning
for these tags.

Default tags and payloads: 1 boolean (u8 0 or 1); 2 integer (i64, within the column's range); 3 double
(u64 bit pattern, NaN canonicalised to `0x7ff8000000000000`); 4 numeric (u32 length + canonical decimal
text: optional `-`, digits with no leading zero except a single `0`, optionally `.` and digits with no
trailing zero, and no `-0`; any other spelling refuses, so the encoding is canonical); 5 text or varchar
(UTF-8, u32 length in bytes; the character count must be within the varchar limit); 6 bytea (u32 length); 7 uuid (16
bytes, big endian); 8 date (i32 days since 1970-01-01); 9 timestamptz (i64 microseconds since
1970-01-01 00:00:00 UTC); 10 jsonb (UTF-8 text as written, which is not canonical: the executor compares jsonb textually, and so does
the identity test in Migration, so two spellings of the same JSON are two definitions). These are the units of the executor's
`Datum`. Defaults are constants; `now()` and expressions are not representable and need a new
definition version.

Structural rules that the engine validates, before append and again on replay, without SQL knowledge (the
engine does not depend on `lattice-sql` or `lattice-exec`):

- column names nonempty and distinct; "lower case" means ASCII only: no byte `A`-`Z`, and no Unicode
  folding is performed or implied (other UTF-8 is kept as written). Constraint names are nonempty,
  at most 255 bytes, and distinct, including the primary key name; positions in range; no repeated position in one
  key; every primary key column has `NOT NULL`; constraint names distinct;
- key columns are not numeric (6) or jsonb (13), matching the executor's `key_type_ok`;
- default tag matches the type tag and is in range; reserved flag bits are zero; the `varchar` limit rule;
- the table id leaves the marker space clear: `id <= 2^26 - 1` and at most 15 uniques.

The executor re-checks the semantics it owns when it turns a recovered definition into a `TableDef`
(for example that a numeric default parses and a jsonb default is valid JSON). A failure refuses
`Database::open` with a corruption error; it is never skipped.

The definition does not contain the table name. The name comes from the catalog operation, so a rename
(`CatalogOp::Rename`) needs no new definition. Constraint names are stored so that a rename or a change
of the executor's naming rule cannot change what error messages report. The marker table of unique
constraint `k` is `0x8000_0000 | id << 4 | k`, with `k` the position in this record; the format freezes
that mapping.

### Catalog semantics

`Catalog` gains a third history beside tables and managed indexes: a map from table id to (defining LSN,
canonical bytes, dropped LSN or none). Replay rules, enforced in `check_change` with the same
all-or-nothing behaviour as indexes (nothing is published unless every step of the record is valid):

1. A table has at most one definition for the whole life of its id. The definition is immutable.
2. `DefineTable` needs the table live after the same change's table operations. It may be in the same
   change as the `Create` (the normal case), or define an existing table that has no definition
   (adoption, see Migration). It is refused for a dropped table or one already defined.
3. `Drop` of a defined table marks the definition dropped at that LSN. The definition is kept so reads
   `AS OF` an earlier snapshot can still decode rows. It can be pruned with the table versions at the
   vacuum floor, as `vacuum_index_versions` already does for index versions; the id stays used.
4. `Rename` leaves the definition untouched.
5. A row write to a table that has a definition is checked against the catalog at commit: the table must
   be live in the post-change catalog of the same change set. Marker tables are included: a write to a
   physical id with bit 31 set is mapped back to its owner (`(id & 0x7fff_ffff) >> 4`) and its
   constraint index `k` (`id & 0xf`). If the owner has a definition, the owner must be live and `k` must
   be below its unique count; a marker write with `k` out of range, or to a dropped owner, is refused.
   A marker id whose owner has no definition stays a raw write. This is a new engine check (`RowStore::commit`
   does not consult the catalog today). It is the primitive the executor needs so a transaction that
   bound a table cannot commit rows into a table dropped in the meantime (retryable, SQLSTATE `40001`).
   Undefined and raw tables keep the existing byte API.

Because a definition is immutable per id, no per-version dispatch is needed to read old rows: all rows of
a table were written under its one definition. Any later schema change is a new physical table or
generation, which is what RFC 0004 already proposes (a hidden target generation with new ids). If a
future design wants in-place layout versions it adds a definition version and a row envelope with its own
review. Consequence: with immutable definitions, a future `ADD COLUMN` is a rewrite into a new
generation with a new table id, and its unique-constraint marker ids change with the id (they derive from
it); `ALTER TABLE` is not available in the first slice and this is the price of the rule. The catalog
holds one shared copy of the canonical bytes per id, so no earlier definition is
copied per change (the tables history is cloned per change today, issue 234).

### Commit and recovery

`RowStore::commit_with_catalog` gains definitions (as an argument or a sibling method; a detail for the
work package). It asks the `authorize_ddl` hook, with `DdlRef` extended to list the definitions, then
checks the snapshot floor, conflicts and the catalog rules above, encodes LCIX v3 when definitions are
present, and commits with mask 9. Table creation, its definition and any row writes of the same
transaction publish at one LSN or not at all (I2, I3).

Recovery is the existing streaming fold. A version 3 payload is decoded, validated against the recovered
catalog by borrow, and applied only when the whole record succeeds. A CRC-valid payload that claims LCIX
version 3 and fails parsing or validation refuses open; it is not skipped as unknown data. Version 3
outside a segment with mask 9 refuses. Unacknowledged but complete durable change sets may be adopted,
exactly as for LCIX v1. Retried recovery reproduces the same catalog.

Recovery cannot tell a SQL table that lost its definition from a table deliberately created raw; both are
simply undefined. Only a scrubber check against the host's expectation (for example the executor
listing tables it believes it created) can notice, and the format carries no "SQL table" flag in version 1.
A flag on `Create` would close this at the price of another catalog change; it is left out and noted.

Tables with no definition (raw tables created through the library API, and tables created by a build
before this change) are listed as undefined. The SQL layer reports them as not addressable, as it does
today. It never guesses a definition.

### Executor and host changes (consequences, for the work packages)

- `Database::open` builds `Schema` from the recovered definitions. `attach_table` leaves normal startup.
  The in-process `fingerprints` map is deleted; the durable definition is the comparison key.
- `create_table` commits `Create` and `DefineTable` in one change set. The retry on `IdUsed` stays.
- `catalog_version` no longer needs `schema.generation()`: a definition appears or disappears only
  through a catalog commit, so the catalog's applied LSN is the whole version.
- `latticed --data-dir` keeps its "unattached" list for legacy tables only.

### Versioning and old-reader refusal

Three levels, each refusing rather than guessing:

1. Capability bit 3 in the segment header. A reader without it refuses before any callback or repair
   (RFC 0008). A frozen-reader proof, against a reader that has the guard, is part of the acceptance evidence.
2. The envelope version byte (3), the definition version byte (1) and the layout rule byte (1). A reader
   that knows bit 3 but not a later definition version refuses, because the decoder is strict.
3. A later incompatible definition version should sit behind its own capability bit (for example bit 4)
   so that refusal happens at header preflight and not in the middle of replay. This RFC adopts that
   practice and does not allocate the bit.

A directory written with bit 3 cannot be opened by an older build, and there is no downgrade converter.
Rollback means restoring the verified backup taken before the first definition was written. As in
RFC 0005, mixed-version access is unsupported. A directory created by the executor already carries bit 0,
so the new bit narrows the set of builds that can open it rather than introducing a first refusal.

### Upgrade policy

- New empty directory: the first `CREATE TABLE` installs the guard through the normal empty-tail
  promotion. No operator action.
- Existing directory written before this change: opening it must not silently turn it into one the
  previous build cannot open. The host passes an explicit "allow format upgrade" setting. Without it,
  `CREATE TABLE` on such a directory is refused with a message that names the setting and the backup
  requirement; reads of legacy tables are unaffected. A plain automatic upgrade is the simpler
  alternative. The owner is asked to choose.

### Migration from name-only tables

No version has been released and the `--data-dir` path is on an unmerged PR; data from the review period
is expected to be disposable. A migration is still specified so the question is not reopened:

1. Legacy tables stay undefined and not addressable, as today. Nothing is guessed.
2. An explicit `Database::adopt_table` (replacing `attach_table`) takes a candidate definition and:
   a. refuses unless the id and name are live in the catalog and the table has no durable definition;
   b. fixes the catalog LSN `P` and holds the single writer (`&mut Database`; the directory lock excludes
      other processes) from the start of the verification until the commit, so no change set can land in
      between. The commit also takes `P` as a precondition and the engine refuses `DefineTable` for an
      existing table if the applied LSN moved or the table or any of its marker tables changed since
      `P`. A host that cannot pause writers for the pass runs it before serving clients;
   c. runs the verification over every retained version of the table and of its marker tables, in both
      directions and per version at the same LSN: each row version decodes under the candidate layout;
      its primary key equals the key derived from the row; `NOT NULL` and `varchar` limits hold; for
      every unique constraint `k` whose columns are all non-null in that version, the marker table has an
      entry at that LSN equal to the derived entry (a missing entry would silently permit duplicates), and
      a tombstone at the same LSN where the row was deleted or the key changed; conversely, every marker
      entry version has a row version at that LSN that derives it, so no marker entry is orphaned and
      none exists for a null-containing key. A marker table with an index outside `0..unique count` that
      holds any entry fails the check. The pass is cancellable and reports the first violation;
   d. only then commits `DefineTable` for the existing id in one change set, under the upgrade policy
      and the DDL grant.
   Memory: the algorithm needs one row and one entry at a time plus point lookups, but the scan API today
   (`scan_at`) materialises borrowed row references for the whole table, so peak memory grows with the
   table until a streaming scan exists. The work package must measure it; no bound is claimed.
3. Verification cannot prove intent. A different but compatible definition (for example a weaker
   constraint over rows that happen to satisfy it) is accepted once and is then immutable. This is stated
   rather than hidden. Adoption is retroactive for reads at earlier snapshots of the same table, which is
   sound only because step c covers every retained version.
4. Once a definition exists, `adopt_table` with a different one is refused with `42P17`; with an
   identical decoded definition (numeric defaults by value, jsonb by text, as above) it is a no-op.

If the owner declares existing directories disposable, step 2 can be left out of the first slice; the
rest of the design does not depend on it.

### Schema change atomicity and transactional DDL (issue 382)

This RFC supplies the atomic unit and the conflict primitive. It does not implement DDL in transactions.

- One change set carries table operations, definitions, index operations and rows (`CompoundChange`).
  `CREATE TABLE t (...); INSERT INTO t ...` in one transaction is one change set at one LSN. Rule 5 is
  evaluated against the post-change catalog, so an insert into a table created in the same change is valid.
- A definition becomes visible with the catalog, never earlier. A reader at snapshot `S` sees a table iff
  the catalog at `S` lists it; the definition is time-independent for that id. A plan bound before a DDL
  commit is refused by the existing catalog-version check.
- A writer whose table was dropped meanwhile fails at commit (rule 5, retryable). Two transactions that
  create the same name collide in `check_change` (`NameTaken` or `IdUsed`) and the loser retries. A
  transaction that creates a table should pick its id at commit, not at statement time, so a retry does
  not replay statements; that belongs to issue 382.
- Visibility of a transaction's own uncommitted DDL (binding later statements against a tentative
  catalog) is executor work for issue 382 and is not decided here.

### Interplay with RFC 0004, RFC 0011 and checkpoints

RFC 0004 builds a hidden target generation with new physical ids and cuts over atomically. Each
generation's table has its own immutable definition, written when the generation is prepared (an
ordinary `DefineTable` for a hidden table); cutover is a rename and drop in one change set. No in-place
redefinition is needed.

RFC 0011 holds merge kinds beside a layout. A column carrying a merge kind needs a definition flag or a
version bump. The reserved flag bits are zero and refuse in version 1, so the addition is a new reviewed
version, not a silent reinterpretation.

No checkpoint format is accepted and none is chosen here. A checkpoint that replaces a log prefix must
carry at least: the table history, every id ever created, every definition with its defining and dropped
LSN, the managed index history, and the cumulative required-capability mask (RFC 0008). One that drops
definitions would let recovery see tables without definitions, so the engine must refuse to write a
checkpoint that does not capture them. The unmerged checkpoint branches (`feat/engine-checkpoint-recovery`
and the combined typed checkpoint branch) predate this record and need the same extension. This is a
requirement on RFC 0002, which stays owner-gated.

### What this reuses from the typed catalog work

Ideas, not bytes: one writer for catalog and rows; validation both before append and during replay;
never-reused ids reserved from the whole history; refusal of a CRC-valid but invalid known payload;
fencing a writing transaction when the catalog changed; an independent golden generator and a
frozen-reader hosted job (`tools/typed-admission`). It does not reuse LCAT v2, LCIX v2, bit 2, `LTYP`,
or LSL as the definition language.

## Security and resource limits

**Who may read definitions.** A definition holds table and column names, types, constraint names and
constant defaults. The SQL layer already answers names to any actor that may connect (`describe` binds
against the catalog; the executor README states this and defers hiding to Phase 4). This RFC keeps that
rule: definitions are metadata readable through the host-only `Catalog` API and through SQL binding, with
no new per-actor read check. `CatalogStore` and the raw scrub and read tools have no policy hook at all,
as for names today, so anyone who can run them or read the directory sees every definition. The
`authorize_ddl` hook, extended with the definitions, governs writes only. A catalog-read authorization
rule is a Phase 4 policy decision, not made here.

**Cleartext.** Definitions, including constant defaults, are stored unencrypted in the WAL, in every
segment copy and in backups, like row values. A default must not carry a secret. The documentation and the
executor's error messages must say so; the executor never echoes a default value in an error.

**Catalog-wide caps.** The per-definition bounds (1,024 columns, 65,536 bytes, 1,024 definition operations
per payload) do not bound the catalog. Proposed, for the owner to confirm: at most 16,384 live defined
tables and 64 MiB of retained definition bytes in total. Dropped definitions count until the vacuum floor
passes their drop LSN, since they are needed for `AS OF` reads. A commit that would exceed either cap is
refused with a resource error before append. Replay does not refuse a history a writer accepted, so a
later lower cap affects new commits only; memory stays bounded by the caps in force when the history was
written.

**What the fuzz targets bound.** Decoding allocates at most the input length (no `with_capacity` from an
unvalidated count; a count larger than the remaining bytes divided by the smallest possible element is
refused first), never panics, and re-encodes a successful decode to the input. The replay target also
asserts that the catalog's retained definition bytes never exceed the sum of accepted payload sizes.

## Verification plan

### Fixtures

An independent generator (`formats/generate_sql_definition_golden.py`, standard library only, its own
bitwise CRC, not the Rust encoder) emits, with `.golden.toml` metadata marked proposed until the owner
decides: a minimal definition; every type tag; every default tag; a composite primary key; fifteen
uniques; an LCIX v3 payload nesting a real LCAT v1 `Create` and a definition; the same with LROW rows;
header fixtures for masks 8 and 9; and a rejection example per rule (unknown version, tag or flag bit;
repeated column or key position; key on numeric; default mismatch; id above the limit; 16 uniques;
definition for a dropped table; second definition for an id; version 3 with zero definitions; version 3
without mask 9; trailing byte; every strict prefix of the smallest valid payload). Expected bytes never
come from running the code under test.

### Property tests and fuzzing

Properties generate valid definitions (no heavy `prop_assume!`): encode/decode round trip, canonical
re-encode, every strict prefix refused, a single-bit change either refused or decoding to a value that
re-encodes to the changed bytes, and catalog replay equal to a model map `id -> definition` after
arbitrary create/define/rename/drop sequences. 10,000 cases on PRs, 1,000,000 nightly.

New fuzz targets, seeded from the corpus in `fuzz/seeds`: `sqldef_decode` (no panic, bounded
allocation, round trip), `catalog_compound_v3_decode`, and `catalog_replay_defs` (arbitrary operation
sequences against `check_change`). They are added to the fuzz matrix check.

### Planted bugs

One per new invariant, so the simulator and the tests can fail:

| Planted bug | Invariant it defeats | Caught by |
|---|---|---|
| definitions not recovered (catalog kept, definitions dropped at replay) | durable definition | reopen oracle and a definitions-vs-catalog scrubber check |
| capability bit 3 omitted on the append | old-reader refusal | independent capability and adoption oracle, as `RequiredGuardOmitted` |
| `Create` and `DefineTable` committed as two change sets | atomic create | power-cut sweep: table without definition after a cut |
| drop leaves the definition live, or an id is reused after a drop | ids never reused; `AS OF` decoding | model map comparison and scrubber |
| row write to a dropped defined table accepted | commit-time liveness | targeted test and simulator workload step |
| marker write with `k` out of range or to a dropped owner accepted | commit-time liveness for marker ids | targeted tests in WP4 and WP6 |
| adoption skips the reverse (orphan) marker check or the LSN precondition | adoption soundness | adoption tests with a missing and an orphaned marker entry and a concurrent commit |
| mismatched definition adopted | no weaker re-definition (the issue 379 hazard) | adoption test with a deliberately weaker candidate |
| columns sorted at decode | layout rule 1 | row round trip against the independent fixture |

### Fault injection and compatibility

A bounded power-cut sweep in the style of RFC 0005 and RFC 0008, for: the first `CREATE TABLE` in an
empty directory (empty-tail promotion), the first definition in a legacy directory with a nonempty tail
(rollover), create plus insert in one change set, drop, rename, and adoption. At every mutating seam
operation: cut, `ENOSPC` on append, failed file and directory syncs, and torn appends at every prefix.
Each outcome records whether the guard and the definition survived, that no acknowledged table loses its
definition, that no definition survives in an unguarded segment, and a second recovery. Controls prove
each fault site is reached.

A hosted job pinned to an immutable revision that contains the RFC 0008 guard but not bit 3 (the pattern of
`typed-admission.yml`; not `743b0fb`, the pre-guard reader, which refuses a guarded header by `BadMagic`
and would not test capability refusal) builds that real reader with its original lockfile, runs it on every exported directory and checks it refuses
guarded directories with unchanged before/after byte manifests. Evidence goes to `docs/evidence/` with the
run id; a workflow file is not evidence until a run succeeds.

### Simulator

The row simulation gains DDL steps (create with definition, drop, rename, define existing) and a model
map oracle for definitions. The simulator checks only I1, I2, I4, I6, I9 and I12 today; this work adds a
named scrubber check that every live defined table has the acknowledged definition, and claims no more.

### Not run locally

Mutation testing of the new modules, 1,000,000-case properties, the full fuzz matrix, the fault sweep and
the frozen-reader job run on CI.

## Work packages

Draft issue text is in the pull request description; no issue is created before the owner decides. In
dependency order:

1. Capability bit 3 in `lattice-core::admission`, guard tests and header fixtures.
2. `lattice-engines::sqldef`: structural codec, strict decoder, properties, fuzz target, goldens, format
   document.
3. LCIX v3 encode and decode, `CompoundChange` definitions, `Catalog` definition history and rules,
   `CatalogStore` projection, planted bugs.
4. `RowStore` commit and replay with mask 9, commit-time liveness check including marker tables,
   catalog caps, upgrade-policy setting, `DdlRef` extension.
5. Fault sweep and frozen-reader hosted compatibility job with evidence.
6. `lattice-exec` (6a): open loads definitions, atomic create, remove `fingerprints`, simplify the
   catalog version. (6b): `adopt_table` with the bidirectional verification pass and LSN precondition.
7. `latticed`: data directory startup, the restart test changes from "names only" to full recovery,
   undefined-table reporting, upgrade setting.
8. Simulator workload step, definitions scrubber check, model oracle.
9. Docs, gate rows, `docs/sql-subset.md`, crate READMEs, ADR index, changelog; independent review.
10. Checkpoint requirements for RFC 0002 (blocked on the owner's checkpoint decision).

## Alternatives

Options B and C and the digest-only stopgap are described above. Also rejected: storing the CREATE TABLE
statement text and re-running the SQL parser at open. The parser and binder rules would then be part of
the on-disk format, and a parser change could alter the meaning of an old table. Serializing
`TableDef` with its derived `Debug` output (what the in-memory fingerprint uses) would make private Rust
struct layout the disk format.

## Compatibility and migration

- WAL v1, `LATCAP02`, LCAT v1, LROW v1 and LCIX v1 bytes and meanings are unchanged. A change without
  definitions is still encoded as LCIX v1.
- Older readers refuse any directory with bit 3, before replay. There is no in-place downgrade and no
  converter. Take a verified backup before the first definition is written; rollback restores it.
- The typed-catalog proposal's numbers (LCAT 2, LCIX 2, bit 2) are left alone. If both proposals are
  accepted, a change that needs both uses a later, separately reviewed envelope version; that composition
  is not designed here.
- Legacy name-only tables are neither deleted nor guessed (see Migration).
- Tests that assert "a restart recovers table names but not columns" (`exec_host.rs` and
  `latticed/tests/sql_binary.rs` in PR 412) change with the executor work package.

## Decisions for the owner

1. Accept, amend or reject the format: LCIX version 3, definition version 1, layout rule 1, capability
   bit 3 (value 8). Choose Option A, B or C. This RFC recommends A.
2. Confirm that issue 379 does not wait for RFC 0001 / PR 291, that RFC 0001's numbers stay reserved for
   it, and that this proposal takes bit 3.
3. Keep or waive the seven-day comment period (ends 2026-11-06).
4. Upgrade policy for existing directories: explicit "allow format upgrade" setting (recommended) or
   automatic.
5. Whether pre-alpha name-only directories are disposable (leave adoption out of the first slice) or
   must be migrated by verified adoption.
6. Whether the checkpoint requirements above can be added to RFC 0002's discussion now.
7. The catalog caps (16,384 live defined tables, 64 MiB retained definition bytes) and the read rule:
   definitions are readable metadata like names today, with no per-actor check before Phase 4.
8. Whether Option B stays the stated fallback if Option A is rejected.
9. Acceptance of the RFC 0008 `.guard` amendment (issue 307), which this proposal depends on.

An independent review of the final diff is required before merge and is the tech lead's call for the
`reviewed` label. It does not replace the owner's format decision.
