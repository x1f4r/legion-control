//! Platform-specific process, file, and locking primitives for Legion.
//!
//! Higher-level crates call into this crate so OS differences between Linux, macOS, and Windows
//! remain isolated behind a uniform API with no scattered `cfg` branches above this layer.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod atomic;
pub mod capped_alloc;
pub mod detach;
pub mod display_awake;
mod error;
pub mod file_id;
pub mod fs_kind;
pub mod hardware;
pub mod lock;
pub mod mem_limit;
pub mod open_regular;
pub mod procid;
pub mod run;
pub mod shell;
pub mod user;
pub mod webview2;
#[cfg(windows)]
mod win_proc;

pub use error::{OsError, Result};
