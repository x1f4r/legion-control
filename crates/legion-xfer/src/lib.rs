#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! Transport-agnostic file transfer engine.
//!
//! Each operation works over plain blocking [`std::io::Read`] and [`std::io::Write`] streams so
//! local copies and copies relayed through remote pipes share the same code paths.

pub mod archive;
pub mod error;
pub mod file;
pub mod hash;
pub mod manifest;
pub mod path;
pub mod plan;
mod platform;

pub use archive::{
    PackReport, PlacedFileMeta, RenamedEntry, UnpackReport, pack, unpack, unpack_filtered,
    unpack_with_options, unpack_with_options_and_hashes, unpack_with_options_hashes_and_state,
};
pub use error::{Error, Result};
pub use file::{
    GetStream, IfExists, PART_SUFFIX, PlaceOutcome, PutOutcome, PutReport, PutSession,
    STALE_TEMP_AGE, default_xfer_state_dir, get, move_lock_state_path, numbered_candidate,
    open_get, part_lock_state_path, part_meta_state_path, part_path, put, reclaim_stale_claim,
    resume_offset, resume_offset_for, resume_offset_for_with_state, resume_offset_with_state,
    sweep_stale_state, sweep_stale_temps, try_lock_state_file, write_partial_meta_in_state,
};
pub use hash::{Hash, HashingReader, HashingWriter};
pub use manifest::{
    CaseCollision, Entry, EntryKind, Manifest, ManifestOptions, SkippedEntry, Timestamp,
};
pub use path::{resolve_relative_path, validate_relative_path};
pub use plan::{Plan, PlanConflict, PlanOptions, PlanTotals};
pub use platform::{is_legion_internal_name, is_sweepable_temp_name};
