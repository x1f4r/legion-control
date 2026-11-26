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
pub mod put;
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
/// Structured `details.reason` when a relay node does not advertise `stream.relay` and needs an
/// update before it can pass files along from another machine (`code: "unsupported"`).
pub const REASON_RELAY_TOO_OLD: &str = "relay_too_old";
/// Structured `details.reason` when neither this device nor its relay knows a machine by that name
/// (`code: "not_found"`).
pub const REASON_UNKNOWN_MACHINE: &str = "unknown_machine";
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

/// Attaches `details.sent = false` to an error that provably failed before any request bytes left
/// this device.
pub(crate) fn mark_not_sent(mut err: legion_proto::ErrorBody) -> legion_proto::ErrorBody {
    let mut details = err.details.take().unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = details.as_object_mut() {
        obj.insert(DETAIL_SENT.to_owned(), serde_json::Value::Bool(false));
    } else {
        details = serde_json::json!({
            DETAIL_SENT: false,
            "detail": details,
        });
    }
    err.details = Some(details);
    err
}

/// Preserves `err.machine` when the remote side already named which machine refused, falling back
/// to `fallback` otherwise.
pub(crate) fn ensure_error_machine(
    err: legion_proto::ErrorBody,
    fallback: &str,
) -> legion_proto::ErrorBody {
    if err.machine.is_some() {
        err
    } else {
        err.with_machine(fallback)
    }
}

/// Decodes a remote `{"error": ...}` stream frame into an [`legion_proto::ErrorBody`] using the
/// same forward-tolerance rules as [`Session`]:
///
/// - Any valid error object — including one carrying an unrecognized non-empty `code` string from
///   a newer peer — decodes via [`legion_proto::ErrorBody`]'s `Deserialize` impl (falling back to
///   [`legion_proto::ErrorCode::Failed`] with the original string preserved in
///   `details.unknown_code`).
/// - If the `error` payload cannot be decoded as an [`legion_proto::ErrorBody`] at all (missing
///   required fields, empty `code`, or wrong shape), returns [`legion_proto::ErrorCode::Internal`]
///   with `details.reason = "unrecognized_reply"` and the same wording as `session.rs`.
pub(crate) fn decode_remote_error(
    error_val: &serde_json::Value,
    fallback_machine: &str,
) -> legion_proto::ErrorBody {
    match serde_json::from_value::<legion_proto::ErrorBody>(error_val.clone()) {
        Ok(body) => ensure_error_machine(strip_remote_sent(body), fallback_machine),
        Err(decode_err) => {
            let machine = error_val
                .get("machine")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_machine);
            legion_proto::ErrorBody::new(
                legion_proto::ErrorCode::Internal,
                format!(
                    "{machine} answered with a reply this version of legion does not understand ({decode_err})."
                ),
            )
            .with_hint(
                "Update legion on whichever side is older so both understand the same replies.",
            )
            .with_machine(machine)
            .with_details(serde_json::json!({ "reason": "unrecognized_reply" }))
        }
    }
}

pub use command::{RemoteOs, Subcommand, remote_command, validate_program};
pub use content::{
    CAP_STREAM_RELAY, ContentStream, ContentTime, DEFAULT_MAX_PARKED_STREAMS, RawStream,
    StreamOpenError, StreamOpener,
};
pub use invite::{
    DEFAULT_ENROL_TIMEOUT, Enrolled, FleetPushOutcome, INVITE_PREFIX, INVITE_VERSION, Invite,
    MAX_INVITE_LEN, SshTarget, SshTimeouts,
};
#[cfg(feature = "ssh")]
pub use invite::{enrol, restore_pairing};
#[cfg(feature = "ssh")]
pub use key::DeviceKey;
pub use link::{
    ConflictingIdentityReport, ContentRoutePolicy, DEFAULT_CHANNEL_REFUSED_RECONNECT_COOLDOWN,
    DEFAULT_IDLE_TIMEOUT, DEFAULT_UNSYNCED_BACKOFF, DisagreementReport, LearnDisagreement,
    LearnRejection, LearnReport, Link, LinkConfig, LinkOptions, MAX_CONFIGURED_NODES,
    MAX_ENDPOINT_FIELD_LEN, MAX_ENDPOINT_HOST_KEY_LEN, MAX_ENDPOINT_HOST_KEYS, MAX_ENDPOINT_HOSTS,
    MachineRouteStatus, NodeLink, NodeOrigin, PendingIdentityChange, Route, RoutedValue,
    SessionOpener, parse_optional_target, public_key_fingerprint,
};
pub use put::{CAP_IF_EXISTS, PutIfExists, PutRequest, PutResult, PutStream};
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

#[cfg(test)]
mod decode_tests {
    use super::*;
    use legion_proto::ErrorCode;
    use serde_json::json;

    #[test]
    fn decode_remote_error_matches_session_for_unknown_code_and_malformed_frames() {
        // 1. Unknown non-empty error code decodes via `ErrorBody` to `Failed` with
        //    `details.unknown_code`, preserving existing details and stripping spoofed `sent`.
        let unknown = decode_remote_error(
            &json!({
                "code": "future_fancy_code",
                "message": "Future stream error from a newer node.",
                "hint": "Upgrade client.",
                "details": { "extra": 42, "sent": false }
            }),
            "tower",
        );
        assert_eq!(unknown.code, ErrorCode::Failed);
        assert_eq!(unknown.message, "Future stream error from a newer node.");
        assert_eq!(unknown.hint.as_deref(), Some("Upgrade client."));
        assert_eq!(unknown.machine.as_deref(), Some("tower"));
        assert_eq!(
            unknown.details,
            Some(json!({ "extra": 42, "unknown_code": "future_fancy_code" }))
        );

        // 2. Unparseable error shapes (empty code, missing message, non-object) fail with
        //    `Internal` + `details.reason = "unrecognized_reply"`, matching `session.rs`.
        for malformed in [
            json!({ "code": "", "message": "empty code" }),
            json!({ "code": "failed" }),
            json!("unexpected string payload"),
        ] {
            let err = decode_remote_error(&malformed, "tower");
            assert_eq!(err.code, ErrorCode::Internal, "for {malformed}: {err:?}");
            assert_eq!(err.machine.as_deref(), Some("tower"));
            assert!(
                err.message.contains("tower") && err.message.contains("does not understand"),
                "unexpected message for {malformed}: {}",
                err.message
            );
            assert_eq!(err.details, Some(json!({ "reason": "unrecognized_reply" })));
        }
    }
}
