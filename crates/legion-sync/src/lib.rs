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
//!   using non-replacing renames or atomic swaps (`renameat2`, `renamex_np`, `ReplaceFileW` /
//!   `MoveFileExW`, with claim-then-rename fallback) to move verified prior copies to
//!   `.legion-sync/bin` and place the new file.
//! - **Crash safety**: every working-tree mutation (including `restore_from_bin` and
//!   `resolve_conflict`) is bracketed by a durable [`PendingOp`] write-ahead intent in the index
//!   store; non-crash errors mid-apply recover immediately, and restarting after a crash at any
//!   step converges without losing bytes or fabricating conflicts.
//! - **No silent data loss**: remote deletions and replacements move existing copies to
//!   `.legion-sync/bin` (30-day default retention, random bin IDs unique across restarts, clock
//!   sanity guard on expiry, self-describing restore metadata); concurrent different edits
//!   preserve both versions under deterministic content-hashed names
//!   (`name (changed on <machine> <date>, <hash6>).ext`) with independent version vectors so sync
//!   never replaces a live conflict copy with different content; delete-vs-change keeps the
//!   change.
//! - **Safe scanning & identity**: unreadable directories never emit deletions; `.legion-sync/marker.json`
//!   stops unmounted or replaced folders before any deletion can be recorded; a mass-deletion
//!   brake pauses before recording large-scale deletions; random per-index incarnations detect
//!   backup restores, reinstalls, and cloned member IDs; short per-path hash ancestry and deleted
//!   history prevent stale copies from winning or resurrecting deletions on join or re-add.
//! - **Direct acknowledgements only**: deletion tombstones are discarded only after every other
//!   current folder member has directly confirmed seeing the local member's `(incarnation, seq)` —
//!   third-party hearsay is never accepted.
//! - **Fixed ignore rules**: `.legion-sync` at any depth (including case-folded and NFKC
//!   variants), `.DS_Store`, `Thumbs.db`, `desktop.ini`, and Legion in-flight files
//!   (`*.legion-part`, `.legion-*.tmp`). `.git` and editor/download temporary files are left to
//!   per-folder `ignore` globs. Nested sync folders are refused.
//! - **Platform honesty**: names the local filesystem cannot hold (case clashes on
//!   case-insensitive or Unicode-normalizing systems, Windows reserved names and forbidden
//!   characters, over-long paths, and symbolic links on Windows) are held back and reported as
//!   actionable problems rather than mangled.
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
//!   `FOLDED_LIVE`, `BIN_ENTRIES`, `HELD_BACK`, `PEER_CURSORS`, `DELETED_HISTORY`, and `META`
//!   (with an explicit `schema_version` refused cleanly when newer).
//! - **500,000+ file scale**: compact versioned binary record encoding (`codec`), O(log N + K)
//!   incremental sequence scans via `BY_SEQ`, O(T) tombstone GC via `TOMBSTONES`, subtree scans
//!   for file watchers, and per-page group commits keep scans and applies fast.

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
pub use engine::{DEFAULT_PAGE_LIMIT, FOLDER_MARKER_PROBLEM_SENTENCE, PullReport, SyncEngine};
pub use error::{FsErrorKind, Result, SyncError};
pub use fs_real::{RealFs, check_folders_not_nested};
pub use model::{
    BinEntry, BinReason, ConflictChoice, ConflictInfo, DEFAULT_BIN_RETENTION,
    DEFAULT_BRAKE_MIN_FILES, DEFAULT_BRAKE_MIN_PERCENT, DEFAULT_FETCH_TIMEOUT, DeferredItem,
    DeletionBrake, DeletionBrakeConfig, DeletionBrakeDecision, ExpectedLocalSnapshot,
    FORCED_ROTATION_COOLDOWN_SECS, FetchOptions, FolderConfig, FolderDeletionLogEntry,
    FolderDeletionRecord, FolderId, FolderMarker, FolderMeta, FolderStatus, HandshakeHello,
    HeldBackRecord, INDEX_SCHEMA_VERSION, IndexRecord, MAX_ANCESTRY_HASHES, MAX_DEFERRAL_ATTEMPTS,
    MAX_HELD_BACK_ENTRIES, MAX_LINK_TARGET_BYTES, MAX_PAGE_RECORDS, MAX_PEER_PROBLEMS,
    MAX_VERSION_COUNTER, MAX_VV_INCARNATIONS_PER_MEMBER, MIN_PLAUSIBLE_CLOCK_SECS, MemberId,
    MemberState, MemberStatus, PeerAckView, PeerCursor, PeerHandshake, PendingOp, ProblemKind,
    RecordsPage, RestoreStrategy, SYNC_PROTOCOL_VERSION, SyncProblem, SyncProgress,
    chain_fingerprint, encode_deletion_log_entries, parse_deletion_log, validate_folder_id,
    validate_member_id,
};
pub use path::{
    IgnoreMatcher, ParsedConflictPath, SYNC_BIN_DIR, SYNC_DELETIONS_LOG_FILE, SYNC_MARKER_FILE,
    SYNC_META_DIR, SYNC_TMP_DIR, check_fs_compatibility, check_symlink_target_contained,
    conflict_copy_path, is_fixed_ignored, is_internal_sync_path, is_os_litter_name,
    parse_conflict_copy_path, validate_sync_path,
};
pub use reconcile::{
    LocalIndexView, ReconcileDecision, compare_live_winner, derive_conflict_of, reconcile_record,
    validate_incoming_record,
};
pub use scan::{ScanOptions, ScanReport};
pub use store_mem::MemoryIndexStore;
pub use store_redb::RedbIndexStore;
pub use traits::{
    FsCapabilities, FsMeta, FsWalkEntry, FsWalkResult, IndexStore, Peer, StoreBatch, SyncFs,
    SyncWrite,
};
pub use version::{VersionKey, VersionOrder, VersionVector};
