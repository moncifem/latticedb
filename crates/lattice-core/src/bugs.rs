//! Planted bugs for "testing the tests".
//!
//! With the `planted-bugs` feature a test can switch on a deliberate defect and then prove that
//! the simulation and invariant checkers notice it. Without the feature every check compiles to
//! `false`, so production builds contain none of this.
//!
//! # Adding a planted bug
//!
//! A new bug is two lines: its variant in the `planted_bugs!` list, and its entry in the array of the group that
//! describes how it is detected (extend that array's length, or add a new group const and list it
//! in [`Bug::GROUPS`]). There is no total to bump. The registry test in this file derives
//! everything else from [`Bug::GROUPS`]: every variant is in exactly one group, discriminants are
//! distinct and below `MASK_BITS` (the thread-local mask width). A variant without a group, or in
//! two groups, fails that test. Add the variant to the `planted_bugs!` list; that list also
//! generates [`Bug::VARIANTS`].

/// The integer type of the per-thread mask: one bit per planted bug.
#[cfg(any(test, feature = "planted-bugs"))]
type Mask = u128;
/// How many bugs the mask can hold; discriminants must stay below it.
#[cfg(test)]
const MASK_BITS: u32 = Mask::BITS;

/// Generates [`Bug`] and [`Bug::VARIANTS`] from one list, so the list of all variants cannot
/// drift from the enum. Doc comments and attributes on a variant pass through.
macro_rules! planted_bugs {
    ($($(#[$m:meta])* $name:ident,)*) => {
        /// A deliberate defect that can be switched on in test builds.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum Bug {
            $($(#[$m])* $name,)*
        }

        impl Bug {
            /// Every variant, in declaration order (generated from the enum's own list).
            pub const VARIANTS: &'static [Bug] = &[$(Bug::$name,)*];
        }
    };
}

planted_bugs! {
    /// `Log::sync` skips the fsync (durability).
    SkipFsync,
    /// Duplicate change set ids are recorded again (idempotency).
    NoDedupe,
    /// The hybrid logical clock follows the wall clock backwards (causal order).
    HlcRegress,
    /// Commits are acknowledged before they are synced (durability).
    AckBeforeSync,
    /// Recovery leaves a torn tail in place instead of truncating it (durability).
    AcceptTornTail,
    /// The record checksum does not cover the payload (corruption detection).
    CrcSkipsPayload,
    /// The fence check is skipped (fencing).
    FenceIgnored,
    /// Recovery drops the last valid record (durability).
    RecoveryDropsLastRecord,
    /// Segment rotation does not sync the directory (durability).
    RotateWithoutDirSync,
    /// The next LSN is reused after recovery (ordering).
    LsnReusedAfterRecovery,
    /// The dedupe set is not rebuilt on recovery (idempotency).
    DedupeNotRebuilt,
    /// Turn state is not rebuilt on recovery (fencing).
    FenceNotRebuilt,
    /// Creating the log directory does not make its parents durable (durability).
    NoParentDirSync,
    /// Recovery does not fsync what it adopts as committed (durability).
    SkipRecoverySync,
    /// A repeated id inside a batch is acknowledged whatever happens to its first occurrence.
    BatchDuplicateAckedEarly,
    /// A failed fsync does not poison the log, so a later successful one vouches for lost data.
    FsyncFailureNotPoisoning,
    /// A failed fsync is swallowed and the batch is acknowledged (durability).
    FsyncErrorSwallowed,
    /// A new segment's header is not synced when the log rotates (durability).
    RotateHeaderNotSynced,
    /// The durable LSN runs one ahead of what was written (ordering).
    DurableLsnOvershoot,
    /// The hybrid logical clock does not advance its counter within one millisecond (causal order).
    HlcCounterStuck,
    /// A checkpoint is written while change sets are logged but not applied (recovery then rejects the log).
    CheckpointIgnoresInFlight,
    /// The row store commits without checking for a newer write to the same row (lost update).
    RowConflictCheckSkipped,
    /// The row store applies only the first write of a change set in memory (partial visibility).
    RowPartialApply,
    /// Row recovery drops deletions, so deleted rows come back after a restart (durability).
    RowRecoveryDropsDeletes,
    /// An index keeps the entry for a row's previous value after an update or deletion.
    RowIndexStaleEntries,
    /// Updating a row discards its older versions (immutable snapshots).
    RowHistoryOverwritten,
    /// Visibility searches skip deletion versions (deleted rows resurrect).
    RowTombstoneSkipped,
    /// Transaction reads ignore their own buffered writes and deletes.
    RowReadOwnWriteIgnored,
    /// A row scan treats its exclusive upper bound as inclusive.
    RowScanUpperBoundIncluded,
    /// Index maintenance omits newly indexed entries (missing rows).
    RowIndexMissingInsert,
    /// An index equality lookup matches keys with the requested prefix.
    RowIndexPrefixLookup,
    /// Vacuum also drops the newest version at the horizon, so a snapshot there reads nothing.
    VacuumDropsVisibleVersion,
    /// Query row reads skip the actor policy (authorization).
    RowPolicyReadSkipped,
    /// A row transaction appends without authorizing all final transitions.
    RowPolicyWriteSkipped,
    /// The tree CRDT applies a move without checking that it would create a cycle (convergence).
    TreeCycleCheckSkipped,
    /// A capability-requiring append omits its durable segment guard.
    RequiredGuardOmitted,
    /// An append that requires the SQL definitions capability drops that bit from its durable
    /// segment guard but keeps the others (old readers would replay definitions they cannot read).
    SqlDefinitionsBitDropped,
    /// Recovery ignores managed index definitions and therefore omits their rebuilt entries.
    ManagedIndexesNotRecovered,
    /// Strict HLC history resolution silently substitutes the vacuum floor for an expired view.
    HlcSnapshotClamped,
    /// The duplicate check runs before the fence check, so a superseded turn retrying an
    /// already stored record id is answered duplicate instead of fenced (plugin contract: fence first).
    DuplicateBeforeFence,
    /// An empty tail's guard is rewritten in place without a durable staged copy (issue 307).
    GuardRewriteUnstaged,
    /// A catalog or managed-index change commits without asking the actor policy (authorization).
    DdlPolicySkipped,
    /// The sync subscription host reads events for a fixed development identity instead of the
    /// principal its connection authenticated as (authorization).
    SubscribePrincipalDropped,
    /// A peer's operation id is accepted whatever node it carries (forged node id).
    CrdtNodeBindingSkipped,
    /// A peer's operation id is accepted however far ahead of the clock it is (clock drift).
    CrdtDriftNotChecked,
    /// A peer's batch is accepted whatever its length (operation budget).
    CrdtBatchLimitSkipped,
    /// The text CRDT does not count inserts per quota key (lifetime budget exhaustion).
    CrdtKeyBudgetSkipped,
    /// The tree CRDT does not cap the number of nodes (memory exhaustion).
    TreeNodeLimitSkipped,
    /// The tree CRDT does not cap node depth (walk cost).
    TreeDepthLimitSkipped,
    /// The tree CRDT does not share the op log between origins (log exhaustion).
    TreeOriginQuotaSkipped,
    /// The set CRDT accepts a merge of any size (memory exhaustion).
    SetTagLimitSkipped,
    /// Recovery truncates a checksum-valid record with an unexpected LSN as if it were a torn tail
    /// (silent loss of acknowledged data after a gap or repeat).
    GapTailTruncated,
    /// The executor's primary-key range scan treats an inclusive upper bound (`<=`, `BETWEEN`) as
    /// exclusive (missing boundary rows).
    ExecIndexUpperBoundOpen,
    /// The executor's unique-constraint lookup fetches the row without the actor's read policy
    /// (hidden rows revealed).
    ExecIndexPolicySkipped,
    /// The executor's aggregates feed `NULL` arguments to the fold (`count(x)` counts them, `sum`,
    /// `avg`, `min` and `max` see them), so results differ from PostgreSQL.
    AggregateSkipsNull,
    /// The executor's `GROUP BY` groups by the first key only, merging groups that differ in a later
    /// key (wrong aggregates).
    GroupKeyTruncated,
    /// A row commit accepts a key longer than the change feed can carry (feed availability).
    RowOversizeKeyAccepted,
    /// The tree CRDT replays any number of live ops for one peer operation (CPU exhaustion).
    TreeReplayBudgetSkipped,
    /// Recovery keeps a zero-filled torn tail in place instead of truncating it, so later
    /// acknowledged records land behind a damaged region (silent loss once that region is repaired).
    ZeroFilledTailKept,
    /// The kernel-backed event source reads the feed with an allow-all policy instead of the
    /// principal's, so a denied principal sees rows and tables it may not (authorization).
    FeedSourcePolicyDropped,
    /// A table grant that only reads also authorizes writes, so a read-only account changes data
    /// (authorization).
    GrantReadOnlyTreatedAsWrite,
    /// A table grant is matched by name prefix instead of exactly, so a grant on `notes` also
    /// covers `notes_private` (authorization).
    GrantTableMatchByPrefix,
    /// The executor takes the author of a commit from client-controlled text (the statement)
    /// instead of the authenticated principal (forged provenance).
    AuthorFromClient,
    /// The account registry derives an author id from a hash truncated to a few bits, so two
    /// accounts can share one id (provenance and CRDT node binding).
    AuthorIdTruncatedHash,
    /// Startup does not check the author registry against the author ids in the retained log, so a
    /// missing or older registry restarts its counter at ids that history already carries.
    AuthorRegistryLogCheckSkipped,
    /// Only the first bytes of a bearer token's secret reach its verifier, so a token that shares
    /// that prefix authenticates (authentication).
    TokenAcceptsPrefix,
    /// `CredentialStore::revoke_account` revokes nothing, so removing an account with the
    /// credential tool leaves its bearer tokens working (authentication).
    AccountRemoveKeepsTokens,
    /// The credential tool's create-new write replaces an existing file instead of refusing, so
    /// `account init` run twice destroys the decoy secret (credential loss).
    AccountFileOverwritten,
    /// The credential tool accepts a password shorter than its minimum (weak credential).
    WeakPasswordAccepted,
    /// The credential tool does not take its directory lock, so a token creation racing a
    /// revocation can write back the older file and undo the revocation (lost update).
    AccountNoLock,
    /// The engine's feed read ignores the caller's event limit and returns everything after the
    /// cursor (unbounded memory and executor time per read).
    FeedLimitIgnored,
    /// The engine's feed read ignores its bound on versions scanned, so one read over history the
    /// reader may not see runs to the end (unbounded executor time per read).
    FeedScanBudgetIgnored,
    /// The row store leaves a change set out of its commit index, so the feed misses its events
    /// (silent loss for every subscriber).
    FeedIndexCommitSkipped,
    /// A waiter on the commit notifier does not look at the published LSN before it first sleeps,
    /// so a commit that landed before it arrived is waited for in vain (lost wake).
    NotifyWaitIgnoresPublished,
    /// The executor host publishes the next LSN to the commit notifier before the statement that
    /// commits it has run, so a woken subscriber can find nothing to read (wake before durable).
    NotifyBeforeDurable,
    /// The SQL definition decoder sorts columns by name, so the row layout (declaration order,
    /// layout rule 1) and the key positions no longer match what was written.
    SqlDefColumnsSorted,
    /// The gateway takes the first of several `Authorization` headers instead of refusing the
    /// request, so a proxy and the server can disagree about which credential was presented
    /// (authentication).
    GatewayDuplicateAuthorizationAccepted,
    /// The gateway accepts an upgrade request whose target carries a query string, where a client
    /// would put a token that then lands in access logs and referrers (credential exposure).
    GatewayQueryStringAccepted,
    /// A refused bearer token is not counted against the client address, so guessing is never
    /// rate limited (authentication).
    GatewayAuthFailureNotCounted,
    /// The gateway admits a connection past its configured cap (resource exhaustion).
    GatewayConnectionCapIgnored,
    /// A live gateway connection is never checked again against the current credential store, so a
    /// revoked token keeps its stream (authentication).
    GatewayRevalidationSkipped,
    /// The bearer verifier accepts a token whose line says `revoked`, so a revocation written to the
    /// credential file never takes effect, neither for new upgrades nor for live connections
    /// (authentication; issue #577).
    RevokedTokenStillAccepted,
    /// The MCP tool backend runs a call that carries a caller other than the one derived from its
    /// bound principal, instead of refusing it (confused deputy: a client string names the
    /// identity).
    McpCallerFromArgument,
    /// The MCP write drops its `expected_version` check, so a stale writer overwrites a newer row
    /// (lost update).
    McpWriteIgnoresExpectedVersion,
    /// The MCP backend answers a table the principal may not see differently from a table that does
    /// not exist (existence oracle).
    McpDeniedTableDistinguishable,
    /// The MCP backend writes stored row text outside the fixed untrusted envelope, so an agent
    /// cannot tell data from the server's own words (prompt injection, N-11).
    McpRowTextOutsideEnvelope,
    /// The MCP `lattice_subscribe` answers from a cursor one step back, so the event the client
    /// already has comes again on every call (duplicate delivery; a resumed reader sees a repeat).
    McpSubscribeInclusiveCursor,
    /// The MCP HTTP endpoint serves a request whose `Host` is not its own address, so a page on another
    /// name that resolves to loopback can call it (DNS rebinding, issue #580).
    McpHostCheckSkipped,
    /// The MCP HTTP endpoint serves a request that carries an `Origin` header, so a browser page on
    /// another origin can call it (issue #580).
    McpOriginCheckSkipped,
    /// The MCP HTTP endpoint reads a body whose declared length is over its cap before refusing it,
    /// so an oversize request is buffered (issue #527, declared length first).
    McpBodyCheckedAfterRead,
    /// The MCP HTTP endpoint runs a tool call as the identity the request body names, instead of the
    /// one bound to the verified token (confused deputy). `McpBackend` still refuses a call whose caller
    /// is not the one derived from its principal, so this defect alone does not widen access; its
    /// witness in `latticed::ws::mcp_http` is what detects it.
    McpPrincipalFromBody,
    /// Recovery decodes the definition blocks of the log but does not keep them in the catalog,
    /// so a table created with a definition comes back without one.
    DefinitionDroppedOnReplay,
    /// A commit that carries definitions requests only the managed-compound capability, so its
    /// segment guard lacks bit 3 and an older reader would replay definitions it cannot read.
    GuardBitNotRequested,
    /// The catalog accepts a second definition for a table and keeps the newer one, although a
    /// definition is written once at creation and never changes.
    SecondDefinitionReplaces,
    /// The catalog accepts a definition for a table that is not live after the same change.
    DefinitionForMissingTableAccepted,
    /// Dropping a table leaves its definition marked live in the current catalog.
    DefinitionDropIgnored,
    /// A definition for a table the same change does not create is accepted (adoption of an
    /// existing table, which is not implemented and would bypass the create-time DDL grant).
    DefinitionOfExistingTableAccepted,
    /// The first definition in a directory written before bit 3 existed is accepted without the
    /// explicit format-upgrade setting.
    FormatUpgradeUnchecked,
    /// `CREATE TABLE` commits the table without its definition, so the executor knows the columns
    /// only until the process restarts (the pre-RFC 0012 behaviour).
    ExecDefinitionNotPersisted,
}

impl Bug {
    /// Fence-order defect, detected by the fencing race simulation (`lattice-testkit`), not by the
    /// general log simulation, which does not retry stored writes of superseded turns.
    pub const FENCE_ORDER: [Bug; 1] = [Bug::DuplicateBeforeFence];

    /// Defects in the row store, detected by the row simulation rather than the log simulation.
    pub const ROW: [Bug; 4] = [
        Bug::RowConflictCheckSkipped,
        Bug::RowPartialApply,
        Bug::RowRecoveryDropsDeletes,
        Bug::RowIndexStaleEntries,
    ];

    /// Storage defects exercised by the deterministic public-operation witness suite.
    /// `ROW` remains the four-defect contract of the seeded row simulation workloads.
    pub const STORAGE: [Bug; 10] = [
        Bug::RowConflictCheckSkipped,
        Bug::RowPartialApply,
        Bug::RowRecoveryDropsDeletes,
        Bug::RowIndexStaleEntries,
        Bug::RowHistoryOverwritten,
        Bug::RowTombstoneSkipped,
        Bug::RowReadOwnWriteIgnored,
        Bug::RowScanUpperBoundIncluded,
        Bug::RowIndexMissingInsert,
        Bug::RowIndexPrefixLookup,
    ];

    /// Vacuum defects, detected by the vacuum workload in the row simulation.
    pub const VACUUM: [Bug; 1] = [Bug::VacuumDropsVisibleVersion];

    /// Authorization defects, detected by the independent public row-policy witness suite.
    pub const POLICY: [Bug; 2] = [Bug::RowPolicyReadSkipped, Bug::RowPolicyWriteSkipped];
    /// Catalog-managed definition recovery defects, exercised by the focused fault witness.
    pub const CATALOG_INDEX: [Bug; 2] = [Bug::ManagedIndexesNotRecovered, Bug::DdlPolicySkipped];

    /// Key-limit defects, detected by the oversize key witnesses in `lattice-engines`.
    pub const KEY_LIMIT: [Bug; 1] = [Bug::RowOversizeKeyAccepted];

    /// SQL definition codec defects, detected by the layout witness in `lattice-engines::sqldef`.
    pub const SQLDEF: [Bug; 1] = [Bug::SqlDefColumnsSorted];

    /// Durable SQL definition defects (RFC 0012), detected by the witnesses in
    /// `lattice-engines/tests/sql_definitions.rs`.
    pub const SQL_DEFINITIONS: [Bug; 7] = [
        Bug::DefinitionOfExistingTableAccepted,
        Bug::DefinitionDroppedOnReplay,
        Bug::GuardBitNotRequested,
        Bug::SecondDefinitionReplaces,
        Bug::DefinitionForMissingTableAccepted,
        Bug::DefinitionDropIgnored,
        Bug::FormatUpgradeUnchecked,
    ];

    /// Executor defects in persisting SQL table definitions, detected by
    /// `lattice-exec/tests/definitions.rs`.
    pub const EXEC_DEFINITIONS: [Bug; 1] = [Bug::ExecDefinitionNotPersisted];

    /// Strict HLC snapshot defects, detected by the focused retention witness.
    pub const HLC_SNAPSHOT: [Bug; 1] = [Bug::HlcSnapshotClamped];

    /// Hostile-peer defects (G-08), detected by the hostile peer witnesses in `crdt_peer_tests`.
    pub const CRDT_PEER: [Bug; 9] = [
        Bug::CrdtNodeBindingSkipped,
        Bug::CrdtDriftNotChecked,
        Bug::CrdtBatchLimitSkipped,
        Bug::CrdtKeyBudgetSkipped,
        Bug::TreeNodeLimitSkipped,
        Bug::TreeDepthLimitSkipped,
        Bug::TreeOriginQuotaSkipped,
        Bug::SetTagLimitSkipped,
        Bug::TreeReplayBudgetSkipped,
    ];

    /// Index-scan defects in the SQL executor, detected by the differential index-scan witnesses.
    pub const EXEC_INDEX: [Bug; 2] = [Bug::ExecIndexUpperBoundOpen, Bug::ExecIndexPolicySkipped];

    /// Aggregate defects in the SQL executor, detected by `count` over a column with `NULL`s and by
    /// a `GROUP BY` over two keys.
    pub const EXEC_AGGREGATE: [Bug; 2] = [Bug::AggregateSkipsNull, Bug::GroupKeyTruncated];

    /// Tree CRDT defects, detected by the convergence and acyclicity checks in `crdt_tree`.
    pub const CRDT: [Bug; 1] = [Bug::TreeCycleCheckSkipped];

    /// Principal-binding defects, detected by the subscription host witness in `lattice-api`.
    pub const PRINCIPAL: [Bug; 1] = [Bug::SubscribePrincipalDropped];

    /// Feed-source authorization defect, detected by the principal witness in
    /// `lattice-api::feed_source`.
    pub const FEED_SOURCE: [Bug; 1] = [Bug::FeedSourcePolicyDropped];

    /// Per-table grant defects, detected by the grant matrix in `lattice-engines::grants` and
    /// `lattice-api`.
    pub const TABLE_GRANTS: [Bug; 2] = [
        Bug::GrantReadOnlyTreatedAsWrite,
        Bug::GrantTableMatchByPrefix,
    ];
    /// Bearer-token defect, detected by the prefix witness in `lattice-api::bearer`.
    pub const BEARER: [Bug; 2] = [Bug::TokenAcceptsPrefix, Bug::RevokedTokenStillAccepted];

    /// Credential tool defects (issue 582), caught by the tests of `lattice-api::bearer`,
    /// `latticed::files` and `lattice-cli::account`.
    pub const ACCOUNT_TOOL: [Bug; 4] = [
        Bug::AccountRemoveKeepsTokens,
        Bug::AccountFileOverwritten,
        Bug::WeakPasswordAccepted,
        Bug::AccountNoLock,
    ];
    /// Feed read-bound defects, detected by the bounded-read witnesses in `lattice-engines`
    /// (`feed_emit` tests).
    pub const FEED_BOUND: [Bug; 3] = [
        Bug::FeedLimitIgnored,
        Bug::FeedScanBudgetIgnored,
        Bug::FeedIndexCommitSkipped,
    ];
    /// Commit-notifier defects, detected by the notifier and host witnesses in `lattice-api`.
    pub const NOTIFY: [Bug; 2] = [Bug::NotifyWaitIgnoresPublished, Bug::NotifyBeforeDurable];

    /// Gateway transport defects, detected by the witnesses in `latticed::ws`.
    pub const GATEWAY: [Bug; 5] = [
        Bug::GatewayDuplicateAuthorizationAccepted,
        Bug::GatewayQueryStringAccepted,
        Bug::GatewayAuthFailureNotCounted,
        Bug::GatewayConnectionCapIgnored,
        Bug::GatewayRevalidationSkipped,
    ];

    /// MCP tool backend defects, detected by the witnesses in `latticed::mcp_backend`.
    pub const MCP: [Bug; 5] = [
        Bug::McpCallerFromArgument,
        Bug::McpWriteIgnoresExpectedVersion,
        Bug::McpDeniedTableDistinguishable,
        Bug::McpRowTextOutsideEnvelope,
        Bug::McpSubscribeInclusiveCursor,
    ];

    /// MCP HTTP endpoint defects (issues #527 and #580), detected by the witnesses in
    /// `latticed::ws::mcp_http`. The mask is thread-local, so these are decided by pure rules.
    pub const MCP_HTTP: [Bug; 4] = [
        Bug::McpHostCheckSkipped,
        Bug::McpOriginCheckSkipped,
        Bug::McpBodyCheckedAfterRead,
        Bug::McpPrincipalFromBody,
    ];

    /// Guarded-WAL admission defects, exercised by independent context witnesses.
    pub const ADMISSION: [Bug; 3] = [
        Bug::RequiredGuardOmitted,
        Bug::GuardRewriteUnstaged,
        Bug::SqlDefinitionsBitDropped,
    ];

    /// Recovery-order defect, detected by the focused LSN-gap witnesses in `log.rs`: the log
    /// simulation never writes a gap, so it cannot reach this path.
    pub const RECOVERY_ORDER: [Bug; 1] = [Bug::GapTailTruncated];

    /// Torn-tail defect that only a non-prefix power loss reaches (a zero-filled extent), detected
    /// by the `non-prefix` fault profile and the focused witnesses in `crash_shape_tests`.
    pub const NON_PREFIX: [Bug; 1] = [Bug::ZeroFilledTailKept];

    /// Log defects, for the log simulation detection suite.
    pub const ALL: [Bug; 21] = [
        Bug::SkipFsync,
        Bug::NoDedupe,
        Bug::HlcRegress,
        Bug::AckBeforeSync,
        Bug::AcceptTornTail,
        Bug::CrcSkipsPayload,
        Bug::FenceIgnored,
        Bug::RecoveryDropsLastRecord,
        Bug::RotateWithoutDirSync,
        Bug::LsnReusedAfterRecovery,
        Bug::DedupeNotRebuilt,
        Bug::FenceNotRebuilt,
        Bug::NoParentDirSync,
        Bug::SkipRecoverySync,
        Bug::BatchDuplicateAckedEarly,
        Bug::FsyncFailureNotPoisoning,
        Bug::FsyncErrorSwallowed,
        Bug::RotateHeaderNotSynced,
        Bug::DurableLsnOvershoot,
        Bug::HlcCounterStuck,
        Bug::CheckpointIgnoresInFlight,
    ];

    /// Provenance defects (issue 520): the author recorded with a commit must be the authenticated
    /// principal's injective id, detected by the author witnesses in `lattice-engines` and `lattice-api`.
    pub const AUTHOR: [Bug; 3] = [
        Bug::AuthorFromClient,
        Bug::AuthorIdTruncatedHash,
        Bug::AuthorRegistryLogCheckSkipped,
    ];

    /// Every group, the single registry of planted bugs: each variant belongs to exactly one of
    /// these. `ROW` is a subset view of `STORAGE` (the seeded row simulation's contract) and is
    /// deliberately not listed. Add a new group const here.
    pub const GROUPS: &'static [&'static [Bug]] = &[
        &Bug::ALL,
        &Bug::STORAGE,
        &Bug::VACUUM,
        &Bug::POLICY,
        &Bug::CRDT,
        &Bug::PRINCIPAL,
        &Bug::FEED_SOURCE,
        &Bug::TABLE_GRANTS,
        &Bug::BEARER,
        &Bug::ACCOUNT_TOOL,
        &Bug::FEED_BOUND,
        &Bug::NOTIFY,
        &Bug::EXEC_INDEX,
        &Bug::EXEC_AGGREGATE,
        &Bug::CRDT_PEER,
        &Bug::ADMISSION,
        &Bug::CATALOG_INDEX,
        &Bug::HLC_SNAPSHOT,
        &Bug::KEY_LIMIT,
        &Bug::SQLDEF,
        &Bug::SQL_DEFINITIONS,
        &Bug::EXEC_DEFINITIONS,
        &Bug::FENCE_ORDER,
        &Bug::RECOVERY_ORDER,
        &Bug::NON_PREFIX,
        &Bug::AUTHOR,
        &Bug::GATEWAY,
        &Bug::MCP,
        &Bug::MCP_HTTP,
    ];
}

#[cfg(feature = "planted-bugs")]
mod imp {
    use super::{Bug, Mask};
    use std::cell::Cell;

    thread_local! {
        static MASK: Cell<Mask> = const { Cell::new(0) };
    }

    fn bit(b: Bug) -> Mask {
        1 << (b as u32)
    }

    /// Switches a defect on for the current thread.
    pub fn enable(b: Bug) {
        MASK.with(|m| m.set(m.get() | bit(b)));
    }

    /// Switches every defect off for the current thread.
    pub fn clear() {
        MASK.with(|m| m.set(0));
    }

    /// Whether a defect is switched on for the current thread.
    pub fn active(b: Bug) -> bool {
        MASK.with(|m| m.get() & bit(b) != 0)
    }
}

#[cfg(not(feature = "planted-bugs"))]
mod imp {
    use super::Bug;

    /// Without the feature no defect can be enabled.
    pub fn active(_b: Bug) -> bool {
        false
    }
}

pub use imp::active;
#[cfg(feature = "planted-bugs")]
pub use imp::{clear, enable};

#[cfg(test)]
mod registry_tests {
    use super::{Bug, MASK_BITS};
    use std::collections::BTreeSet;

    /// Checks that `groups` partition `variants` and that every discriminant fits a mask of
    /// `limit` bits.
    fn check_registry(groups: &[&[Bug]], variants: &[Bug], limit: u32) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for bug in groups.iter().flat_map(|g| g.iter().copied()) {
            if !seen.insert(bug as u32) {
                return Err(format!("{bug:?} appears in more than one group"));
            }
            if (bug as u32) >= limit {
                return Err(format!("guard mask exhausted: {bug:?}"));
            }
        }
        for bug in variants {
            if !seen.contains(&(*bug as u32)) {
                return Err(format!("{bug:?} is declared but in no group"));
            }
        }
        if seen.len() != variants.len() {
            return Err(format!(
                "{} grouped bugs but {} declared variants",
                seen.len(),
                variants.len()
            ));
        }
        Ok(())
    }

    #[test]
    fn the_groups_partition_every_variant_below_the_mask_bound() {
        assert_eq!(
            check_registry(Bug::GROUPS, Bug::VARIANTS, MASK_BITS),
            Ok(())
        );
    }

    #[test]
    fn a_registry_with_a_missing_duplicate_or_overflowing_bug_is_refused() {
        let declared = Bug::VARIANTS;
        let skip = [Bug::SkipFsync];
        let fence = [Bug::DuplicateBeforeFence];

        let missing: Vec<&[Bug]> = Bug::GROUPS
            .iter()
            .copied()
            .filter(|g| *g != &fence[..])
            .collect();
        let err = check_registry(&missing, declared, MASK_BITS).unwrap_err();
        assert!(err.contains("DuplicateBeforeFence"), "{err}");

        let mut duplicated = Bug::GROUPS.to_vec();
        duplicated.push(&skip);
        let err = check_registry(&duplicated, declared, MASK_BITS).unwrap_err();
        assert!(err.contains("more than one group"), "{err}");

        let err = check_registry(Bug::GROUPS, declared, 8).unwrap_err();
        assert!(err.contains("mask exhausted"), "{err}");

        // A bug listed twice inside one group is refused as well.
        let twice = [Bug::SkipFsync, Bug::SkipFsync];
        let err = check_registry(&[&twice], declared, MASK_BITS).unwrap_err();
        assert!(err.contains("more than one group"), "{err}");
    }
}

#[cfg(all(test, feature = "planted-bugs"))]
mod tests {
    use super::*;

    fn all_bugs() -> impl Iterator<Item = Bug> {
        Bug::GROUPS.iter().flat_map(|g| g.iter().copied())
    }

    #[test]
    fn every_log_and_storage_guard_has_a_distinct_mask_bit() {
        for bug in all_bugs() {
            clear();
            enable(bug);
            assert!(active(bug));
            for other in all_bugs() {
                assert_eq!(active(other), bug == other);
            }
        }
        clear();
    }
}
