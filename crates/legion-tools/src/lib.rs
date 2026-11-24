//! The tool families built into Legion, one module per family.
//!
//! Each module has a `register(&mut Registry)` that adds its tools; [`register_all`] calls every
//! one. A new family is a new module with its own `register`, added to [`register_all`]. Tools
//! never know which door or machine a call came from beyond what their [`Ctx`](legion_core::Ctx)
//! says, so they work the same on the command line, over MCP and when forwarded from another node.

pub mod audit;
pub mod exec;
pub mod fs;
pub mod job;
pub mod meta;
pub mod proj;
pub mod svc;
pub mod sys;
#[cfg(feature = "testing")]
pub mod testing;
pub mod token;
pub mod xfer;

use legion_core::Registry;

/// Adds every built-in family to `registry`.
pub fn register_all(registry: &mut Registry) {
    meta::register(registry);
    audit::register(registry);
    sys::register(registry);
    exec::register(registry);
    fs::register(registry);
    job::register(registry);
    proj::register(registry);
    svc::register(registry);
    xfer::register(registry);
    token::register(registry);
    #[cfg(feature = "testing")]
    testing::register(registry);
}

/// A registry holding every built-in tool.
pub fn registry() -> Registry {
    let mut registry = Registry::new();
    register_all(&mut registry);
    registry
}
