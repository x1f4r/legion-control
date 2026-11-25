//! The single release signing key Legion trusts, compiled into the binary via `legion-update`.
//!
//! Every release manifest (`SHA256SUMS`) fetched from GitHub or read from the dist cache must carry
//! a valid detached Ed25519 signature (`SHA256SUMS.sig`) made offline by this key. There is
//! deliberately no environment variable, configuration file, or CLI flag to override or disable
//! this key at run time: anyone who can edit a GitHub release or a machine's config must never be
//! able to swap the trust anchor that governs replacing executable code.
//!
//! The verification primitives live in `legion-update` and are re-exported here unchanged in
//! behaviour so `meta.update` and `fleet.install` share one implementation with the app updater.

pub use legion_update::{
    RELEASE_PUBLIC_KEY, SIGNATURE_FILENAME, verify_release_signature, verify_signature_with_key,
};

/// Formats a raw 64-byte Ed25519 signature into the canonical `.sig` text format (base64 + `\n`).
#[cfg(test)]
pub(crate) fn encode_signature_bytes(sig_bytes: &[u8; 64]) -> String {
    legion_update::encode_signature_bytes(sig_bytes)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
    use legion_core::proto::ErrorCode;

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
