//! A Legion node: where it keeps its files and how it reads its configuration, which the policy,
//! the registry and the audit log are built on.

pub mod config;
pub mod paths;

pub use config::{ConfigStore, Document};
pub use paths::Paths;

/// The wire types, so a tool family needs only this crate.
pub use legion_proto as proto;
