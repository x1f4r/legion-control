//! Device identity keys (Ed25519) in OpenSSH format.

use legion_proto::{ErrorBody, ErrorCode};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey};
use zeroize::{Zeroize, Zeroizing};

/// An Ed25519 SSH identity key owned by a controller device.
///
/// The private half stays on the device (stored as an OpenSSH private key string by the caller);
/// only the OpenSSH public key line is sent to a node during enrolment.
#[derive(Clone)]
pub struct DeviceKey {
    inner: PrivateKey,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKey")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl DeviceKey {
    /// Generates a fresh Ed25519 device key using the operating system's CSPRNG.
    pub fn generate() -> Result<Self, ErrorBody> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut *seed).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not read random bytes for a new device key: {err}."),
            )
            .with_hint("Retry on a platform with an available OS random source.")
        })?;
        Ok(Self::from_seed(*seed))
    }

    /// Builds an Ed25519 key deterministically from a 32-byte seed, zeroizing its local copy.
    pub fn from_seed(mut seed: [u8; 32]) -> Self {
        let keypair = Ed25519Keypair::from_seed(&seed);
        seed.zeroize();
        let inner = PrivateKey::from(keypair);
        DeviceKey { inner }
    }

    /// Parses an unencrypted OpenSSH private key string.
    pub fn from_openssh(text: &str) -> Result<Self, ErrorBody> {
        let inner = PrivateKey::from_openssh(text.trim()).map_err(|err| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Could not parse OpenSSH private key: {err}."),
            )
            .with_hint("Provide an unencrypted OpenSSH private key (`-----BEGIN OPENSSH PRIVATE KEY-----`).")
        })?;
        if inner.is_encrypted() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Encrypted OpenSSH private keys are not supported here.",
            )
            .with_hint("Provide an unencrypted device key."));
        }
        Ok(DeviceKey { inner })
    }

    /// Exports the private key as an OpenSSH private key string suitable for storage by the app.
    pub fn to_openssh(&self) -> Result<String, ErrorBody> {
        self.inner
            .to_openssh(LineEnding::LF)
            .map(|s| s.to_string())
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Could not encode device key in OpenSSH format: {err}."),
                )
            })
    }

    /// Exports the public key as an OpenSSH `authorized_keys` line (`ssh-ed25519 AAAA... comment`).
    ///
    /// When `comment` is empty, the trailing comment is omitted (`ssh-ed25519 AAAA...`).
    pub fn public_key_line(&self, comment: &str) -> Result<String, ErrorBody> {
        let trimmed = comment.trim();
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Public key comment cannot contain control characters or newlines.",
            )
            .with_hint("Use a single-line device name."));
        }
        let pubkey = PublicKey::new(self.inner.public_key().key_data().clone(), trimmed);
        pubkey.to_openssh().map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not encode OpenSSH public key line: {err}."),
            )
        })
    }

    /// Returns the OpenSSH SHA-256 fingerprint (`SHA256:...`) of the public half.
    pub fn fingerprint(&self) -> String {
        self.inner
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string()
    }

    pub(crate) fn private_key(&self) -> &PrivateKey {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::process::Command;

    use super::*;

    #[test]
    fn seed_and_openssh_round_trip_deterministically() {
        let seed = [42u8; 32];
        let k1 = DeviceKey::from_seed(seed);
        let k2 = DeviceKey::from_seed(seed);
        assert_eq!(k1.fingerprint(), k2.fingerprint());
        assert_eq!(
            k1.public_key_line("phone").unwrap(),
            k2.public_key_line("phone").unwrap()
        );
        let pub_with_comment = k1.public_key_line("phone").unwrap();
        assert!(
            pub_with_comment.starts_with("ssh-ed25519 AAAA"),
            "{pub_with_comment}"
        );
        assert!(pub_with_comment.ends_with(" phone"), "{pub_with_comment}");
        let pub_bare = k1.public_key_line("").unwrap();
        assert_eq!(pub_bare.split_whitespace().count(), 2, "{pub_bare}");

        let pem = k1.to_openssh().unwrap();
        assert!(pem.contains("BEGIN OPENSSH PRIVATE KEY"));
        let reloaded = DeviceKey::from_openssh(&pem).unwrap();
        assert_eq!(reloaded.fingerprint(), k1.fingerprint());
        assert_eq!(reloaded.public_key_line("phone").unwrap(), pub_with_comment);
    }

    #[test]
    fn generated_keys_are_distinct_and_round_trip() {
        let a = DeviceKey::generate().unwrap();
        let b = DeviceKey::generate().unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        let pem = a.to_openssh().unwrap();
        let restored = DeviceKey::from_openssh(&pem).unwrap();
        assert_eq!(restored.fingerprint(), a.fingerprint());
    }

    #[test]
    fn public_key_line_matches_ssh_keygen_y() {
        let check = Command::new("ssh-keygen").arg("-?").output();
        if check.is_err() {
            // ssh-keygen is absent on this host; skip rather than fail.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("device_key");
        let key = DeviceKey::generate().unwrap();
        let pem = key.to_openssh().unwrap();
        std::fs::write(&key_path, &pem).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let out = Command::new("ssh-keygen")
            .args(["-y", "-f"])
            .arg(&key_path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ssh-keygen -y failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let keygen_line = String::from_utf8(out.stdout).unwrap();
        let keygen_fields: Vec<&str> = keygen_line.split_whitespace().collect();
        let our_line = key.public_key_line("phone").unwrap();
        let our_fields: Vec<&str> = our_line.split_whitespace().collect();
        assert!(keygen_fields.len() >= 2, "{keygen_line:?}");
        assert_eq!(our_fields[0], keygen_fields[0]);
        assert_eq!(our_fields[1], keygen_fields[1]);
        assert_eq!(our_fields[2], "phone");
        assert_eq!(
            key.public_key_line("").unwrap(),
            format!("{} {}", keygen_fields[0], keygen_fields[1])
        );
    }

    #[test]
    fn rejects_invalid_openssh_private_key() {
        let err = DeviceKey::from_openssh("not a key").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.hint.is_some());
    }
}
