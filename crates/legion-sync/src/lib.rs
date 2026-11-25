#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Hubless peer-to-peer folder synchronization engine.
//!
//! `legion-sync` implements the pure synchronization core described in `docs/legion/files.md`
//! ("Keeping folders in sync") without any network transport, watcher daemon, or CLI/tool wiring.
//! Both the member's filesystem ([`SyncFs`]) and remote peers ([`Peer`]) sit behind traits so the
//! same engine runs against real directories on disk, future network transports, and a
//! deterministic fault-injecting multi-member simulator.
//!
//! # Guarantees
//!
//! - **Pull-only writes**: a member only mutates its own disk, pulling incremental index records
//!   by sequence number and verifying every file's BLAKE3 digest in `.legion-sync/tmp` before
//!   moving any prior copy to `.legion-sync/bin` and renaming the verified file into place.
//! - **Crash safety**: every working-tree mutation is bracketed by a durable [`PendingOp`]
//!   write-ahead intent in the index store so restarting after a crash at any step converges
//!   without losing bytes or fabricating conflicts.
//! - **No silent data loss**: remote deletions and replacements move existing copies to
//!   `.legion-sync/bin` (30-day default retention, self-describing restore metadata); concurrent
//!   different edits preserve both versions under deterministic names
//!   (`name (changed on <machine> <date>).ext`); delete-vs-change keeps the change.
//! - **No zombie files**: deletion tombstones carry causal origin dependencies and are retained
//!   until every current folder member has transitively acknowledged them in the [`CausalMatrix`],
//!   independent of bin expiry. A per-member monotonic `local_clock` ensures counters are never
//!   reused even after a tombstone is garbage-collected.
//! - **Platform honesty**: names the local filesystem cannot hold (case clashes on
//!   case-insensitive systems, Windows reserved names and forbidden characters, over-long paths,
//!   and symbolic links on Windows) are held back and reported as actionable problems rather than
//!   mangled.
//!
//! # Durable index storage choice: `redb`
//!
//! [`RedbIndexStore`] uses [`redb`] (v5.0) as the embedded index store:
//!
//! - **Cross-platform builds without a C toolchain**: pure Rust (zero C dependencies; only `libc`
//!   on Unix), compiling cleanly for static Linux `musl`, macOS, and Windows
//!   (`x86_64-pc-windows-gnu` and MSVC).
//! - **ACID crash safety**: copy-on-write B+trees with dual checksummed superblocks and synced
//!   transactions guarantee atomic commits across `RECORDS`, `BY_SEQ`, `TOMBSTONES`, `CONFLICTS`,
//!   `FOLDED_LIVE`, `BIN_ENTRIES`, `HELD_BACK`, and `META`.
//! - **500,000+ file scale**: compact versioned binary record encoding (`codec`), O(log N + K)
//!   incremental sequence scans via `BY_SEQ`, O(T) tombstone GC via `TOMBSTONES`, and single-batch
//!   commits keep full scans and reconciliations fast without rewriting the whole index on every
//!   edit.
//! - **Licence and maintenance**: dual-licensed `MIT OR Apache-2.0` (compatible with this
//!   workspace's MIT licence) and actively maintained on Rust edition 2024.

pub mod apply;
pub mod codec;
pub mod engine;
pub mod error;
pub mod fs_real;
pub mod model;
pub mod path;
pub mod reconcile;
pub mod scan;
pub mod store_mem;
pub mod store_redb;
pub mod traits;
pub mod version;

pub use apply::{ApplyOutcome, RecoveryReport};
pub use engine::{DEFAULT_PAGE_LIMIT, PullReport, SyncEngine};
pub use error::{FsErrorKind, Result, SyncError};
pub use fs_real::RealFs;
pub use model::{
    BinEntry, BinReason, CausalMatrix, ConflictChoice, ConflictInfo, DEFAULT_BIN_RETENTION,
    ExpectedLocalSnapshot, FolderConfig, FolderId, FolderMeta, FolderStatus, HeldBackRecord,
    IndexRecord, MemberId, MemberState, MemberStatus, PendingOp, ProblemKind, RecordsPage,
    RestoreStrategy, SyncProblem, SyncProgress, validate_folder_id, validate_member_id,
};
pub use path::{
    IgnoreMatcher, SYNC_BIN_DIR, SYNC_META_DIR, SYNC_TMP_DIR, check_fs_compatibility,
    conflict_copy_path, is_fixed_ignored, is_internal_sync_path, validate_sync_path,
};
pub use reconcile::{LocalIndexView, ReconcileDecision, compare_live_winner, reconcile_record};
pub use scan::{ScanOptions, ScanReport};
pub use store_mem::MemoryIndexStore;
pub use store_redb::RedbIndexStore;
pub use traits::{
    FsCapabilities, FsMeta, FsWalkEntry, FsWalkResult, IndexStore, Peer, StoreBatch, SyncFs,
    SyncWrite,
};
pub use version::{VersionOrder, VersionVector};
