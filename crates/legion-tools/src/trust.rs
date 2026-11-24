//! The single release signing key Legion trusts, compiled into the binary.
//!
//! Every release manifest (`SHA256SUMS`) fetched from GitHub or read from the dist cache must carry
//! a valid detached Ed25519 signature (`SHA256SUMS.sig`) made offline by this key. There is
//! deliberately no environment variable, configuration file, or CLI flag to override or disable
//! this key at run time: anyone who can edit a GitHub release or a machine's config must never be
//! able to swap the trust anchor that governs replacing executable code.
//!
//! The public key bytes below are the raw 32-byte Ed25519 key from
//! `contract/release-public-key.pem` (the exact same key pinned in `agent/src/trust.mjs`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use legion_core::proto::{ErrorBody, ErrorCode};

/// Detached signature filename paired with `SHA256SUMS` on every signed Legion release.
pub const SIGNATURE_FILENAME: &str = "SHA256SUMS.sig";

/// Raw 32-byte Ed25519 public key from `contract/release-public-key.pem` / `agent/src/trust.mjs`.
///
/// SPKI PEM: `MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=`.
/// Deliberately compiled in with no runtime override.
pub const RELEASE_PUBLIC_KEY: [u8; 32] = [
    0x07, 0xca, 0xb5, 0x0c, 0xd6, 0xa6, 0x14, 0x5d, 0x12, 0x84, 0xc8, 0xba, 0x5b, 0x3a, 0x6c, 0xff,
    0x45, 0x08, 0x18, 0x49, 0x0b, 0x98, 0x70, 0xc3, 0xbd, 0x0a, 0x8e, 0xbe, 0xeb, 0xfd, 0xf3, 0x38,
];

/// Verifies `sig_text` (standard base64 of the 64-byte Ed25519 signature, with or without trailing
/// whitespace) over the exact bytes of `SHA256SUMS` using the compiled-in [`RELEASE_PUBLIC_KEY`].
///
/// Returns `ErrorCode::Denied` when the signature is malformed or does not match the trusted key.
pub fn verify_release_signature(sums_bytes: &[u8], sig_text: &str) -> Result<(), ErrorBody> {
    verify_signature_with_key(sums_bytes, sig_text, &RELEASE_PUBLIC_KEY)
}

/// Verifies a detached Ed25519 signature against an explicit 32-byte public key.
///
/// Production code always calls [`verify_release_signature`] with [`RELEASE_PUBLIC_KEY`]; this
/// entry point exists so unit tests across crates can sign manifests with an ephemeral keypair
/// without ever adding a runtime override to any tool or config.
#[doc(hidden)]
pub fn verify_signature_with_key(
    sums_bytes: &[u8],
    sig_text: &str,
    public_key_bytes: &[u8; 32],
) -> Result<(), ErrorBody> {
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

    let key = VerifyingKey::from_bytes(public_key_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Trusted release public key is invalid: {err}."),
        )
    })?;
    let signature = Signature::from_bytes(&sig_bytes);

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

/// Formats a raw 64-byte Ed25519 signature into the canonical `.sig` text format (base64 + `\n`).
#[cfg(test)]
pub(crate) fn encode_signature_bytes(sig_bytes: &[u8; 64]) -> String {
    format!("{}\n", BASE64.encode(sig_bytes))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn test_keypair(seed: u8) -> (SigningKey, [u8; 32]) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let verifying = signing.verifying_key().to_bytes();
        (signing, verifying)
    }

    #[test]
    fn pinned_public_key_parses_as_valid_ed25519_key() {
        let key = VerifyingKey::from_bytes(&RELEASE_PUBLIC_KEY).unwrap();
        assert_eq!(
            BASE64.encode(key.to_bytes()),
            "B8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg="
        );
    }

    #[test]
    fn verifies_good_signature_and_rejects_wrong_key_modified_file_and_truncated_sig() {
        let (signing, pubkey) = test_keypair(0x11);
        let (_, other_pubkey) = test_keypair(0x22);
        let sums = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  legion-0.2.0-x86_64-unknown-linux-musl\n";

        let sig = signing.sign(sums);
        let sig_text = encode_signature_bytes(&sig.to_bytes());

        // Good signature passes.
        verify_signature_with_key(sums, &sig_text, &pubkey).unwrap();

        // Wrong key is denied.
        let wrong_key = verify_signature_with_key(sums, &sig_text, &other_pubkey).unwrap_err();
        assert_eq!(wrong_key.code, ErrorCode::Denied);

        // Any byte modification to SHA256SUMS is denied.
        let mut modified = sums.to_vec();
        modified.push(b' ');
        let mod_err = verify_signature_with_key(&modified, &sig_text, &pubkey).unwrap_err();
        assert_eq!(mod_err.code, ErrorCode::Denied);

        // Truncated or malformed signatures are denied.
        let truncated = &sig_text[..40];
        let trunc_err = verify_signature_with_key(sums, truncated, &pubkey).unwrap_err();
        assert_eq!(trunc_err.code, ErrorCode::Denied);

        let empty_err = verify_signature_with_key(sums, "", &pubkey).unwrap_err();
        assert_eq!(empty_err.code, ErrorCode::Denied);

        // Signature from test key is denied by the real compiled-in release key.
        let real_err = verify_release_signature(sums, &sig_text).unwrap_err();
        assert_eq!(real_err.code, ErrorCode::Denied);
    }
}
