//! Semantic version comparison and the per-node / per-machine version floor.
//!
//! An Ed25519 signature proves a release came from the owner's key, not that it is still safe to
//! install: every historically signed release remains validly signed forever. Comparing versions
//! with SemVer precedence and recording a persistent floor in the state directory (modeled on
//! `agent/src/floor.mjs`) stops a silent downgrade to an older signed build unless the caller
//! explicitly passes `allow_downgrade = true`.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::atomic::{WriteOptions, write_atomic};
use serde::{Deserialize, Serialize};

/// Maximum accepted size of a version-floor JSON file.
const MAX_FLOOR_BYTES: u64 = 64 * 1024;

/// Parsed SemVer 2.0.0 version (`major.minor.patch[-prerelease][+build]`).
#[derive(Clone, Debug, Eq)]
pub struct SemVer {
    /// Major version component.
    pub major: u64,
    /// Minor version component.
    pub minor: u64,
    /// Patch version component.
    pub patch: u64,
    /// Optional dot-separated pre-release identifiers (empty for stable releases).
    pub prerelease: Vec<PreId>,
    /// Optional build metadata (preserved for display, ignored in precedence).
    pub build: Option<String>,
}

/// One dot-separated SemVer 2.0.0 pre-release identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreId {
    /// Purely numeric identifier, compared numerically and always lower than alphanumeric ones.
    Numeric(u64),
    /// Alphanumeric identifier containing at least one non-digit ASCII character.
    Alpha(String),
}

impl Ord for PreId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreId::Numeric(a), PreId::Numeric(b)) => a.cmp(b),
            (PreId::Numeric(_), PreId::Alpha(_)) => Ordering::Less,
            (PreId::Alpha(_), PreId::Numeric(_)) => Ordering::Greater,
            (PreId::Alpha(a), PreId::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    (true, true) => Ordering::Equal,
                    // SemVer 2.0.0 §11.3: a normal version has higher precedence than a pre-release.
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.prerelease.cmp(&other.prerelease),
                },
            )
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            f.write_str("-")?;
            for (idx, id) in self.prerelease.iter().enumerate() {
                if idx > 0 {
                    f.write_str(".")?;
                }
                match id {
                    PreId::Numeric(n) => write!(f, "{n}")?,
                    PreId::Alpha(s) => f.write_str(s)?,
                }
            }
        }
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

impl SemVer {
    /// Parses a semantic version string, stripping an optional leading `legion-v` or `v`.
    pub fn parse(raw: &str) -> Result<Self, ErrorBody> {
        let trimmed = raw.trim();
        let stripped = trimmed
            .strip_prefix("legion-v")
            .or_else(|| trimmed.strip_prefix('v'))
            .unwrap_or(trimmed);

        let (without_build, build) = match stripped.split_once('+') {
            Some((left, meta)) => {
                if meta.is_empty()
                    || meta.split('.').any(|part| {
                        part.is_empty()
                            || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
                {
                    return Err(invalid_version(raw));
                }
                (left, Some(meta.to_owned()))
            }
            None => (stripped, None),
        };

        let (core, pre_str) = match without_build.split_once('-') {
            Some((left, pre)) => (left, Some(pre)),
            None => (without_build, None),
        };

        let mut core_parts = core.split('.');
        let (Some(maj_s), Some(min_s), Some(pat_s), None) = (
            core_parts.next(),
            core_parts.next(),
            core_parts.next(),
            core_parts.next(),
        ) else {
            return Err(invalid_version(raw));
        };

        let major = parse_numeric_part(maj_s).ok_or_else(|| invalid_version(raw))?;
        let minor = parse_numeric_part(min_s).ok_or_else(|| invalid_version(raw))?;
        let patch = parse_numeric_part(pat_s).ok_or_else(|| invalid_version(raw))?;

        let mut prerelease = Vec::new();
        if let Some(pre) = pre_str {
            if pre.is_empty() {
                return Err(invalid_version(raw));
            }
            for part in pre.split('.') {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(invalid_version(raw));
                }
                if part.bytes().all(|b| b.is_ascii_digit()) {
                    let num = parse_numeric_part(part).ok_or_else(|| invalid_version(raw))?;
                    prerelease.push(PreId::Numeric(num));
                } else {
                    prerelease.push(PreId::Alpha(part.to_owned()));
                }
            }
        }

        Ok(Self {
            major,
            minor,
            patch,
            prerelease,
            build,
        })
    }

    /// Whether this version carries pre-release identifiers (for example `0.2.0-rc.1`).
    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }
}

fn parse_numeric_part(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // SemVer 2.0.0 forbids leading zeroes on numeric identifiers (`01` is invalid; `0` is valid).
    if part.len() > 1 && part.starts_with('0') {
        return None;
    }
    part.parse::<u64>().ok()
}

fn invalid_version(raw: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::InvalidArgs,
        format!("Version {raw:?} is not a valid semantic version."),
    )
    .with_hint("Use a semantic version such as `0.2.0`, `0.2.0-rc.1`, or `legion-v0.2.0`.")
}

/// Path to this node's version floor file (`<state_dir>/version-floor.json`).
pub fn node_floor_path(paths: &Paths) -> PathBuf {
    paths.state_dir().join("version-floor.json")
}

/// Path to a remote fleet machine's version floor file (`<state_dir>/fleet-floors/<machine>.json`).
pub fn machine_floor_path(paths: &Paths, machine: &str) -> PathBuf {
    paths
        .state_dir()
        .join("fleet-floors")
        .join(format!("{machine}.json"))
}

/// On-disk state of a version floor file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FloorState {
    /// No floor file exists yet.
    Missing,
    /// A valid floor version is recorded.
    Recorded(SemVer),
    /// The file exists but is unreadable, oversized, or malformed.
    Invalid(String),
}

#[derive(Debug, Deserialize, Serialize)]
struct FloorRecord {
    schema: u32,
    version: String,
    updated_at: String,
    reason: String,
}

/// Reads a version floor file without modifying it.
pub fn read_floor(path: &Path) -> FloorState {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return FloorState::Missing,
        Err(err) => {
            return FloorState::Invalid(format!("{} cannot be read: {err}", path.display()));
        }
    };
    if !meta.is_file() || meta.len() > MAX_FLOOR_BYTES {
        return FloorState::Invalid(format!("{} is not a small regular file", path.display()));
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            return FloorState::Invalid(format!("{} cannot be read: {err}", path.display()));
        }
    };
    let Ok(record) = serde_json::from_str::<FloorRecord>(&text) else {
        return FloorState::Invalid(format!(
            "{} does not hold a valid version-floor JSON record",
            path.display()
        ));
    };
    if record.schema != 1 {
        return FloorState::Invalid(format!(
            "{} has unsupported schema {}",
            path.display(),
            record.schema
        ));
    }
    match SemVer::parse(&record.version) {
        Ok(ver) => FloorState::Recorded(ver),
        Err(_) => FloorState::Invalid(format!(
            "{} holds invalid version {:?}",
            path.display(),
            record.version
        )),
    }
}

/// Refuses downgrading below either the currently running version or the recorded version floor
/// unless `allow_downgrade` is explicitly set.
///
/// An unreadable or corrupted floor file also blocks transitions when `allow_downgrade` is false
/// so nobody can silently bypass a floor by corrupting the file.
pub fn check_version_transition(
    target: &SemVer,
    live: Option<&SemVer>,
    floor_path: &Path,
    allow_downgrade: bool,
) -> Result<(), ErrorBody> {
    let floor = read_floor(floor_path);

    if let FloorState::Invalid(detail) = &floor
        && !allow_downgrade
    {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot check target version {target} against the version floor: {detail}."),
        )
        .with_hint(
            "Fix or inspect the floor file, or repeat with `allow_downgrade=true` to reset it to the target version.",
        ));
    }

    if let FloorState::Recorded(recorded) = &floor
        && target < recorded
        && !allow_downgrade
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Refusing to downgrade to {target}: it is older than the recorded version floor {recorded}."
            ),
        )
        .with_hint("Pass `allow_downgrade=true` to downgrade deliberately and lower the floor."));
    }

    if let Some(current) = live
        && target < current
        && !allow_downgrade
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Refusing to downgrade to {target}: it is older than the installed version {current}."
            ),
        )
        .with_hint("Pass `allow_downgrade=true` to downgrade deliberately."));
    }

    Ok(())
}

/// Persists the version floor after a successful install or update.
///
/// Normally the floor only moves upward (`max(existing, installed)`). When `allow_downgrade` is
/// `true`, the floor is intentionally set to `installed` so subsequent checks reflect the new
/// baseline. Never overwrites a corrupt floor file unless `allow_downgrade` is `true`.
pub fn commit_floor(
    floor_path: &Path,
    installed: &SemVer,
    allow_downgrade: bool,
) -> Result<(), ErrorBody> {
    let existing = read_floor(floor_path);
    let (next, reason) = match existing {
        FloorState::Invalid(detail) if !allow_downgrade => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Version floor was not updated because {detail}."),
            ));
        }
        FloorState::Recorded(current) if !allow_downgrade => {
            if installed <= &current {
                return Ok(());
            }
            (installed, "install")
        }
        _ => (
            installed,
            if allow_downgrade {
                "downgrade"
            } else {
                "install"
            },
        ),
    };

    if let Some(parent) = floor_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not create state directory {}: {err}.",
                    parent.display()
                ),
            )
        })?;
    }

    let record = FloorRecord {
        schema: 1,
        version: next.to_string(),
        updated_at: Timestamp::now().to_string(),
        reason: reason.to_owned(),
    };
    let mut json = serde_json::to_string_pretty(&record).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not serialize version floor: {err}."),
        )
    })?;
    json.push('\n');

    write_atomic(floor_path, json.as_bytes(), WriteOptions::with_mode(0o600)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not write version floor {}: {err}.",
                floor_path.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn semver_ordering_follows_semver_2_rules() {
        let v0_1 = SemVer::parse("0.1.0").unwrap();
        let v0_2_alpha = SemVer::parse("0.2.0-alpha.1").unwrap();
        let v0_2_rc1 = SemVer::parse("legion-v0.2.0-rc.1").unwrap();
        let v0_2_rc2 = SemVer::parse("0.2.0-rc.2").unwrap();
        let v0_2_rc10 = SemVer::parse("0.2.0-rc.10").unwrap();
        let v0_2 = SemVer::parse("v0.2.0").unwrap();
        let v0_10 = SemVer::parse("0.10.0").unwrap();
        let v0_10_build = SemVer::parse("0.10.0+build.42").unwrap();

        assert!(v0_1 < v0_2_alpha);
        assert!(v0_2_alpha < v0_2_rc1);
        assert!(v0_2_rc1 < v0_2_rc2);
        assert!(v0_2_rc2 < v0_2_rc10);
        assert!(v0_2_rc10 < v0_2);
        assert!(v0_2 < v0_10);
        assert_eq!(v0_10, v0_10_build);
        assert!(v0_2_rc1.is_prerelease());
        assert!(!v0_2.is_prerelease());

        assert!(SemVer::parse("01.2.3").is_err());
        assert!(SemVer::parse("1.2").is_err());
        assert!(SemVer::parse("not-a-version").is_err());
    }

    #[test]
    fn floor_blocks_downgrades_and_corrupt_files_unless_allow_downgrade() {
        let dir = tempdir().unwrap();
        let floor = dir.path().join("version-floor.json");
        let v0_1 = SemVer::parse("0.1.0").unwrap();
        let v0_2 = SemVer::parse("0.2.0").unwrap();
        let v0_3 = SemVer::parse("0.3.0").unwrap();

        // Fresh state allows installing 0.2.0 over 0.1.0 and raises floor to 0.2.0.
        check_version_transition(&v0_2, Some(&v0_1), &floor, false).unwrap();
        commit_floor(&floor, &v0_2, false).unwrap();
        assert_eq!(read_floor(&floor), FloorState::Recorded(v0_2.clone()));

        // Downgrading below floor (even if live is somehow lower) is refused.
        let err = check_version_transition(&v0_1, Some(&v0_1), &floor, false).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("floor"), "{}", err.message);

        // Downgrading below live version is refused even with no floor.
        let no_floor = dir.path().join("missing.json");
        let live_err = check_version_transition(&v0_2, Some(&v0_3), &no_floor, false).unwrap_err();
        assert_eq!(live_err.code, ErrorCode::InvalidArgs);
        assert!(
            live_err.message.contains("installed"),
            "{}",
            live_err.message
        );

        // Explicit allow_downgrade permits going back and lowers the floor on commit.
        check_version_transition(&v0_1, Some(&v0_2), &floor, true).unwrap();
        commit_floor(&floor, &v0_1, true).unwrap();
        assert_eq!(read_floor(&floor), FloorState::Recorded(v0_1));

        // Corrupt floor file blocks transitions without allow_downgrade.
        fs::write(&floor, "{broken json").unwrap();
        let corrupt = check_version_transition(&v0_3, Some(&v0_2), &floor, false).unwrap_err();
        assert_eq!(corrupt.code, ErrorCode::Failed);
        check_version_transition(&v0_3, Some(&v0_2), &floor, true).unwrap();
    }
}
