//! Shared update-set parsing, signature verification, monotonic sequence ordering, and artifact
//! selection for both the Legion node and the native apps.
//!
//! Every program installs a newer copy of itself only when all four rules hold:
//!
//! 1. `SHA256SUMS` carries a valid detached Ed25519 signature (`SHA256SUMS.sig`) by a compiled-in
//!    trusted key matching the set's channel (`stable` -> release key, `preview` -> optional
//!    build-time preview key from `LEGION_PREVIEW_UPDATE_KEY`).
//! 2. `update.json` and the artifact to install are listed in `SHA256SUMS` and their SHA-256
//!    digests match.
//! 3. The set's `sequence` is strictly greater than both the running build's sequence and the
//!    recorded persistent sequence floor.
//! 4. The artifact matches this platform's program, OS, architecture, and install kind.

pub mod checksums;
pub mod floor;
pub mod manifest;
pub mod trust;
pub mod verify;
pub mod version;

pub use checksums::{
    MAX_ENTRY_NAME_LEN, MAX_SUMS_BYTES, Sha256Sums, lookup_checksum, sha256_bytes, sha256_file,
    validate_entry_name, verify_file_checksum,
};
pub use floor::{
    SequenceFloorState, check_sequence_upgrade, commit_sequence_floor, read_sequence_floor,
};
pub use manifest::{
    Arch, Artifact, Channel, InstallKind, MAX_ARTIFACTS, MAX_MANIFEST_BYTES, MAX_NOTES_BYTES, Os,
    Program, TargetPlatform, UpdateManifest,
};
pub use trust::{
    CHECKSUMS_FILENAME, MANIFEST_FILENAME, PREVIEW_PUBLIC_KEY, RELEASE_PUBLIC_KEY,
    SIGNATURE_FILENAME, SignerKind, TrustAnchors, VerifiedSignature, encode_signature_bytes,
    key_fingerprint, verify_release_signature, verify_signature_with_key,
};
pub use verify::{
    MAX_SIG_BYTES, VerifiedSetMeta, select_verified_artifact, verify_artifact_file,
    verify_set_directory, verify_set_metadata,
};
pub use version::{
    android_version_code, compiled_sequence, desktop_bundle_version, macos_bundle_version,
};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
    use legion_proto::ErrorCode;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn keypair(seed: u8) -> (SigningKey, [u8; 32]) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let verifying = signing.verifying_key().to_bytes();
        (signing, verifying)
    }

    type TestArtifact<'a> = (&'a str, &'a str, &'a str, &'a str, &'a str, &'a [u8]);

    fn build_signed_set(
        dir: &Path,
        channel: &str,
        version: &str,
        sequence: u64,
        artifacts: &[TestArtifact<'_>],
        signer: &SigningKey,
    ) {
        let mut artifact_entries = Vec::new();
        let mut sum_entries: Vec<(String, String)> = Vec::new();

        for (name, program, os, arch, kind, bytes) in artifacts {
            fs::write(dir.join(name), bytes).unwrap();
            sum_entries.push(((*name).to_owned(), sha256_bytes(bytes)));
            artifact_entries.push(json!({
                "name": name,
                "program": program,
                "os": os,
                "arch": arch,
                "kind": kind,
            }));
        }

        let manifest_val = json!({
            "channel": channel,
            "version": version,
            "build": "a1b2c3d4e5f6",
            "sequence": sequence,
            "published_at": "2026-11-25T01:00:00Z",
            "notes": "Self-updating works smoothly.",
            "artifacts": artifact_entries,
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest_val).unwrap();
        fs::write(dir.join(MANIFEST_FILENAME), &manifest_bytes).unwrap();
        sum_entries.push((MANIFEST_FILENAME.to_owned(), sha256_bytes(&manifest_bytes)));
        sum_entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut sums = String::new();
        for (name, hash) in sum_entries {
            sums.push_str(&format!("{hash}  {name}\n"));
        }
        let sums_bytes = sums.as_bytes();
        fs::write(dir.join(CHECKSUMS_FILENAME), sums_bytes).unwrap();

        let sig = signer.sign(sums_bytes);
        let sig_text = encode_signature_bytes(&sig.to_bytes());
        fs::write(dir.join(SIGNATURE_FILENAME), sig_text).unwrap();
    }

    #[test]
    fn pinned_release_key_matches_contract_pem() {
        let key = VerifyingKey::from_bytes(&RELEASE_PUBLIC_KEY).unwrap();
        assert_eq!(
            BASE64.encode(key.to_bytes()),
            "B8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg="
        );
        assert!(key_fingerprint(&RELEASE_PUBLIC_KEY).starts_with("SHA256:"));
    }

    #[test]
    fn valid_stable_and_preview_sets_verify_and_select_artifacts() {
        let (rel_sign, rel_pub) = keypair(0x11);
        let (prev_sign, prev_pub) = keypair(0x22);
        let anchors_both = TrustAnchors::with_keys(rel_pub, Some(prev_pub));
        let floor_dir = tempdir().unwrap();
        let floor_path = floor_dir.path().join("floor.json");

        // Preview set with universal Android APK and Linux AppImage.
        let prev_dir = tempdir().unwrap();
        build_signed_set(
            prev_dir.path(),
            "preview",
            "0.2.0",
            202611250130,
            &[
                (
                    "Legion-0.2.0-universal.apk",
                    "app",
                    "android",
                    "universal",
                    "apk",
                    b"fake-apk-payload",
                ),
                (
                    "Legion-0.2.0-x86_64.AppImage",
                    "app",
                    "linux",
                    "x86_64",
                    "appimage",
                    b"fake-appimage-payload",
                ),
            ],
            &prev_sign,
        );

        let verified = verify_set_directory(prev_dir.path(), &anchors_both).unwrap();
        assert_eq!(verified.signer, SignerKind::Preview);
        assert_eq!(verified.manifest.sequence, 202611250130);

        let android_art = select_verified_artifact(
            &verified,
            202611250100,
            &floor_path,
            TargetPlatform {
                program: Program::App,
                os: Os::Android,
                arch: Arch::Aarch64,
                kind: InstallKind::Apk,
            },
        )
        .unwrap();
        assert_eq!(android_art.name, "Legion-0.2.0-universal.apk");

        let linux_art = select_verified_artifact(
            &verified,
            202611250100,
            &floor_path,
            TargetPlatform {
                program: Program::App,
                os: Os::Linux,
                arch: Arch::X86_64,
                kind: InstallKind::AppImage,
            },
        )
        .unwrap();
        assert_eq!(linux_art.name, "Legion-0.2.0-x86_64.AppImage");

        // A preview build also accepts a newer stable set signed by the release key.
        commit_sequence_floor(&floor_path, 202611250130).unwrap();
        let stable_dir = tempdir().unwrap();
        build_signed_set(
            stable_dir.path(),
            "stable",
            "0.2.1",
            202611250200,
            &[(
                "Legion-0.2.1-x86_64.AppImage",
                "app",
                "linux",
                "x86_64",
                "appimage",
                b"stable-appimage",
            )],
            &rel_sign,
        );
        let stable_meta = verify_set_directory(stable_dir.path(), &anchors_both).unwrap();
        assert_eq!(stable_meta.signer, SignerKind::Release);
        let chosen = select_verified_artifact(
            &stable_meta,
            202611250130,
            &floor_path,
            TargetPlatform {
                program: Program::App,
                os: Os::Linux,
                arch: Arch::X86_64,
                kind: InstallKind::AppImage,
            },
        )
        .unwrap();
        assert_eq!(chosen.name, "Legion-0.2.1-x86_64.AppImage");
    }

    #[test]
    fn refuses_tampered_artifact() {
        let (rel_sign, rel_pub) = keypair(0x31);
        let anchors = TrustAnchors::with_keys(rel_pub, None);
        let dir = tempdir().unwrap();
        build_signed_set(
            dir.path(),
            "stable",
            "0.2.0",
            202611250130,
            &[(
                "Legion-0.2.0-x86_64.AppImage",
                "app",
                "linux",
                "x86_64",
                "appimage",
                b"good-bytes",
            )],
            &rel_sign,
        );

        let meta = verify_set_directory(dir.path(), &anchors).unwrap();
        fs::write(
            dir.path().join("Legion-0.2.0-x86_64.AppImage"),
            b"evil-bytes",
        )
        .unwrap();

        let dir_err = verify_set_directory(dir.path(), &anchors).unwrap_err();
        assert_eq!(dir_err.code, ErrorCode::Failed);
        assert!(dir_err.message.contains("SHA-256 mismatch"));

        let file_err = verify_artifact_file(
            &meta,
            "Legion-0.2.0-x86_64.AppImage",
            &dir.path().join("Legion-0.2.0-x86_64.AppImage"),
        )
        .unwrap_err();
        assert_eq!(file_err.code, ErrorCode::Failed);
        assert!(file_err.message.contains("SHA-256 mismatch"));
    }

    #[test]
    fn refuses_tampered_update_json() {
        let (rel_sign, rel_pub) = keypair(0x32);
        let anchors = TrustAnchors::with_keys(rel_pub, None);
        let dir = tempdir().unwrap();
        build_signed_set(
            dir.path(),
            "stable",
            "0.2.0",
            202611250130,
            &[(
                "Legion-0.2.0-x86_64.AppImage",
                "app",
                "linux",
                "x86_64",
                "appimage",
                b"good-bytes",
            )],
            &rel_sign,
        );

        // Tamper with update.json after SHA256SUMS was signed.
        let mut raw = fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).unwrap();
        raw = raw.replace("0.2.0", "9.9.9");
        fs::write(dir.path().join(MANIFEST_FILENAME), raw).unwrap();

        let err = verify_set_directory(dir.path(), &anchors).unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(err.message.contains("SHA-256 mismatch"));
    }

    #[test]
    fn refuses_unknown_signer_and_preview_without_preview_key_and_channel_mismatch() {
        let (rel_sign, rel_pub) = keypair(0x41);
        let (prev_sign, prev_pub) = keypair(0x42);
        let (unknown_sign, _) = keypair(0x43);

        let anchors_release_only = TrustAnchors::with_keys(rel_pub, None);
        let anchors_with_preview = TrustAnchors::with_keys(rel_pub, Some(prev_pub));

        // 1. Unknown key refused even when preview key is enabled.
        let unknown_dir = tempdir().unwrap();
        build_signed_set(
            unknown_dir.path(),
            "stable",
            "0.2.0",
            202611250130,
            &[("app.apk", "app", "android", "aarch64", "apk", b"apk")],
            &unknown_sign,
        );
        let err = verify_set_directory(unknown_dir.path(), &anchors_with_preview).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // 2. Preview set offered to a build without preview key is refused.
        let preview_dir = tempdir().unwrap();
        build_signed_set(
            preview_dir.path(),
            "preview",
            "0.2.0",
            202611250131,
            &[("app.apk", "app", "android", "aarch64", "apk", b"apk")],
            &prev_sign,
        );
        assert!(verify_set_directory(preview_dir.path(), &anchors_with_preview).is_ok());
        let no_prev_err =
            verify_set_directory(preview_dir.path(), &anchors_release_only).unwrap_err();
        assert_eq!(no_prev_err.code, ErrorCode::Denied);

        // 3. Preview key cannot forge a stable set.
        let forged_stable = tempdir().unwrap();
        build_signed_set(
            forged_stable.path(),
            "stable",
            "0.2.0",
            202611250132,
            &[("app.apk", "app", "android", "aarch64", "apk", b"apk")],
            &prev_sign,
        );
        let mismatch =
            verify_set_directory(forged_stable.path(), &anchors_with_preview).unwrap_err();
        assert_eq!(mismatch.code, ErrorCode::Denied);
        assert!(mismatch.message.contains("channel"));

        // 4. Release key cannot sign a preview channel set.
        let wrong_preview = tempdir().unwrap();
        build_signed_set(
            wrong_preview.path(),
            "preview",
            "0.2.0",
            202611250133,
            &[("app.apk", "app", "android", "aarch64", "apk", b"apk")],
            &rel_sign,
        );
        let mismatch2 =
            verify_set_directory(wrong_preview.path(), &anchors_with_preview).unwrap_err();
        assert_eq!(mismatch2.code, ErrorCode::Denied);
    }

    #[test]
    fn refuses_older_or_equal_sequence_and_corrupt_floor() {
        let (rel_sign, rel_pub) = keypair(0x51);
        let anchors = TrustAnchors::with_keys(rel_pub, None);
        let set_dir = tempdir().unwrap();
        build_signed_set(
            set_dir.path(),
            "stable",
            "0.2.0",
            202611250130,
            &[("app.apk", "app", "android", "aarch64", "apk", b"apk")],
            &rel_sign,
        );
        let meta = verify_set_directory(set_dir.path(), &anchors).unwrap();
        let floor_dir = tempdir().unwrap();
        let floor_path = floor_dir.path().join("floor.json");
        let target = TargetPlatform {
            program: Program::App,
            os: Os::Android,
            arch: Arch::Aarch64,
            kind: InstallKind::Apk,
        };

        // Equal to running sequence is refused.
        let eq_err =
            select_verified_artifact(&meta, 202611250130, &floor_path, target).unwrap_err();
        assert_eq!(eq_err.code, ErrorCode::Denied);

        // Older than running sequence is refused.
        let older_err =
            select_verified_artifact(&meta, 202611250131, &floor_path, target).unwrap_err();
        assert_eq!(older_err.code, ErrorCode::Denied);

        // Recorded floor blocks equal or older even if running_sequence were lower.
        commit_sequence_floor(&floor_path, 202611250130).unwrap();
        let floor_eq =
            select_verified_artifact(&meta, 202611250100, &floor_path, target).unwrap_err();
        assert_eq!(floor_eq.code, ErrorCode::Denied);
        assert!(floor_eq.message.contains("floor"));

        // Corrupt floor file fails closed.
        fs::write(&floor_path, "{broken").unwrap();
        let corrupt_err =
            select_verified_artifact(&meta, 202611250100, &floor_path, target).unwrap_err();
        assert_eq!(corrupt_err.code, ErrorCode::Failed);
    }

    #[test]
    fn refuses_wrong_platform_or_install_kind_and_unlisted_checksum_file() {
        let (rel_sign, rel_pub) = keypair(0x61);
        let anchors = TrustAnchors::with_keys(rel_pub, None);
        let dir = tempdir().unwrap();
        build_signed_set(
            dir.path(),
            "stable",
            "0.2.0",
            202611250130,
            &[(
                "Legion-0.2.0-x86_64.AppImage",
                "app",
                "linux",
                "x86_64",
                "appimage",
                b"appimage-bytes",
            )],
            &rel_sign,
        );
        let meta = verify_set_directory(dir.path(), &anchors).unwrap();
        let floor_dir = tempdir().unwrap();
        let floor_path = floor_dir.path().join("floor.json");

        // Wrong OS (macOS asking for Linux set).
        let wrong_os = select_verified_artifact(
            &meta,
            1,
            &floor_path,
            TargetPlatform {
                program: Program::App,
                os: Os::Macos,
                arch: Arch::Aarch64,
                kind: InstallKind::AppTar,
            },
        )
        .unwrap_err();
        assert_eq!(wrong_os.code, ErrorCode::NotFound);

        // Wrong install kind (Linux deb asking for AppImage-only set).
        let wrong_kind = select_verified_artifact(
            &meta,
            1,
            &floor_path,
            TargetPlatform {
                program: Program::App,
                os: Os::Linux,
                arch: Arch::X86_64,
                kind: InstallKind::Deb,
            },
        )
        .unwrap_err();
        assert_eq!(wrong_kind.code, ErrorCode::NotFound);

        // Wrong arch (Linux aarch64 asking for x86_64 AppImage).
        let wrong_arch = select_verified_artifact(
            &meta,
            1,
            &floor_path,
            TargetPlatform {
                program: Program::App,
                os: Os::Linux,
                arch: Arch::Aarch64,
                kind: InstallKind::AppImage,
            },
        )
        .unwrap_err();
        assert_eq!(wrong_arch.code, ErrorCode::NotFound);

        // Asking to verify a file NOT listed in SHA256SUMS is refused.
        fs::write(dir.path().join("unlisted.bin"), b"sneaky").unwrap();
        let unlisted_err =
            verify_artifact_file(&meta, "unlisted.bin", &dir.path().join("unlisted.bin"))
                .unwrap_err();
        assert_eq!(unlisted_err.code, ErrorCode::NotFound);

        // A set whose update.json references an artifact missing from SHA256SUMS is refused.
        let manifest_val = json!({
            "channel": "stable",
            "version": "0.2.0",
            "build": "a1b2c3d4e5f6",
            "sequence": 202611250135u64,
            "published_at": "2026-11-25T01:00:00Z",
            "notes": "Missing artifact hash.",
            "artifacts": [{
                "name": "missing.apk",
                "program": "app",
                "os": "android",
                "arch": "aarch64",
                "kind": "apk",
            }],
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest_val).unwrap();
        let sums = format!("{}  {}\n", sha256_bytes(&manifest_bytes), MANIFEST_FILENAME);
        let sig = encode_signature_bytes(&rel_sign.sign(sums.as_bytes()).to_bytes());
        let missing_sum_err =
            verify_set_metadata(sums.as_bytes(), &sig, &manifest_bytes, &anchors).unwrap_err();
        assert_eq!(missing_sum_err.code, ErrorCode::NotFound);
    }

    #[test]
    fn refuses_path_like_entry_names() {
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "../secret",
            "dir/file.apk",
            "dir\\file.apk",
            "/etc/passwd",
            "C:foo",
            "a..b.apk",
            "space name.apk",
        ] {
            let err = validate_entry_name(bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "should reject {bad:?}");
        }
        validate_entry_name("update.json").unwrap();
        validate_entry_name("Legion-0.2.0+202611250130_arm64-v8a.apk").unwrap();
    }

    #[test]
    fn android_version_code_is_strictly_monotonic_and_fits_i32() {
        assert_eq!(android_version_code(1).unwrap(), 1);
        assert_eq!(android_version_code(42).unwrap(), 42);
        let c1 = android_version_code(202611250130).unwrap();
        let c2 = android_version_code(202611250131).unwrap();
        let c3 = android_version_code(202611250200).unwrap();
        let c4 = android_version_code(202701010000).unwrap();
        assert!(c1 > 20_000_000);
        assert_eq!(c2, c1 + 1);
        assert!(c3 > c2);
        assert!(c4 > c3);
        assert!(android_version_code(299912312359).unwrap() < 2_100_000_000);

        assert!(android_version_code(0).is_err());
        assert!(android_version_code(202602300000).is_err());
        assert!(android_version_code(202613010000).is_err());
        assert!(android_version_code(202611252500).is_err());
        assert_eq!(
            desktop_bundle_version("0.1.0", 202611250130).unwrap(),
            "0.1.0+202611250130"
        );
        assert_eq!(macos_bundle_version(202611250130).unwrap(), "202611250130");
    }
}
