//! End-to-end verification of signed Legion update sets against the four update rules.

use std::fs;
use std::path::Path;

use legion_proto::{ErrorBody, ErrorCode};

use crate::checksums::{MAX_SUMS_BYTES, Sha256Sums, validate_entry_name};
use crate::floor::check_sequence_upgrade;
use crate::manifest::{Artifact, MAX_MANIFEST_BYTES, TargetPlatform, UpdateManifest};
use crate::trust::{
    CHECKSUMS_FILENAME, MANIFEST_FILENAME, SIGNATURE_FILENAME, SignerKind, TrustAnchors,
};

/// Maximum accepted size of a `SHA256SUMS.sig` file.
pub const MAX_SIG_BYTES: usize = 1024;

/// Verified metadata of one update set after signature and `update.json` digest checks pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedSetMeta {
    /// Parsed and validated `update.json`.
    pub manifest: UpdateManifest,
    /// Parsed and validated `SHA256SUMS`.
    pub sums: Sha256Sums,
    /// Which compiled-in trusted key signed `SHA256SUMS`.
    pub signer: SignerKind,
    /// Stable `SHA256:<base64>` fingerprint of the trusted public key.
    pub key_fingerprint: String,
}

/// Verifies rule 1 (signature by a trusted key matching the channel) and `update.json` integrity
/// against `SHA256SUMS`, plus that every declared artifact is listed in `SHA256SUMS`.
pub fn verify_set_metadata(
    sums_bytes: &[u8],
    sig_text: &str,
    manifest_bytes: &[u8],
    anchors: &TrustAnchors,
) -> Result<VerifiedSetMeta, ErrorBody> {
    if sums_bytes.is_empty() || sums_bytes.len() > MAX_SUMS_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{CHECKSUMS_FILENAME} is empty or exceeds {MAX_SUMS_BYTES} bytes."),
        ));
    }
    if sig_text.len() > MAX_SIG_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("{SIGNATURE_FILENAME} exceeds {MAX_SIG_BYTES} bytes."),
        ));
    }

    // Rule 1: verify the Ed25519 signature over the exact raw bytes of SHA256SUMS BEFORE trusting
    // anything inside SHA256SUMS or update.json.
    let verified_sig = anchors.verify_any(sums_bytes, sig_text)?;
    let sums = Sha256Sums::parse(sums_bytes)?;

    // Rule 2 (manifest): update.json MUST be listed in the signed SHA256SUMS and match its digest.
    sums.verify_bytes(MANIFEST_FILENAME, manifest_bytes)?;
    let manifest = UpdateManifest::parse(manifest_bytes)?;

    // Enforce strict channel-to-key binding: stable sets must come from the release key and
    // preview sets must come from the compiled-in preview key.
    if verified_sig.signer.channel() != manifest.channel {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing update set: channel is {:?}, which cannot be signed by the {} key.",
                manifest.channel.as_str(),
                verified_sig.signer.as_str()
            ),
        )
        .with_hint(
            "Sign stable sets with the release key and preview sets with the preview key.",
        ));
    }

    // Every artifact declared in update.json must be listed in SHA256SUMS.
    for artifact in &manifest.artifacts {
        sums.require(&artifact.name)?;
    }

    Ok(VerifiedSetMeta {
        manifest,
        sums,
        signer: verified_sig.signer,
        key_fingerprint: verified_sig.key_fingerprint,
    })
}

/// Verifies a complete update set directory on disk: signature, `update.json`, and the SHA-256 of
/// every file listed in `SHA256SUMS` (refusing symlinks and missing or tampered files).
pub fn verify_set_directory(
    dir: &Path,
    anchors: &TrustAnchors,
) -> Result<VerifiedSetMeta, ErrorBody> {
    let dir_meta = fs::symlink_metadata(dir).map_err(|err| {
        ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "Update set directory {} cannot be inspected: {err}.",
                dir.display()
            ),
        )
        .with_hint("Pass a directory created by scripts/make-update-set.mjs.")
    })?;
    if !dir_meta.file_type().is_dir() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} is not a directory (symlinks are refused).",
                dir.display()
            ),
        ));
    }

    let sums_bytes = read_small_regular_file(
        &dir.join(CHECKSUMS_FILENAME),
        CHECKSUMS_FILENAME,
        MAX_SUMS_BYTES,
    )?;
    let sig_bytes = read_small_regular_file(
        &dir.join(SIGNATURE_FILENAME),
        SIGNATURE_FILENAME,
        MAX_SIG_BYTES,
    )?;
    let sig_text = String::from_utf8(sig_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!("{SIGNATURE_FILENAME} is not valid UTF-8: {err}."),
        )
    })?;
    let manifest_bytes = read_small_regular_file(
        &dir.join(MANIFEST_FILENAME),
        MANIFEST_FILENAME,
        MAX_MANIFEST_BYTES,
    )?;

    let meta = verify_set_metadata(&sums_bytes, &sig_text, &manifest_bytes, anchors)?;

    // Verify every single file listed in SHA256SUMS exists as a regular file and matches its hash.
    for (name, _) in meta.sums.iter() {
        meta.sums.verify_file(name, &dir.join(name))?;
    }

    Ok(meta)
}

/// Checks rules 3 and 4 on an already signature-verified set: strictly newer sequence than both
/// `running_sequence` and `floor_path`, and a matching artifact for `target` listed in `SHA256SUMS`.
pub fn select_verified_artifact<'a>(
    meta: &'a VerifiedSetMeta,
    running_sequence: u64,
    floor_path: &Path,
    target: TargetPlatform,
) -> Result<&'a Artifact, ErrorBody> {
    check_sequence_upgrade(meta.manifest.sequence, running_sequence, floor_path)?;
    let artifact = meta.manifest.select_artifact(target)?;
    meta.sums.require(&artifact.name)?;
    Ok(artifact)
}

/// Verifies a downloaded artifact file on disk against the signed `SHA256SUMS` inside `meta`.
pub fn verify_artifact_file(
    meta: &VerifiedSetMeta,
    artifact_name: &str,
    file_path: &Path,
) -> Result<String, ErrorBody> {
    validate_entry_name(artifact_name)?;
    meta.sums.verify_file(artifact_name, file_path)
}

fn read_small_regular_file(
    path: &Path,
    label: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, ErrorBody> {
    let meta = fs::symlink_metadata(path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("Update set is missing {label} ({}): {err}.", path.display()),
        )
        .with_hint("Assemble and sign the update set with scripts/make-update-set.mjs.")
    })?;
    if !meta.file_type().is_file() {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{label} at {} is not a regular file (symlinks are refused).",
                path.display()
            ),
        ));
    }
    if meta.len() > max_bytes as u64 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{label} exceeds the {max_bytes}-byte limit."),
        ));
    }
    fs::read(path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not read {label} at {}: {err}.", path.display()),
        )
    })
}
