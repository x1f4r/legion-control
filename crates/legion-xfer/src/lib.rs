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
    PackReport, RenamedEntry, UnpackReport, pack, unpack, unpack_filtered, unpack_with_options,
    unpack_with_options_and_hashes,
};
pub use error::{Error, Result};
pub use file::{
    GetStream, IfExists, PART_SUFFIX, PlaceOutcome, PutOutcome, PutReport, PutSession,
    STALE_TEMP_AGE, get, numbered_candidate, open_get, part_path, put, resume_offset,
    sweep_stale_temps,
};
pub use hash::{Hash, HashingReader, HashingWriter};
pub use manifest::{
    CaseCollision, Entry, EntryKind, Manifest, ManifestOptions, SkippedEntry, Timestamp,
};
pub use path::{resolve_relative_path, validate_relative_path};
pub use plan::{Plan, PlanConflict, PlanOptions, PlanTotals};
