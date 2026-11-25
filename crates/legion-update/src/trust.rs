//! Trusted Ed25519 keys compiled into Legion and strict signature verification over `SHA256SUMS`.
//!
//! Every build trusts [`RELEASE_PUBLIC_KEY`] for `stable` updates. A build trusts a preview key
//! ([`PREVIEW_PUBLIC_KEY`]) only when `LEGION_PREVIEW_UPDATE_KEY` was supplied at compile time.
//! There is no runtime flag, config file, or environment variable that weakens or overrides this
//! trust anchor.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD as BASE64_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::Channel;

/// Name of the checksum manifest in every signed release and update set.
pub const CHECKSUMS_FILENAME: &str = "SHA256SUMS";

/// Detached Ed25519 signature filename paired with `SHA256SUMS`.
pub const SIGNATURE_FILENAME: &str = "SHA256SUMS.sig";

/// Metadata JSON filename inside every update set.
pub const MANIFEST_FILENAME: &str = "update.json";

/// Raw 32-byte Ed25519 release public key from `contract/release-public-key.pem`.
///
/// SPKI PEM: `MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=`.
/// Deliberately compiled in with no runtime override.
pub const RELEASE_PUBLIC_KEY: [u8; 32] = [
    0x07, 0xca, 0xb5, 0x0c, 0xd6, 0xa6, 0x14, 0x5d, 0x12, 0x84, 0xc8, 0xba, 0x5b, 0x3a, 0x6c, 0xff,
    0x45, 0x08, 0x18, 0x49, 0x0b, 0x98, 0x70, 0xc3, 0xbd, 0x0a, 0x8e, 0xbe, 0xeb, 0xfd, 0xf3, 0x38,
];

include!(concat!(env!("OUT_DIR"), "/preview_key.rs"));

static PREVIEW_KEY_MARKER_BYTES: &[u8] = PREVIEW_KEY_MARKER_STR.as_bytes();

/// NUL-terminated static marker compiled into binaries indicating whether a preview update signing
/// key was compiled in (`LEGION_UPDATE_PREVIEW_KEY=none\0` for official release/CI builds, or
/// `LEGION_UPDATE_PREVIEW_KEY=SHA256:<fingerprint>\0` when `LEGION_PREVIEW_UPDATE_KEY` was set).
pub fn compiled_preview_key_marker() -> &'static [u8] {
    PREVIEW_KEY_MARKER_BYTES
}

/// Which compiled-in key signed an update set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SignerKind {
    /// The offline release key (`contract/release-public-key.pem`), trusted for `stable`.
    Release,
    /// The preview key compiled in via `LEGION_PREVIEW_UPDATE_KEY`, trusted for `preview`.
    Preview,
}

impl SignerKind {
    /// Lowercase wire name (`"release"` or `"preview"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Preview => "preview",
        }
    }

    /// Channel that this key is authorised to sign.
    pub const fn channel(self) -> Channel {
        match self {
            Self::Release => Channel::Stable,
            Self::Preview => Channel::Preview,
        }
    }
}

impl std::fmt::Display for SignerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of verifying a detached Ed25519 signature over `SHA256SUMS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedSignature {
    /// Which trusted key matched the signature.
    pub signer: SignerKind,
    /// Stable SHA-256 fingerprint of the trusted public key (`SHA256:<base64>`).
    pub key_fingerprint: String,
}

/// Set of public keys trusted by a running Legion binary.
///
/// Production code always calls [`TrustAnchors::compiled_in`]. [`TrustAnchors::with_keys`] exists
/// solely so cross-crate tests can exercise verification with ephemeral keypairs without adding any
/// runtime override to tools or configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrustAnchors {
    release: [u8; 32],
    preview: Option<[u8; 32]>,
}

impl TrustAnchors {
    /// Returns the compiled-in trust anchors for this build.
    pub const fn compiled_in() -> Self {
        Self {
            release: RELEASE_PUBLIC_KEY,
            preview: PREVIEW_PUBLIC_KEY,
        }
    }

    /// Constructs trust anchors with explicit keys for tests.
    #[doc(hidden)]
    pub const fn with_keys(release: [u8; 32], preview: Option<[u8; 32]>) -> Self {
        Self { release, preview }
    }

    /// Whether this build trusts a preview update key.
    pub const fn has_preview(&self) -> bool {
        self.preview.is_some()
    }

    /// Returns the release public key bytes.
    pub const fn release_key(&self) -> &[u8; 32] {
        &self.release
    }

    /// Returns the optional preview public key bytes.
    pub const fn preview_key(&self) -> Option<&[u8; 32]> {
        self.preview.as_ref()
    }

    /// Returns the public key bytes for `kind`, if compiled in.
    pub const fn key_for(&self, kind: SignerKind) -> Option<&[u8; 32]> {
        match kind {
            SignerKind::Release => Some(&self.release),
            SignerKind::Preview => self.preview.as_ref(),
        }
    }

    /// Returns the `SHA256:<base64>` fingerprint of the release key.
    pub fn release_fingerprint(&self) -> String {
        key_fingerprint(&self.release)
    }

    /// Returns the `SHA256:<base64>` fingerprint of the compiled-in preview key, if any.
    pub fn preview_fingerprint(&self) -> Option<String> {
        self.preview.as_ref().map(key_fingerprint)
    }

    /// Verifies `sig_text` over `sums_bytes` against any trusted key in this set.
    ///
    /// Returns `ErrorCode::Denied` if the signature is malformed or does not match any trusted key.
    pub fn verify_any(
        &self,
        sums_bytes: &[u8],
        sig_text: &str,
    ) -> Result<VerifiedSignature, ErrorBody> {
        let signature = parse_signature(sig_text)?;

        let release_key = parse_verifying_key(&self.release, "release")?;
        if release_key.verify_strict(sums_bytes, &signature).is_ok() {
            return Ok(VerifiedSignature {
                signer: SignerKind::Release,
                key_fingerprint: key_fingerprint(&self.release),
            });
        }

        if let Some(preview_bytes) = &self.preview {
            let preview_key = parse_verifying_key(preview_bytes, "preview")?;
            if preview_key.verify_strict(sums_bytes, &signature).is_ok() {
                return Ok(VerifiedSignature {
                    signer: SignerKind::Preview,
                    key_fingerprint: key_fingerprint(preview_bytes),
                });
            }
        }

        let hint = if self.preview.is_some() {
            "SHA256SUMS or SHA256SUMS.sig may have been modified or signed by an untrusted key; nothing was accepted."
        } else {
            "This build only trusts official release signatures; preview sets require a build compiled with LEGION_PREVIEW_UPDATE_KEY."
        };
        Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing update set: {SIGNATURE_FILENAME} does not match any Ed25519 update key trusted by this build."
            ),
        )
        .with_hint(hint))
    }

    /// Verifies `sig_text` over `sums_bytes` specifically for `channel`.
    ///
    /// Enforces channel-key binding: `stable` sets must be signed by the release key, and `preview`
    /// sets must be signed by the compiled-in preview key.
    pub fn verify_for_channel(
        &self,
        channel: Channel,
        sums_bytes: &[u8],
        sig_text: &str,
    ) -> Result<VerifiedSignature, ErrorBody> {
        let signature = parse_signature(sig_text)?;
        match channel {
            Channel::Stable => {
                let key = parse_verifying_key(&self.release, "release")?;
                key.verify_strict(sums_bytes, &signature).map_err(|_| {
                    ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing stable update set: {SIGNATURE_FILENAME} does not match the compiled-in Ed25519 release key."
                        ),
                    )
                    .with_hint(
                        "SHA256SUMS or SHA256SUMS.sig may have been modified or signed by an untrusted key; nothing was accepted.",
                    )
                })?;
                Ok(VerifiedSignature {
                    signer: SignerKind::Release,
                    key_fingerprint: key_fingerprint(&self.release),
                })
            }
            Channel::Preview => {
                let Some(preview_bytes) = &self.preview else {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        "Refusing preview update set: this build does not trust a preview update key.",
                    )
                    .with_hint(
                        "Official builds only accept stable releases; compile with LEGION_PREVIEW_UPDATE_KEY to follow previews.",
                    ));
                };
                let key = parse_verifying_key(preview_bytes, "preview")?;
                key.verify_strict(sums_bytes, &signature).map_err(|_| {
                    ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing preview update set: {SIGNATURE_FILENAME} does not match the compiled-in Ed25519 preview key."
                        ),
                    )
                    .with_hint(
                        "SHA256SUMS or SHA256SUMS.sig may have been modified or signed by another preview key; nothing was accepted.",
                    )
                })?;
                Ok(VerifiedSignature {
                    signer: SignerKind::Preview,
                    key_fingerprint: key_fingerprint(preview_bytes),
                })
            }
        }
    }
}

/// Computes a stable human-readable SHA-256 fingerprint (`SHA256:<base64>`) for a 32-byte Ed25519
/// public key.
pub fn key_fingerprint(public_key_bytes: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key_bytes);
    format!("SHA256:{}", BASE64_NO_PAD.encode(digest))
}

/// Verifies `sig_text` (standard base64 of the 64-byte Ed25519 signature, with or without trailing
/// whitespace) over the exact bytes of `SHA256SUMS` using the compiled-in [`RELEASE_PUBLIC_KEY`].
///
/// Returns `ErrorCode::Denied` when the signature is malformed or does not match the trusted key.
pub fn verify_release_signature(sums_bytes: &[u8], sig_text: &str) -> Result<(), ErrorBody> {
    verify_signature_with_key(sums_bytes, sig_text, &RELEASE_PUBLIC_KEY)
}

/// Verifies a detached Ed25519 signature against an explicit 32-byte public key.
///
/// Production code always calls [`verify_release_signature`] or [`TrustAnchors::compiled_in`]; this
/// entry point exists so unit tests across crates can sign manifests with an ephemeral keypair
/// without ever adding a runtime override to any tool or config.
#[doc(hidden)]
pub fn verify_signature_with_key(
    sums_bytes: &[u8],
    sig_text: &str,
    public_key_bytes: &[u8; 32],
) -> Result<(), ErrorBody> {
    let signature = parse_signature(sig_text)?;
    let key = parse_verifying_key(public_key_bytes, "release")?;

    // Strict verification rejects small-order and non-canonical signatures in addition to
    // mismatches, ensuring one canonical signature representation.
    key.verify_strict(sums_bytes, &signature).map_err(|_| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing release: {SIGNATURE_FILENAME} does not match the Ed25519 release key compiled into legion."
            ),
        )
        .with_hint(
            "SHA256SUMS or SHA256SUMS.sig may have been modified or signed by an untrusted key; nothing was installed.",
        )
    })
}

fn parse_signature(sig_text: &str) -> Result<Signature, ErrorBody> {
    let encoded = sig_text.trim();
    if encoded.len() != 88
        || !encoded.ends_with("==")
        || !encoded[..86]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing release: {SIGNATURE_FILENAME} is not a valid 64-byte base64 Ed25519 signature."
            ),
        )
        .with_hint(
            "Only offline-signed releases carrying a valid SHA256SUMS.sig are accepted; to install an unsigned local build, pass `from=<local path>`.",
        ));
    }

    let raw = BASE64.decode(encoded).map_err(|_| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!("Refusing release: {SIGNATURE_FILENAME} is not valid base64."),
        )
        .with_hint("Verify that SHA256SUMS.sig was uploaded intact to the release.")
    })?;

    let sig_bytes: [u8; 64] = raw.try_into().map_err(|_| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!("Refusing release: {SIGNATURE_FILENAME} did not decode to 64 bytes."),
        )
        .with_hint("Verify that SHA256SUMS.sig was created by scripts/sign-legion-release.mjs.")
    })?;

    Ok(Signature::from_bytes(&sig_bytes))
}

fn parse_verifying_key(
    public_key_bytes: &[u8; 32],
    label: &str,
) -> Result<VerifyingKey, ErrorBody> {
    VerifyingKey::from_bytes(public_key_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Trusted {label} public key is invalid: {err}."),
        )
    })
}

/// Formats a raw 64-byte Ed25519 signature into the canonical `.sig` text format (base64 + `\n`).
#[doc(hidden)]
pub fn encode_signature_bytes(sig_bytes: &[u8; 64]) -> String {
    format!("{}\n", BASE64.encode(sig_bytes))
}
