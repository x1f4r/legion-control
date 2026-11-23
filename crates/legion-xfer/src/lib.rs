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

pub mod error;
pub mod hash;
pub mod manifest;
mod platform;

pub use error::{Error, Result};
pub use hash::{Hash, HashingReader, HashingWriter};
pub use manifest::{
    CaseCollision, Entry, EntryKind, Manifest, ManifestOptions, SkippedEntry, Timestamp,
};
