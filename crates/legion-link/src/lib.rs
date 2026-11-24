//! The client side of the Legion wire protocol: multiplexed JSON-RPC 2.0 sessions over any async
//! byte stream, fixed remote command strings shared with `legion gate`, pairing invites, device
//! keys, an optional in-process SSH pipe (`ssh`), an optional desktop child-process pipe (`local`),
//! and the high-level [`Link`] facade for controller apps.
//!
//! # Features and dependencies
//!
//! By default this crate depends only on `legion-proto`, `serde`, `serde_json`, `base64`, `tokio`,
//! and `tokio-util` (no process spawning, no SSH library, no `legion-os` / `legion-tools` /
//! `legion-core`). The `legion` node binary depends on `legion-link` with default features only so
//! `russh` is never compiled into the node binary.
//!
//! - **`ssh`**: adds [`DeviceKey`], [`ssh::open`], [`ssh::open_raw`], and [`enrol`] using `russh`.
//! - **`local`**: adds [`local::open_local_session`] for desktop apps that spawn a local `legion
//!   rpc` child process.
//! - **`schemars`**: derives `JsonSchema` on [`RemoteOs`] for `legion-net`.
//!
//! ## SSH crypto backend choice
//!
//! The `ssh` feature configures `russh` with `default-features = false, features = ["ring", "rsa"]`
//! rather than `aws-lc-rs`. `ring` builds its pre-generated assembly with only a C compiler (`cc`),
//! requiring no `cmake`, `nasm`, `perl`, or `bindgen` — which makes cross-compiling for Android
//! (`aarch64-linux-android`, `x86_64-linux-android` with the Android NDK) and iOS
//! (`aarch64-apple-ios`, `aarch64-apple-ios-sim` with Xcode) straightforward. Enabling `rsa` adds
//! pure-Rust support for RSA server host keys alongside Ed25519 and ECDSA. The negotiated algorithm
//! list is restricted to modern algorithms accepted by default by OpenSSH 8+ and Windows OpenSSH.
//!
//! # JSON result shapes across all three pipes
//!
//! [`Link`] returns the same JSON shapes as `legion rpc` and `legion serve` (`/v1/*`) so one
//! controller interface can sit on top of in-app SSH, a local child process, or HTTPS:
//!
//! - **`hello`**: `{"node", "version", "build"?, "protocol", "os", "arch", "systems", "system"?, "boot_id"?}`
//! - **`tools`**: `{"tools": [{"name", "description", "effect", "input_schema"}, ...]}`
//! - **`call` (single machine)**: the tool's own JSON object.
//! - **`call` (fan-out via `on`)**: `{"results": {"<machine>": {"ok": {...}} | {"error": ErrorBody}}}`.
//! - **`events` callback**: `{"kind": "stdout"|"stderr"|"progress"|"log", "data": ..., "machine"?: "..."}`.
//! - **Errors**: [`legion_proto::ErrorBody`] (`{"code", "message", "hint"?, "machine"?, "details"?}`).
//!   Because [`legion_proto::ErrorCode`] is a closed 13-code wire set, SSH-specific rejections use
//!   `code: "denied"` with `details.reason` set to `"host_key_mismatch"` (plus `details.offered`)
//!   or `"auth_failed"`, while connect timeouts and unreachable hosts reuse `code: "timeout"` and
//!   `code: "unreachable"`.

pub mod command;
pub mod invite;
#[cfg(feature = "ssh")]
pub mod key;
pub mod link;
#[cfg(feature = "local")]
pub mod local;
pub mod session;
#[cfg(feature = "ssh")]
pub mod ssh;
pub mod stderr;

pub use command::{RemoteOs, Subcommand, remote_command, validate_program};
#[cfg(feature = "ssh")]
pub use invite::enrol;
pub use invite::{Enrolled, INVITE_PREFIX, INVITE_VERSION, Invite, SshTarget};
#[cfg(feature = "ssh")]
pub use key::DeviceKey;
pub use link::{
    DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_IDLE_TIMEOUT, Link, LinkConfig, LinkOptions, NodeLink,
    SessionOpener, parse_optional_target,
};
pub use session::{
    CallError, OpenError, Session, SessionInfo, TransportHandle, cancel_reply_timeout,
};
#[cfg(feature = "ssh")]
pub use ssh::{
    REASON_AUTH_FAILED, REASON_HOST_KEY_MISMATCH, REASON_TIMEOUT, REASON_UNREACHABLE, SshPipe,
    SshPipeHandle, SshTimeouts,
};
pub use stderr::StderrTail;

/// Wire types re-exported from `legion-proto`.
pub use legion_proto as proto;
/// Cancellation token re-exported from `tokio-util`.
pub use tokio_util::sync::CancellationToken;
