//! Cryptographic keys, OpenSSH wire formats, fingerprints, invite tokens, and sshd host keys.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use ed25519_dalek::{SigningKey, VerifyingKey};
use legion_core::CancellationToken;
use legion_core::policy::expand_tilde;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Timeout for one `ssh-keyscan` probe against local `sshd`.
const KEYSCAN_TIMEOUT: Duration = Duration::from_secs(7);

/// How long `device.endpoint` caches an auto-discovered host-key scan per `(node, port)` so
/// repeated read-only endpoint discovery calls do not repeatedly run `ssh-keyscan`.
pub const HOST_KEY_CACHE_TTL: Duration = Duration::from_secs(60);

/// Prefix of every Legion v1 pairing invite string.
pub const INVITE_PREFIX: &str = "legion-invite-1.";

/// Wire format version of the pairing invite payload.
pub const INVITE_VERSION: u32 = 1;

/// Byte length of an OpenSSH `ssh-ed25519` public key blob: `u32(11) + "ssh-ed25519" + u32(32) + pk(32)`.
const ED25519_WIRE_BLOB_LEN: usize = 4 + 11 + 4 + 32;

/// Maximum accepted byte length for an incoming `ssh-ed25519` public key line during enrolment.
const MAX_PUBLIC_KEY_LEN: usize = 512;

/// Random byte length of an invite identifier (128 bits -> 32 lowercase hex characters).
const INVITE_ID_BYTES: usize = 16;

/// Maximum accepted byte length of an invite identifier.
const MAX_INVITE_ID_LEN: usize = 64;

/// JSON payload encoded inside `legion-invite-1.<base64url>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitePayload {
    /// Contract version (always `1`).
    pub v: u32,
    /// Single-use invite identifier.
    pub id: String,
    /// Node name as configured in `node.toml` or derived from the hostname.
    pub node: String,
    /// Candidate host names or Tailscale addresses to reach this node's sshd.
    pub hosts: Vec<String>,
    /// SSH port on the node.
    pub port: u16,
    /// OS account name on the node.
    pub user: String,
    /// Pinned SSH public host keys (`"<type> <base64>"`, without comments).
    pub host_keys: Vec<String>,
    /// Standard base64 of the temporary Ed25519 key's 32-byte seed.
    pub key: String,
    /// Principal the paired device will act as after enrolment.
    pub principal: String,
    /// RFC 3339 UTC timestamp after which this invite is no longer valid.
    pub expires: String,
}

impl InvitePayload {
    /// Encodes the invite payload to the `legion-invite-1.<unpadded base64url>` contract format.
    pub fn encode(&self) -> Result<String, ErrorBody> {
        let json = serde_json::to_vec(self).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("The pairing invite cannot be encoded as JSON: {error}."),
            )
        })?;
        Ok(format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(json)))
    }

    /// Decodes a `legion-invite-1.<unpadded base64url>` string into its [`InvitePayload`].
    pub fn decode(text: &str) -> Result<InvitePayload, ErrorBody> {
        let encoded = text.trim().strip_prefix(INVITE_PREFIX).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Invite text must start with `{INVITE_PREFIX}`."),
            )
        })?;
        let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|error| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Invite text is not valid base64url: {error}."),
            )
        })?;
        let payload: InvitePayload = serde_json::from_slice(&bytes).map_err(|error| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Invite payload is not valid JSON: {error}."),
            )
        })?;
        if payload.v != INVITE_VERSION {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "Unsupported invite version {}; this build supports version {INVITE_VERSION}.",
                    payload.v
                ),
            ));
        }
        Ok(payload)
    }

    /// Decodes the standard-base64 32-byte Ed25519 private seed carried in `key`.
    pub fn decode_seed(&self) -> Result<[u8; 32], ErrorBody> {
        let raw = STANDARD.decode(&self.key).map_err(|error| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Invite key is not valid standard base64: {error}."),
            )
        })?;
        <[u8; 32]>::try_from(raw.as_slice()).map_err(|_| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Invite key decoded to {} bytes; expected a 32-byte Ed25519 seed.",
                    raw.len()
                ),
            )
        })
    }
}

/// Freshly generated single-use Ed25519 key pair for an invite.
pub(crate) struct EphemeralEd25519 {
    /// Standard base64 of the 32-byte Ed25519 private seed (never stored or logged).
    pub(crate) seed_b64: String,
    /// OpenSSH public key string: `"ssh-ed25519 <base64>"`.
    pub(crate) public_key: String,
    /// `SHA256:...` fingerprint matching `ssh-keygen -l`.
    pub(crate) fingerprint: String,
}

/// Generates a fresh 32-byte Ed25519 key pair using OS randomness.
pub(crate) fn generate_ephemeral_ed25519() -> Result<EphemeralEd25519, ErrorBody> {
    let mut seed = [0_u8; 32];
    getrandom::fill(&mut seed).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("The operating system gave no random bytes for the pairing key: {error}."),
        )
    })?;
    let signing = SigningKey::from_bytes(&seed);
    let public_bytes = signing.verifying_key().to_bytes();
    let blob = encode_ed25519_wire_blob(&public_bytes);
    let seed_b64 = STANDARD.encode(seed);
    // Overwrite the stack copy of the seed once encoded.
    seed.fill(0);
    let key_b64 = STANDARD.encode(blob);
    let fingerprint = fingerprint_blob(&blob);
    Ok(EphemeralEd25519 {
        seed_b64,
        public_key: format!("ssh-ed25519 {key_b64}"),
        fingerprint,
    })
}

/// Generates a random single-use invite id (32 lowercase hex characters).
pub(crate) fn generate_invite_id() -> Result<String, ErrorBody> {
    let mut bytes = [0_u8; INVITE_ID_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("The operating system gave no random bytes for the invite id: {error}."),
        )
    })?;
    let mut out = String::with_capacity(INVITE_ID_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Validates an invite id passed on the command line, over stdin, or to `device.remove`.
pub fn validate_invite_id(id: &str) -> Result<(), ErrorBody> {
    let starts_ok = id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric);
    let all_ok = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if id.is_empty() || id.len() > MAX_INVITE_ID_LEN || !starts_ok || !all_ok {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{id:?} is not a valid invite id."),
        )
        .with_hint("Use the invite id reported by `device.invite` or `device.list`."));
    }
    Ok(())
}

/// Generates a random per-pairing identifier (128 bits, 32 lowercase hex characters) embedded in
/// `legion gate --as ... --device <name> --pairing <id>` so re-pairing under the same device name
/// and principal can never revive an earlier revoked pairing's open SSH connection.
pub(crate) fn generate_pairing_id() -> Result<String, ErrorBody> {
    let mut bytes = [0_u8; INVITE_ID_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("The operating system gave no random bytes for the pairing id: {error}."),
        )
    })?;
    let mut out = String::with_capacity(INVITE_ID_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Validates a per-pairing identifier passed via `--pairing <id>` (32 lowercase hex characters).
pub fn validate_pairing_id(id: &str) -> Result<(), ErrorBody> {
    let valid_hex = id.len() == INVITE_ID_BYTES * 2
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid_hex {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{id:?} is not a valid pairing id."),
        )
        .with_hint("Pairing ids are generated automatically during device enrolment."));
    }
    Ok(())
}

/// Encodes a raw 32-byte Ed25519 public key into the 51-byte OpenSSH wire blob.
pub fn encode_ed25519_wire_blob(public_key: &[u8; 32]) -> [u8; ED25519_WIRE_BLOB_LEN] {
    let mut blob = [0_u8; ED25519_WIRE_BLOB_LEN];
    blob[0..4].copy_from_slice(&11_u32.to_be_bytes());
    blob[4..15].copy_from_slice(b"ssh-ed25519");
    blob[15..19].copy_from_slice(&32_u32.to_be_bytes());
    blob[19..51].copy_from_slice(public_key);
    blob
}

/// Computes the `SHA256:...` fingerprint of a raw SSH public key wire blob, matching `ssh-keygen -l`.
pub fn fingerprint_blob(blob: &[u8]) -> String {
    let digest = Sha256::digest(blob);
    format!("SHA256:{}", STANDARD_NO_PAD.encode(digest))
}

/// Computes the `SHA256:...` fingerprint from the base64 portion of an OpenSSH public key.
pub fn fingerprint_key_b64(key_b64: &str) -> Option<String> {
    let blob = STANDARD.decode(key_b64).ok()?;
    if blob.is_empty() {
        return None;
    }
    Some(fingerprint_blob(&blob))
}

/// Computes sorted, deduplicated `SHA256:...` fingerprints for a slice of OpenSSH public host keys
/// (`"<type> <base64>[ comment]"`).
pub fn host_key_fingerprints(host_keys: &[String]) -> Vec<String> {
    let mut fps = BTreeSet::new();
    for raw in host_keys {
        if let Some(key_b64) = raw.split_whitespace().nth(1)
            && let Some(fp) = fingerprint_key_b64(key_b64)
        {
            fps.insert(fp);
        }
    }
    fps.into_iter().collect()
}

/// Strictly validated `ssh-ed25519` public key ready to be written to `authorized_keys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedEd25519Key {
    /// Canonical `"ssh-ed25519 <base64>"` without options or caller-supplied comments.
    pub canonical: String,
    /// Canonical standard base64 of the 51-byte SSH wire blob.
    pub key_b64: String,
    /// Raw 51-byte OpenSSH wire blob (`u32(11) || "ssh-ed25519" || u32(32) || pk`).
    pub wire_blob: [u8; ED25519_WIRE_BLOB_LEN],
    /// `SHA256:...` fingerprint matching `ssh-keygen -l`.
    pub fingerprint: String,
}

/// Computes the canonical `"ssh-ed25519 <base64>"` public key string for a 32-byte Ed25519 seed.
pub fn ed25519_public_key_from_seed(seed: &[u8; 32]) -> String {
    let signing = SigningKey::from_bytes(seed);
    let blob = encode_ed25519_wire_blob(&signing.verifying_key().to_bytes());
    format!("ssh-ed25519 {}", STANDARD.encode(blob))
}

/// Strictly validates a caller-supplied `ssh-ed25519` public key for enrolment.
///
/// Accepts only `"ssh-ed25519 <canonical-base64>[ <safe-comment>]"` where:
/// - total length is bounded (`<= 512` bytes),
/// - no control characters (including `\n`, `\r`, `\t`, `\0`) or `"` appear anywhere,
/// - no `authorized_keys` options precede `ssh-ed25519`,
/// - base64 decodes canonically to a 51-byte `ssh-ed25519` wire blob whose 32-byte payload is a
///   valid, non-weak Ed25519 curve point, and
/// - any optional trailing comment does not smuggle a second key.
///
/// Any trailing comment is discarded from [`ValidatedEd25519Key::canonical`]; enrolment always
/// writes its own `legion-device:<name>` comment.
pub fn validate_ed25519_public_key(raw: &str) -> Result<ValidatedEd25519Key, ErrorBody> {
    let bad = |reason: &str| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Invalid ssh-ed25519 public key: {reason}."),
        )
        .with_hint("Send a single `ssh-ed25519 <base64>` public key with no options or newlines.")
    };

    if raw.is_empty() {
        return Err(bad("key is empty"));
    }
    if raw.len() > MAX_PUBLIC_KEY_LEN {
        return Err(bad("key exceeds maximum allowed length"));
    }
    if raw.chars().any(|c| c.is_control() || c == '"') {
        return Err(bad("key must not contain control characters or quotes"));
    }
    if raw.starts_with(' ') || raw.ends_with(' ') {
        return Err(bad("key must not have leading or trailing spaces"));
    }

    let Some((key_type, rest)) = raw.split_once(' ') else {
        return Err(bad("expected `ssh-ed25519 <base64>`"));
    };
    if key_type != "ssh-ed25519" {
        return Err(bad(
            "only plain `ssh-ed25519` keys with no options are accepted",
        ));
    }

    let (key_b64, comment) = match rest.split_once(' ') {
        Some((key, comment)) => (key, Some(comment)),
        None => (rest, None),
    };
    if key_b64.is_empty() {
        return Err(bad("missing base64 key data after `ssh-ed25519`"));
    }
    if !key_b64
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err(bad("key data contains non-base64 characters"));
    }

    let blob = STANDARD
        .decode(key_b64)
        .map_err(|_| bad("key data is not valid standard base64"))?;
    if blob.len() != ED25519_WIRE_BLOB_LEN {
        return Err(bad("decoded key blob has the wrong length for ssh-ed25519"));
    }
    if blob[0..4] != 11_u32.to_be_bytes()
        || &blob[4..15] != b"ssh-ed25519"
        || blob[15..19] != 32_u32.to_be_bytes()
    {
        return Err(bad("decoded key blob does not carry an ssh-ed25519 header"));
    }
    let mut pk_bytes = [0_u8; 32];
    pk_bytes.copy_from_slice(&blob[19..51]);
    let verifying = VerifyingKey::from_bytes(&pk_bytes)
        .map_err(|_| bad("key payload is not a valid Ed25519 public point"))?;
    if verifying.is_weak() {
        return Err(bad("key payload is a weak/small-order Ed25519 point"));
    }

    let canonical_b64 = STANDARD.encode(&blob);
    if key_b64 != canonical_b64 {
        return Err(bad("key base64 encoding is not canonical"));
    }

    if let Some(comment) = comment {
        if comment.is_empty() {
            return Err(bad("unexpected extra space after key data"));
        }
        for token in comment.split(' ') {
            if looks_like_second_key_token(token) {
                return Err(bad("comment appears to contain a second SSH key"));
            }
        }
    }

    let mut wire_blob = [0_u8; ED25519_WIRE_BLOB_LEN];
    wire_blob.copy_from_slice(&blob);

    Ok(ValidatedEd25519Key {
        canonical: format!("ssh-ed25519 {canonical_b64}"),
        key_b64: canonical_b64,
        wire_blob,
        fingerprint: fingerprint_blob(&blob),
    })
}

fn looks_like_second_key_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("ssh-")
        || lower.starts_with("ecdsa-sha2-")
        || lower.starts_with("sk-ssh-")
        || lower.starts_with("sk-ecdsa-")
    {
        return true;
    }
    // Reject tokens that look like an SSH wire-format base64 key blob (`AAAA...` >= 32 chars).
    token.len() >= 32
        && token.starts_with("AAAA")
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

/// Resolved SSH public host keys together with where they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHostKeys {
    /// Ordered public host keys (`"<type> <base64>"`).
    pub keys: Vec<String>,
    /// Source label: `"configured"`, `"ssh-keyscan+files"`, `"ssh-keyscan only"`, or `"files"`.
    pub source: String,
}

/// Per-node in-memory cache of auto-discovered sshd public host keys, keyed by
/// `(node_config_dir, port)` with a [`HOST_KEY_CACHE_TTL`] expiry.
#[derive(Debug, Default)]
pub(crate) struct HostKeyScanCache {
    entries: Mutex<HashMap<(PathBuf, u16), (Instant, ResolvedHostKeys)>>,
}

impl HostKeyScanCache {
    pub(crate) fn get_at(
        &self,
        node_key: &Path,
        port: u16,
        now: Instant,
    ) -> Option<ResolvedHostKeys> {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (node_key.to_path_buf(), port);
        if let Some((stored_at, resolved)) = guard.get(&key) {
            if now.saturating_duration_since(*stored_at) < HOST_KEY_CACHE_TTL {
                return Some(resolved.clone());
            }
            guard.remove(&key);
        }
        None
    }

    pub(crate) fn insert_at(
        &self,
        node_key: &Path,
        port: u16,
        now: Instant,
        resolved: ResolvedHostKeys,
    ) {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|_, (stored_at, _)| {
            now.saturating_duration_since(*stored_at) < HOST_KEY_CACHE_TTL
        });
        guard.insert((node_key.to_path_buf(), port), (now, resolved));
    }
}

fn global_host_key_cache() -> &'static HostKeyScanCache {
    static CACHE: OnceLock<HostKeyScanCache> = OnceLock::new();
    CACHE.get_or_init(HostKeyScanCache::default)
}

/// Like [`resolve_host_keys`], but caches auto-discovered (`ssh-keyscan` / default-file) results
/// per `(node_key, port)` for [`HOST_KEY_CACHE_TTL`] so repeated read-only `device.endpoint` calls
/// do not repeatedly invoke `ssh-keyscan`. Configured `.pub` files are always read directly.
pub async fn resolve_host_keys_cached(
    node_key: &Path,
    configured: &[String],
    home: Option<&Path>,
    port: u16,
    cancel: CancellationToken,
) -> Result<ResolvedHostKeys, ErrorBody> {
    if !configured.is_empty() {
        return resolve_host_keys(configured, home, port, cancel).await;
    }
    let cache = global_host_key_cache();
    let now = Instant::now();
    if let Some(cached) = cache.get_at(node_key, port, now) {
        return Ok(cached);
    }
    let resolved = resolve_host_keys(configured, home, port, cancel).await?;
    cache.insert_at(node_key, port, Instant::now(), resolved.clone());
    Ok(resolved)
}

/// Resolves this node's sshd public host keys for an invite on `port`.
///
/// - When `configured` (`[devices] host_keys` in `node.toml`) is non-empty, reads those `.pub`
///   files directly (`source = "configured"`).
/// - Otherwise probes the live local `sshd` via `ssh-keyscan -p <port> -T 5 127.0.0.1` (and `::1`
///   if IPv4 yields no keys) and also reads the default host key `.pub` files
///   (`/etc/ssh/ssh_host_*_key.pub` on Unix, `%ProgramData%\ssh\ssh_host_*_key.pub` on Windows):
///   - When both sources return keys, pins only keys present in both (`source =
///     "ssh-keyscan+files"`); if the intersection is empty, refuses so a non-privileged listener on
///     a loopback port cannot substitute its own host key, and hints to set `[devices] host_keys`.
///   - Uses `ssh-keyscan` alone (`source = "ssh-keyscan only"`) only when no default host-key files
///     can be read.
///   - Uses the default host-key files alone (`source = "files"`) when `ssh-keyscan` yields no
///     keys.
pub async fn resolve_host_keys(
    configured: &[String],
    home: Option<&Path>,
    port: u16,
    cancel: CancellationToken,
) -> Result<ResolvedHostKeys, ErrorBody> {
    if !configured.is_empty() {
        let keys = read_host_keys(configured, home)?;
        return Ok(ResolvedHostKeys {
            keys,
            source: "configured".to_owned(),
        });
    }

    let scanned = scan_local_sshd_host_keys(port, cancel).await;
    let (file_keys, pattern, read_errors) = read_default_host_key_files();
    if let Some(resolved) = choose_auto_host_keys(scanned, file_keys)? {
        return Ok(resolved);
    }

    let detail = if !read_errors.is_empty() {
        format!(" ({})", read_errors.join("; "))
    } else {
        String::new()
    };
    Err(ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "No usable sshd public host keys could be obtained from ssh-keyscan on port {port} or {pattern}{detail}."
        ),
    )
    .with_hint(
        "An invite without a pinned host key is not allowed. Ensure sshd is running on that port or its host `.pub` files are readable (for example `ssh-keygen -A` on Unix), or list the `.pub` files under `[devices] host_keys` in node.toml.",
    ))
}

/// Selects auto-discovered host keys from live `ssh-keyscan` keys and default host-key `.pub`
/// files.
///
/// Both inputs should contain all valid keys from each source (before ed25519/ecdsa/rsa priority
/// filtering) so any overlapping key is preserved. When both sources are non-empty, only their
/// intersection is pinned; an empty intersection returns `Err(ErrorCode::Denied)`.
pub fn choose_auto_host_keys(
    scanned: Vec<String>,
    file_keys: Vec<String>,
) -> Result<Option<ResolvedHostKeys>, ErrorBody> {
    match (scanned.is_empty(), file_keys.is_empty()) {
        (false, false) => {
            let file_set: BTreeSet<&str> = file_keys.iter().map(String::as_str).collect();
            let common: Vec<&str> = scanned
                .iter()
                .map(String::as_str)
                .filter(|k| file_set.contains(k))
                .collect();
            let selected = select_host_keys(&common);
            if selected.is_empty() {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "The host keys returned by ssh-keyscan do not match any host key in the default sshd host-key files.",
                )
                .with_hint(
                    "If sshd uses custom host keys on this port, pin the intended `.pub` file(s) explicitly under `[devices] host_keys` in node.toml.",
                ));
            }
            Ok(Some(ResolvedHostKeys {
                keys: selected,
                source: "ssh-keyscan+files".to_owned(),
            }))
        }
        (false, true) => {
            let refs: Vec<&str> = scanned.iter().map(String::as_str).collect();
            let selected = select_host_keys(&refs);
            if selected.is_empty() {
                return Ok(None);
            }
            Ok(Some(ResolvedHostKeys {
                keys: selected,
                source: "ssh-keyscan only".to_owned(),
            }))
        }
        (true, false) => {
            let refs: Vec<&str> = file_keys.iter().map(String::as_str).collect();
            let selected = select_host_keys(&refs);
            if selected.is_empty() {
                return Ok(None);
            }
            Ok(Some(ResolvedHostKeys {
                keys: selected,
                source: "files".to_owned(),
            }))
        }
        (true, true) => Ok(None),
    }
}

async fn scan_local_sshd_host_keys(port: u16, cancel: CancellationToken) -> Vec<String> {
    let port_str = port.to_string();
    for host in ["127.0.0.1", "::1"] {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let options = RunOptions::new(CommandSpec::argv(
            "ssh-keyscan",
            ["-p", port_str.as_str(), "-T", "5", host],
        ))
        .c_locale()
        .timeout(KEYSCAN_TIMEOUT)
        .cancel(cancel.child_token());

        if let Ok(output) = run(options).await
            && !output.cancelled
            && !output.timed_out
        {
            let parsed = parse_keyscan_all_keys(&output.stdout.text);
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    Vec::new()
}

fn parse_keyscan_all_keys(stdout: &str) -> Vec<String> {
    let stripped: Vec<&str> = stdout
        .lines()
        .filter_map(|raw_line| {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (_, rest) = line.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            (!rest.is_empty()).then_some(rest)
        })
        .collect();
    parse_all_valid_host_keys(&stripped)
}

/// Parses `ssh-keyscan` stdout lines (`<host> <type> <base64>`) into ordered, validated host keys.
pub fn parse_keyscan_output(stdout: &str) -> Vec<String> {
    let all = parse_keyscan_all_keys(stdout);
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    select_host_keys(&refs)
}

fn read_default_host_key_files() -> (Vec<String>, String, Vec<String>) {
    let dir = default_ssh_host_key_dir();
    let pattern = if cfg!(windows) {
        format!("{}\\ssh_host_*_key.pub", dir.display())
    } else {
        format!("{}/ssh_host_*_key.pub", dir.display())
    };
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("ssh_host_") && name_str.ends_with("_key.pub") {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    let mut texts = Vec::new();
    let mut read_errors = Vec::new();
    for path in &found {
        match std::fs::read_to_string(path) {
            Ok(text) => texts.push(text),
            Err(error) => read_errors.push(format!("{}: {error}", path.display())),
        }
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    (parse_all_valid_host_keys(&refs), pattern, read_errors)
}

/// Reads and orders this node's sshd public host keys from `.pub` files.
///
/// When `configured` is non-empty (`[devices] host_keys` in `node.toml`), reads those files;
/// otherwise reads `/etc/ssh/ssh_host_*_key.pub` on Unix and `%ProgramData%\ssh\ssh_host_*_key.pub`
/// on Windows.
///
/// Orders `ssh-ed25519` first, then `ecdsa-sha2-*`, and includes `ssh-rsa` only when neither
/// ed25519 nor ecdsa is present. Fails if no usable host key can be read: an invite without a
/// pinned host key is never issued.
pub fn read_host_keys(
    configured: &[String],
    home: Option<&Path>,
) -> Result<Vec<String>, ErrorBody> {
    let (paths, source_label) = if !configured.is_empty() {
        let mut resolved = Vec::with_capacity(configured.len());
        for raw in configured {
            resolved.push(expand_tilde(Path::new(raw), home)?);
        }
        (resolved, "[devices] host_keys in node.toml".to_owned())
    } else {
        let dir = default_ssh_host_key_dir();
        let pattern = if cfg!(windows) {
            format!("{}\\ssh_host_*_key.pub", dir.display())
        } else {
            format!("{}/ssh_host_*_key.pub", dir.display())
        };
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("ssh_host_") && name_str.ends_with("_key.pub") {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        (found, pattern)
    };

    let mut texts = Vec::new();
    let mut read_errors = Vec::new();
    for path in &paths {
        match std::fs::read_to_string(path) {
            Ok(text) => texts.push(text),
            Err(error) => read_errors.push(format!("{}: {error}", path.display())),
        }
    }

    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let selected = select_host_keys(&refs);
    if selected.is_empty() {
        let detail = if !read_errors.is_empty() {
            format!(" ({})", read_errors.join("; "))
        } else {
            String::new()
        };
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("No usable sshd public host keys could be read from {source_label}{detail}."),
        )
        .with_hint(
            "An invite without a pinned host key is not allowed. Ensure sshd host keys exist and are readable by this account (for example `ssh-keygen -A` on Unix), or list the `.pub` files under `[devices] host_keys` in node.toml.",
        ));
    }
    Ok(selected)
}

fn default_ssh_host_key_dir() -> PathBuf {
    if let Some(program_data) = legion_os::user::windows_program_data_dir() {
        program_data.join("ssh")
    } else {
        PathBuf::from("/etc/ssh")
    }
}

/// Extracts all deduplicated, wire-validated SSH public host keys (`ssh-ed25519`, `ecdsa-sha2-*`,
/// and `ssh-rsa`) from raw `.pub` contents, without dropping RSA when stronger keys exist.
fn parse_all_valid_host_keys(contents: &[&str]) -> Vec<String> {
    let mut all = BTreeSet::new();
    for text in contents {
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(key_type), Some(key_b64)) = (parts.next(), parts.next()) else {
                continue;
            };
            if !matches!(
                key_type,
                "ssh-ed25519"
                    | "ecdsa-sha2-nistp256"
                    | "ecdsa-sha2-nistp384"
                    | "ecdsa-sha2-nistp521"
                    | "ssh-rsa"
            ) {
                continue;
            }
            if !valid_host_key_blob(key_type, key_b64) {
                continue;
            }
            all.insert(format!("{key_type} {key_b64}"));
        }
    }
    all.into_iter().collect()
}

/// Parses and orders SSH public host keys from raw `.pub` file contents.
///
/// Order: `ssh-ed25519` first, then `ecdsa-sha2-*`; `ssh-rsa` is included only if neither ed25519
/// nor ecdsa exists. Comments are stripped and keys are deduplicated.
pub fn select_host_keys(contents: &[&str]) -> Vec<String> {
    let mut ed25519 = BTreeSet::new();
    let mut ecdsa = BTreeSet::new();
    let mut rsa = BTreeSet::new();

    for text in contents {
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(key_type), Some(key_b64)) = (parts.next(), parts.next()) else {
                continue;
            };
            if !valid_host_key_blob(key_type, key_b64) {
                continue;
            }
            let formatted = format!("{key_type} {key_b64}");
            match key_type {
                "ssh-ed25519" => {
                    ed25519.insert(formatted);
                }
                "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521" => {
                    ecdsa.insert(formatted);
                }
                "ssh-rsa" => {
                    rsa.insert(formatted);
                }
                _ => {}
            }
        }
    }

    let mut out = Vec::new();
    out.extend(ed25519);
    out.extend(ecdsa);
    if out.is_empty() {
        out.extend(rsa);
    }
    out
}

/// Checks that `key_b64` decodes as standard base64 whose leading SSH wire string matches
/// `key_type`.
fn valid_host_key_blob(key_type: &str, key_b64: &str) -> bool {
    let Ok(blob) = STANDARD.decode(key_b64) else {
        return false;
    };
    if blob.len() < 4 + key_type.len() {
        return false;
    }
    let declared_len = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    declared_len == key_type.len() && &blob[4..4 + declared_len] == key_type.as_bytes()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fake_ssh_blob(key_type: &str, payload: &[u8]) -> String {
        let mut blob = Vec::new();
        blob.extend_from_slice(&(key_type.len() as u32).to_be_bytes());
        blob.extend_from_slice(key_type.as_bytes());
        blob.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        blob.extend_from_slice(payload);
        format!("{key_type} {}", STANDARD.encode(blob))
    }

    #[test]
    fn ephemeral_key_passes_strict_validation_and_invite_round_trips() {
        let eph = generate_ephemeral_ed25519().unwrap();
        let validated = validate_ed25519_public_key(&eph.public_key).unwrap();
        assert_eq!(validated.canonical, eph.public_key);
        assert_eq!(validated.fingerprint, eph.fingerprint);
        assert!(validated.fingerprint.starts_with("SHA256:"));
        assert_eq!(validated.fingerprint.len(), "SHA256:".len() + 43);

        // Trailing comment is accepted and discarded from canonical output.
        let with_comment = format!("{} phone@example", eph.public_key);
        let stripped = validate_ed25519_public_key(&with_comment).unwrap();
        assert_eq!(stripped.canonical, eph.public_key);

        let payload = InvitePayload {
            v: 1,
            id: generate_invite_id().unwrap(),
            node: "tower".to_owned(),
            hosts: vec!["tower.example".to_owned(), "100.64.0.1".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![eph.public_key],
            key: eph.seed_b64,
            principal: "phone".to_owned(),
            expires: "2026-11-24T12:15:00Z".to_owned(),
        };
        let encoded = payload.encode().unwrap();
        assert!(encoded.starts_with("legion-invite-1."));
        assert!(!encoded.contains('='));
        assert_eq!(InvitePayload::decode(&encoded).unwrap(), payload);
    }

    #[test]
    fn hostile_and_malformed_public_keys_are_refused() {
        let valid1 = generate_ephemeral_ed25519().unwrap().public_key;
        let valid2 = generate_ephemeral_ed25519().unwrap().public_key;
        let valid_b64 = valid1.split_once(' ').unwrap().1;

        // Weak identity/small-order point (all-zero compressed point in Edwards coordinates is
        // order 4; (1, 0) = [1, 0, ...] is the identity point of order 1).
        let mut identity = [0_u8; 32];
        identity[0] = 1;
        let weak_identity = format!(
            "ssh-ed25519 {}",
            STANDARD.encode(encode_ed25519_wire_blob(&identity))
        );

        let rsa = fake_ssh_blob("ssh-rsa", &[1, 2, 3, 4]);
        let wrong_len = format!(
            "ssh-ed25519 {}",
            STANDARD.encode([
                0, 0, 0, 11, b's', b's', b'h', b'-', b'e', b'd', b'2', b'5', b'5', b'1', b'9'
            ])
        );
        let long_comment = format!("{valid1} {}", "a".repeat(600));

        let hostile = [
            String::new(),
            "ssh-ed25519".to_owned(),
            format!(" {valid1}"),
            format!("{valid1} "),
            format!("{valid1}\n{valid2}"),
            format!("{valid1}\r\n"),
            format!("{valid1}\0extra"),
            format!("{valid1}\tcomment"),
            format!("restrict {valid1}"),
            format!("command=\"id\" {valid1}"),
            format!("{valid1} \"quoted\""),
            format!("{valid1} {valid2}"),
            format!("{valid1} ssh-rsa AAAA"),
            format!("{valid1} {valid_b64}"),
            format!("ssh-ed25519  {valid_b64}"),
            format!("ssh-ed25519 {}=", &valid_b64[..valid_b64.len() - 1]),
            "ssh-ed25519 not-valid-base64!!!".to_owned(),
            rsa,
            wrong_len,
            weak_identity,
            long_comment,
        ];
        for case in &hostile {
            let err = validate_ed25519_public_key(case).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "accepted {case:?}");
        }
    }

    #[test]
    fn host_key_selection_prefers_ed25519_then_ecdsa_and_uses_rsa_only_as_fallback() {
        let ed = generate_ephemeral_ed25519().unwrap().public_key;
        let ecdsa = fake_ssh_blob("ecdsa-sha2-nistp256", &[1; 65]);
        let rsa = fake_ssh_blob("ssh-rsa", &[2; 64]);
        let pq = fake_ssh_blob("ssh-mldsa44-ed25519", &[3; 64]);

        let ed_comment = format!("{ed} root@tower");
        let ecdsa_comment = format!("{ecdsa} root@tower");
        let rsa_comment = format!("{rsa} root@tower");

        // ed25519 + ecdsa + rsa + unknown -> ed25519 first, then ecdsa; rsa and unknown skipped.
        let picked = select_host_keys(&[&rsa_comment, &pq, &ecdsa_comment, &ed_comment]);
        assert_eq!(picked, vec![ed.clone(), ecdsa.clone()]);

        // Comments are stripped and duplicates removed.
        let dedup = select_host_keys(&[&ed_comment, &ed]);
        assert_eq!(dedup, vec![ed]);

        // Only rsa -> rsa is returned.
        let rsa_only = select_host_keys(&[&rsa_comment, &pq]);
        assert_eq!(rsa_only, vec![rsa]);

        // Nothing usable -> empty.
        assert!(select_host_keys(&["# comment only\n", &pq, "not a key"]).is_empty());
    }

    #[test]
    fn parses_keyscan_output_and_intersects_with_host_key_files() {
        let ed = generate_ephemeral_ed25519().unwrap().public_key;
        let ed_other = generate_ephemeral_ed25519().unwrap().public_key;
        let ecdsa = fake_ssh_blob("ecdsa-sha2-nistp256", &[1; 65]);
        let ecdsa_other = fake_ssh_blob("ecdsa-sha2-nistp256", &[9; 65]);
        let rsa = fake_ssh_blob("ssh-rsa", &[2; 64]);

        let scan_stdout = format!(
            "# 127.0.0.1:2222 SSH-2.0-OpenSSH_10.6\n\
             [127.0.0.1]:2222 {rsa}\n\
             [127.0.0.1]:2222 {ecdsa}\n\
             [127.0.0.1]:2222 {ed}\n\
             [::1]:2222 {ed}\n"
        );
        let parsed = parse_keyscan_output(&scan_stdout);
        assert_eq!(parsed, vec![ed.clone(), ecdsa.clone()]);

        // Both sources present and matching -> pins intersection with source "ssh-keyscan+files".
        let matched = choose_auto_host_keys(parsed.clone(), parsed.clone())
            .unwrap()
            .unwrap();
        assert_eq!(matched.keys, parsed);
        assert_eq!(matched.source, "ssh-keyscan+files");

        // Partial overlap -> pins only the keys present in both sources.
        let partial = choose_auto_host_keys(
            vec![ed.clone(), ecdsa.clone()],
            vec![ed.clone(), ecdsa_other],
        )
        .unwrap()
        .unwrap();
        assert_eq!(partial.keys, vec![ed.clone()]);
        assert_eq!(partial.source, "ssh-keyscan+files");

        // Intersection happens before RSA fallback drops RSA, so a shared RSA key is still pinned.
        let rsa_overlap = choose_auto_host_keys(
            vec![ed.clone(), rsa.clone()],
            vec![ed_other.clone(), rsa.clone()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(rsa_overlap.keys, vec![rsa]);
        assert_eq!(rsa_overlap.source, "ssh-keyscan+files");

        // Disjoint scan vs host-key files -> refused with Denied and [devices] host_keys hint.
        let disjoint_err = choose_auto_host_keys(vec![ed.clone()], vec![ed_other]).unwrap_err();
        assert_eq!(disjoint_err.code, ErrorCode::Denied);
        assert!(
            disjoint_err
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("[devices] host_keys")),
            "{:?}",
            disjoint_err.hint
        );

        // Scan alone only when no default host-key files could be read.
        let scan_only = choose_auto_host_keys(vec![ed.clone()], Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(scan_only.keys, vec![ed.clone()]);
        assert_eq!(scan_only.source, "ssh-keyscan only");

        // Files alone when scan yields nothing.
        let files_only = choose_auto_host_keys(Vec::new(), vec![ed.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(files_only.keys, vec![ed]);
        assert_eq!(files_only.source, "files");

        // Neither available -> Ok(None).
        assert!(
            choose_auto_host_keys(Vec::new(), Vec::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn host_key_scan_cache_reuses_entries_within_ttl_and_expires_after_one_minute() {
        let cache = HostKeyScanCache::default();
        let node_a = Path::new("/tmp/node-a/config");
        let node_b = Path::new("/tmp/node-b/config");
        let t0 = Instant::now();
        let resolved_a = ResolvedHostKeys {
            keys: vec![ed25519_public_key_from_seed(&[1u8; 32])],
            source: "ssh-keyscan+files".to_owned(),
        };
        let resolved_b = ResolvedHostKeys {
            keys: vec![ed25519_public_key_from_seed(&[2u8; 32])],
            source: "files".to_owned(),
        };

        assert!(cache.get_at(node_a, 22, t0).is_none());
        cache.insert_at(node_a, 22, t0, resolved_a.clone());
        cache.insert_at(node_b, 22, t0, resolved_b.clone());

        // Within the 1-minute TTL, same (node, port) hits; different port misses; nodes are isolated.
        let t_hit = t0 + Duration::from_secs(59);
        assert_eq!(cache.get_at(node_a, 22, t_hit), Some(resolved_a));
        assert_eq!(cache.get_at(node_b, 22, t_hit), Some(resolved_b));
        assert!(cache.get_at(node_a, 2222, t_hit).is_none());

        // At/after HOST_KEY_CACHE_TTL (60 seconds), the cached scan expires.
        let t_expired = t0 + HOST_KEY_CACHE_TTL;
        assert!(cache.get_at(node_a, 22, t_expired).is_none());
        assert!(cache.get_at(node_b, 22, t_expired).is_none());
    }
}
