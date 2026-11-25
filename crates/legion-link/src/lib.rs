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
//! - **`ssh`**: adds `DeviceKey`, `ssh::open`, `ssh::open_raw`, `enrol`, and `Link::pair` using
//!   `russh`.
//! - **`local`**: adds `local::open_local_session` for desktop apps that spawn a local `legion
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
//!
//! # Introducer trust
//!
//! Only directly paired machines may act as introducers or preferred relays; learned machines never
//! introduce other machines. When two or more paired nodes report a brand-new machine with
//! differing endpoints or host keys, it is recorded as a disagreement and not pinned. When another
//! paired node cannot reach a brand-new machine (returns an error), that counts as an abstention
//! rather than a disagreement, so a single reporting paired node pins the new machine on its own
//! when no other paired node can see it; it can never alter a paired machine's pins or silently
//! replace an already known machine's keys or route.
//!
//! # Stable error reasons
//!
//! Because [`legion_proto::ErrorCode`] is a closed 13-code wire set, callers and GUIs can branch
//! on `error.details.reason` without parsing human-readable `message` text. The stable reason
//! strings exported as crate-root constants are:
//!
//! - **`"device_removed_or_unknown"`** ([`REASON_DEVICE_REMOVED_OR_UNKNOWN`], `code: "denied"`):
//!   emitted by [`Link`] when SSH public-key authentication to a paired node is rejected — for
//!   example because the device was removed on the node (`legion device.remove`) or is unknown to
//!   it. Preserves `details.host`, `details.port`, and `details.user` when available, and includes
//!   a `hint` instructing the user to pair the device again.
//! - **`"auth_failed"`** ([`REASON_AUTH_FAILED`], `code: "denied"`): emitted by low-level
//!   `ssh::open` / `ssh::open_raw` and by `enrol` / `Link::pair` when SSH public-key
//!   authentication is rejected (during pairing, usually because the single-use invite expired or
//!   was already consumed).
//! - **`"host_key_mismatch"`** ([`REASON_HOST_KEY_MISMATCH`], `code: "denied"`): the server
//!   offered a host key that did not match any pinned key in `host_keys`. Includes `details.host`,
//!   `details.port`, and either `details.offered` / `details.expected` SHA-256 fingerprints or
//!   `details.offered_algorithms` / `details.pinned_algorithms`.
//! - **`"node_mismatch"`** ([`REASON_NODE_MISMATCH`], `code: "denied"`): the connected SSH server
//!   reported a different `hello.node` name than the configured entry; the session is closed
//!   without executing any tool or failing over. Includes `details.expected` and `details.actual`.
//! - **`"timeout"`** ([`REASON_TIMEOUT`], `code: "timeout"`): DNS lookup, TCP connect, or SSH
//!   handshake timed out. Includes `details.host` and `details.port` when a single host failed.
//! - **`"unreachable"`** ([`REASON_UNREACHABLE`], `code: "unreachable"`): DNS lookup failed, TCP
//!   connection was refused or errored, or the SSH transport or channel failed. Includes
//!   `details.host` and `details.port` when a single host failed.
//! - **`"non_protocol_output"`** ([`REASON_NON_PROTOCOL_OUTPUT`], `code: "unreachable"`): the
//!   remote command emitted too much non-JSON-RPC output before answering `hello` (for example a
//!   noisy login shell profile or an unexpected forced command).
//!
//! # Troubleshooting
//!
//! ## Android 17 local-network permission
//!
//! Android 17 blocks an app's outbound sockets to local/LAN addresses and the CGNAT range
//! (`100.64.0.0/10`, used by overlay networks such as Tailscale) until the app holds the
//! local-network permission. From `legion-link`'s side, blocked connections do not report a
//! distinct OS permission error — they surface as connect timeouts (`code: "timeout"`,
//! `details.reason: "timeout"`). Android apps embedding `legion-link` should request the
//! local-network permission before connecting, or prompt the user to grant it when local or CGNAT
//! nodes time out.

pub mod command;
pub mod content;
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

/// Structured `details.reason` for a pinned host key mismatch (`code: "denied"`).
pub const REASON_HOST_KEY_MISMATCH: &str = "host_key_mismatch";
/// Structured `details.reason` when a connected SSH server's `hello` handshake reports a different
/// node name than the configured entry (`code: "denied"`).
pub const REASON_NODE_MISMATCH: &str = "node_mismatch";
/// Structured `details.reason` for rejected SSH key authentication on a low-level SSH open or
/// invite enrolment (`code: "denied"`).
pub const REASON_AUTH_FAILED: &str = "auth_failed";
/// Structured `details.reason` emitted by [`Link`] when SSH key authentication to a paired node
/// is rejected, indicating the device key was removed from or is unknown on that node
/// (`code: "denied"`).
pub const REASON_DEVICE_REMOVED_OR_UNKNOWN: &str = "device_removed_or_unknown";
/// Structured `details.reason` for a DNS, TCP connect, or SSH handshake timeout
/// (`code: "timeout"`).
pub const REASON_TIMEOUT: &str = "timeout";
/// Structured `details.reason` for an unreachable host or failed SSH transport/channel
/// (`code: "unreachable"`).
pub const REASON_UNREACHABLE: &str = "unreachable";
/// Structured `details.reason` when the remote side emits too much non-protocol output before
/// answering `hello` (`code: "unreachable"`).
pub const REASON_NON_PROTOCOL_OUTPUT: &str = "non_protocol_output";
/// Structured `details` boolean key set to `false` when a [`Link`] request provably never left
/// this device (failure to open a session or [`CallError::NotWritten`]). Absent when bytes were
/// already written to the wire or when an error came back from the node.
pub const DETAIL_SENT: &str = "sent";

/// Removes any caller-reserved [`DETAIL_SENT`] key from an error received over the wire so a
/// remote node can never spoof `details.sent = false` to trigger failover or retries.
pub(crate) fn strip_remote_sent(mut err: legion_proto::ErrorBody) -> legion_proto::ErrorBody {
    if let Some(serde_json::Value::Object(ref mut obj)) = err.details {
        obj.remove(DETAIL_SENT);
        if obj.is_empty() {
            err.details = None;
        }
    }
    err
}

pub use command::{RemoteOs, Subcommand, remote_command, validate_program};
pub use content::{
    ContentStream, ContentTime, DEFAULT_MAX_PARKED_STREAMS, RawStream, StreamOpenError,
    StreamOpener,
};
#[cfg(feature = "ssh")]
pub use invite::enrol;
pub use invite::{
    Enrolled, FleetPushOutcome, INVITE_PREFIX, INVITE_VERSION, Invite, MAX_INVITE_LEN, SshTarget,
    SshTimeouts,
};
#[cfg(feature = "ssh")]
pub use key::DeviceKey;
pub use link::{
    ConflictingIdentityReport, DEFAULT_IDLE_TIMEOUT, DEFAULT_UNSYNCED_BACKOFF, DisagreementReport,
    LearnDisagreement, LearnReport, Link, LinkConfig, LinkOptions, MAX_CONFIGURED_NODES,
    MAX_ENDPOINT_FIELD_LEN, MAX_ENDPOINT_HOST_KEY_LEN, MAX_ENDPOINT_HOST_KEYS, MAX_ENDPOINT_HOSTS,
    MachineRouteStatus, NodeLink, NodeOrigin, PendingIdentityChange, Route, RoutedValue,
    SessionOpener, parse_optional_target, public_key_fingerprint,
};
pub use session::{
    CallError, OpenError, Session, SessionInfo, TransportHandle, cancel_reply_timeout,
};
#[cfg(feature = "ssh")]
pub use ssh::{SshPipe, SshPipeHandle};
pub use stderr::StderrTail;

/// Wire types re-exported from `legion-proto`.
pub use legion_proto as proto;
/// Cancellation token re-exported from `tokio-util`.
pub use tokio_util::sync::CancellationToken;
