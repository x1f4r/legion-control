//! Fallback stub for platforms without a supported OS decode worker sandbox.
//!
//! Always fails closed with [`SandboxUnavailable`].

use crate::sandbox::{SandboxReport, SandboxUnavailable};

pub(super) fn apply_worker_sandbox() -> std::result::Result<SandboxReport, SandboxUnavailable> {
    Err(SandboxUnavailable {
        platform: std::env::consts::OS.to_owned(),
        missing: "OS sandbox implementation".to_owned(),
        detail: format!(
            "no decode worker sandbox is implemented for platform {}",
            std::env::consts::OS
        ),
    })
}
