//! A Legion node: where it keeps its files, how it reads its configuration, and what its policy
//! lets each principal do, which the registry and the audit log are built on.

pub mod config;
mod glob;
pub mod paths;
pub mod policy;

pub use config::{ConfigStore, Document};
pub use paths::Paths;
pub use policy::{Access, Grant, Policy};

/// The wire types, so a tool family needs only this crate.
pub use legion_proto as proto;
