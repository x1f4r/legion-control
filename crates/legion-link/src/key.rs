//! Device identity keys (Ed25519) in OpenSSH format.

use std::collections::BTreeSet;

use legion_proto::{ErrorBody, ErrorCode};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey};
use zeroize::{Zeroize, Zeroizing};

/// OpenSSH `SSHSIG` namespace used for enrolment proof-of-possession signatures.
pub const ENROL_PROOF_NAMESPACE: &str = "legion-enrol";

/// Fixed prefix of the canonical enrolment proof message.
pub const ENROL_PROOF_PREFIX: &str = "legion-enrol-proof-v1";

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

    /// Produces an OpenSSH `SSHSIG` PEM proof of possession (namespace `legion-enrol`, SHA-512)
    /// proving this key owns its private half for `invite_id`, `node`, and `host_keys`.
    pub fn sign_enrol_proof(
        &self,
        invite_id: &str,
        node: &str,
        host_keys: &[String],
    ) -> Result<String, ErrorBody> {
        let canonical_pub = self.public_key_line("")?;
        let mut fps = BTreeSet::new();
        for raw in host_keys {
            if let Some(fp) = public_key_fingerprint(raw) {
                fps.insert(fp);
            }
        }
        if fps.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Invite carries no valid host keys for proof signing.",
            ));
        }
        let joined: Vec<&str> = fps.iter().map(String::as_str).collect();
        let msg = format!(
            "{ENROL_PROOF_PREFIX}\nid={invite_id}\npublic_key={canonical_pub}\nnode={node}\nhost_keys={}\n",
            joined.join(",")
        );
        let sig = self
            .inner
            .sign(ENROL_PROOF_NAMESPACE, HashAlg::Sha512, msg.as_bytes())
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Could not sign enrolment proof: {err}."),
                )
            })?;
        sig.to_pem(LineEnding::LF).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not encode enrolment proof as SSHSIG PEM: {err}."),
            )
        })
    }

    pub(crate) fn private_key(&self) -> &PrivateKey {
        &self.inner
    }
}

/// Computes the OpenSSH SHA-256 fingerprint (`SHA256:...`) for a single-line OpenSSH public key.
pub fn public_key_fingerprint(openssh_line: &str) -> Option<String> {
    crate::link::public_key_fingerprint(openssh_line)
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

    #[test]
    fn enrol_proof_verifies_with_ssh_key_and_ssh_keygen() {
        use russh::keys::ssh_key::SshSig;

        let key = DeviceKey::from_seed([0x33u8; 32]);
        let host_key = DeviceKey::from_seed([0x44u8; 32])
            .public_key_line("")
            .unwrap();
        let proof = key
            .sign_enrol_proof("inv-abc", "tower", std::slice::from_ref(&host_key))
            .unwrap();
        assert!(proof.starts_with("-----BEGIN SSH SIGNATURE-----"));
        assert!(proof.trim_end().ends_with("-----END SSH SIGNATURE-----"));

        let parsed = SshSig::from_pem(&proof).unwrap();
        assert_eq!(parsed.namespace(), ENROL_PROOF_NAMESPACE);
        let canonical = key.public_key_line("").unwrap();
        let host_fp = public_key_fingerprint(&host_key).unwrap();
        let expected_msg = format!(
            "{ENROL_PROOF_PREFIX}\nid=inv-abc\npublic_key={canonical}\nnode=tower\nhost_keys={host_fp}\n"
        );
        key.private_key()
            .public_key()
            .verify(ENROL_PROOF_NAMESPACE, expected_msg.as_bytes(), &parsed)
            .unwrap();

        if Command::new("ssh-keygen").arg("-?").output().is_ok() {
            let dir = tempfile::tempdir().unwrap();
            let allowed = dir.path().join("allowed_signers");
            let sig_path = dir.path().join("proof.sig");
            std::fs::write(&allowed, format!("device {canonical}\n")).unwrap();
            std::fs::write(&sig_path, &proof).unwrap();
            let mut child = Command::new("ssh-keygen")
                .args(["-Y", "verify", "-f"])
                .arg(&allowed)
                .args(["-I", "device", "-n", ENROL_PROOF_NAMESPACE, "-s"])
                .arg(&sig_path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(expected_msg.as_bytes())
                    .unwrap();
            }
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "ssh-keygen -Y verify rejected proof: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
