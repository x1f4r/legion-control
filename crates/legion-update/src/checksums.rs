//! `SHA256SUMS` parsing, bare filename validation, and streaming SHA-256 verification.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use legion_proto::{ErrorBody, ErrorCode};
use sha2::{Digest, Sha256};

use crate::trust::CHECKSUMS_FILENAME;

/// Maximum accepted size of a `SHA256SUMS` file (256 KiB).
pub const MAX_SUMS_BYTES: usize = 256 * 1024;

/// Maximum accepted length of a bare entry name inside an update set.
pub const MAX_ENTRY_NAME_LEN: usize = 255;

/// Validates that `name` is a single bare filename with no path separators, traversal segments,
/// leading dot, whitespace, or control characters.
pub fn validate_entry_name(name: &str) -> Result<(), ErrorBody> {
    if name.is_empty() || name.len() > MAX_ENTRY_NAME_LEN {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Update file name {name:?} is empty or too long."),
        )
        .with_hint(
            "Use a bare file name from the update set, such as `update.json` or an artifact name.",
        ));
    }
    if name.starts_with('.')
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Update file name {name:?} is not a valid bare filename: paths, traversal segments, hidden names, and special characters are refused."
            ),
        )
        .with_hint("Pass only a bare filename listed in the update set's SHA256SUMS."));
    }
    Ok(())
}

/// Computes the lowercase hex SHA-256 digest of a byte slice.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    format_digest(&Sha256::digest(bytes))
}

/// Computes the lowercase hex SHA-256 digest of a regular file on disk using bounded memory.
pub fn sha256_file(path: &Path) -> Result<String, ErrorBody> {
    let mut file = File::open(path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not open {} to compute SHA-256: {err}.",
                path.display()
            ),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not read {} while computing SHA-256: {err}.",
                    path.display()
                ),
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format_digest(&hasher.finalize()))
}

fn format_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Finds the expected lowercase SHA-256 hex digest for `asset_name` inside a `SHA256SUMS` file.
///
/// Kept compatible with `meta.update` and `fleet.install` release checksums (allowing blank lines,
/// `#` comments, and optional binary-mode `*` filename prefixes).
pub fn lookup_checksum(sums_text: &str, asset_name: &str) -> Result<String, ErrorBody> {
    for raw_line in sums_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(hash_raw) = parts.next() else {
            continue;
        };
        let Some(rest) = parts.next() else {
            continue;
        };
        let file = rest
            .trim_start()
            .strip_prefix('*')
            .unwrap_or(rest.trim_start())
            .trim();
        if file == asset_name {
            if !is_valid_sha256_hex(hash_raw) {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{CHECKSUMS_FILENAME} has a malformed SHA-256 entry for {asset_name}."),
                )
                .with_hint("Re-download the release checksums or verify the release assets."));
            }
            return Ok(hash_raw.to_ascii_lowercase());
        }
    }
    Err(ErrorBody::new(
        ErrorCode::Failed,
        format!("{CHECKSUMS_FILENAME} does not contain an entry for {asset_name}."),
    )
    .with_hint("Check that the release includes this platform target and a matching SHA256SUMS."))
}

/// Verifies `path` against the `SHA256SUMS` content for `asset_name`, returning the verified digest.
pub fn verify_file_checksum(
    path: &Path,
    sums_text: &str,
    asset_name: &str,
) -> Result<String, ErrorBody> {
    let expected = lookup_checksum(sums_text, asset_name)?;
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("SHA-256 mismatch for {asset_name}: expected {expected}, got {actual}."),
        )
        .with_hint(
            "The downloaded file may be corrupted or tampered with; nothing was installed.",
        ));
    }
    Ok(actual)
}

/// Strictly parsed `SHA256SUMS` table for an update set.
///
/// Rejects empty manifests, malformed lines, path-like entry names, and duplicate filenames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sha256Sums {
    entries: BTreeMap<String, String>,
}

impl Sha256Sums {
    /// Parses raw `SHA256SUMS` bytes strictly.
    pub fn parse(bytes: &[u8]) -> Result<Self, ErrorBody> {
        if bytes.is_empty() || bytes.len() > MAX_SUMS_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{CHECKSUMS_FILENAME} is empty or exceeds {MAX_SUMS_BYTES} bytes."),
            )
            .with_hint("Regenerate SHA256SUMS with scripts/make-update-set.mjs."));
        }
        let text = std::str::from_utf8(bytes).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{CHECKSUMS_FILENAME} is not valid UTF-8: {err}."),
            )
        })?;

        let mut entries = BTreeMap::new();
        for (idx, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((hash_raw, rest)) = line.split_once(char::is_whitespace) else {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{CHECKSUMS_FILENAME} line {} is malformed.", idx + 1),
                )
                .with_hint("Each line in SHA256SUMS must be `<64 hex digits>  <filename>`."));
            };
            if !is_valid_sha256_hex(hash_raw) {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{CHECKSUMS_FILENAME} line {} has an invalid SHA-256 digest.",
                        idx + 1
                    ),
                ));
            }
            let file = rest
                .trim_start()
                .strip_prefix('*')
                .unwrap_or(rest.trim_start())
                .trim();
            validate_entry_name(file).map_err(|_| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{CHECKSUMS_FILENAME} line {} contains invalid filename {file:?}.",
                        idx + 1
                    ),
                )
                .with_hint(
                    "SHA256SUMS may only list bare filenames inside the update set directory.",
                )
            })?;
            let digest = hash_raw.to_ascii_lowercase();
            if entries.insert(file.to_owned(), digest).is_some() {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{CHECKSUMS_FILENAME} lists {file:?} more than once."),
                )
                .with_hint("Each file must appear exactly once in SHA256SUMS."));
            }
        }

        if entries.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{CHECKSUMS_FILENAME} contains no file entries."),
            ));
        }

        Ok(Self { entries })
    }

    /// Returns whether `name` is listed in this `SHA256SUMS`.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Looks up the expected lowercase hex SHA-256 digest for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(String::as_str)
    }

    /// Returns the expected lowercase hex SHA-256 digest for `name`, or fails if unlisted.
    pub fn require(&self, name: &str) -> Result<&str, ErrorBody> {
        validate_entry_name(name)?;
        self.get(name).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("{CHECKSUMS_FILENAME} does not list {name:?}."),
            )
            .with_hint("Only files listed in the update set's signed SHA256SUMS exist.")
        })
    }

    /// Iterates over `(name, sha256)` pairs in deterministic sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Returns the number of listed entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the manifest has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Verifies an in-memory byte slice against the expected digest for `name`.
    pub fn verify_bytes(&self, name: &str, bytes: &[u8]) -> Result<String, ErrorBody> {
        let expected = self.require(name)?;
        let actual = sha256_bytes(bytes);
        if actual != expected {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("SHA-256 mismatch for {name}: expected {expected}, got {actual}."),
            )
            .with_hint("The file does not match the signed SHA256SUMS; nothing was accepted."));
        }
        Ok(actual)
    }

    /// Verifies a regular non-symlink file on disk against the expected digest for `name`.
    pub fn verify_file(&self, name: &str, path: &Path) -> Result<String, ErrorBody> {
        let expected = self.require(name)?;
        let meta = fs::symlink_metadata(path).map_err(|err| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!(
                    "Expected file {name} at {} could not be read: {err}.",
                    path.display()
                ),
            )
        })?;
        if !meta.file_type().is_file() {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Expected {name} at {} to be a regular file (symlinks and directories are refused).",
                    path.display()
                ),
            ));
        }
        let actual = sha256_file(path)?;
        if actual != expected {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("SHA-256 mismatch for {name}: expected {expected}, got {actual}."),
            )
            .with_hint("The file may be corrupted or tampered with; nothing was accepted."));
        }
        Ok(actual)
    }
}

fn is_valid_sha256_hex(raw: &str) -> bool {
    raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit())
}
