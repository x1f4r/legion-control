//! Platform-specific process, file, and locking primitives for Legion.
//!
//! Higher-level crates call into this crate so OS differences between Linux, macOS, and Windows
//! remain isolated behind a uniform API with no scattered `cfg` branches above this layer.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod atomic;
pub mod detach;
mod error;
pub mod file_id;
pub mod lock;
pub mod procid;
pub mod run;
pub mod shell;
pub mod user;
#[cfg(windows)]
mod win_proc;

pub use error::{OsError, Result};
