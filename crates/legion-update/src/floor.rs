//! Persistent monotonic sequence floor for anti-rollback protection.
//!
//! Every update set signed by a trusted key remains validly signed forever. Recording the highest
//! sequence number a program has ever run or installed prevents replay or downgrade to an older
//! signed set. There is no flag that skips this check, and a corrupted floor file fails closed.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use jiff::Timestamp;
use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::version::check_sequence_plausibility;

/// Maximum accepted size of a sequence-floor JSON file.
const MAX_FLOOR_BYTES: u64 = 64 * 1024;

/// On-disk state of a sequence floor file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceFloorState {
    /// No floor file exists yet.
    Missing,
    /// A valid sequence floor is recorded.
    Recorded(u64),
    /// The file exists but is unreadable, a symlink, oversized, or malformed.
    Invalid(String),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SequenceFloorRecord {
    schema: u32,
    sequence: u64,
    updated_at: String,
}

/// Reads a sequence floor file without modifying it.
pub fn read_sequence_floor(path: &Path) -> SequenceFloorState {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return SequenceFloorState::Missing,
        Err(err) => {
            return SequenceFloorState::Invalid(format!(
                "{} cannot be read: {err}",
                path.display()
            ));
        }
    };
    if !meta.file_type().is_file() || meta.len() > MAX_FLOOR_BYTES {
        return SequenceFloorState::Invalid(format!(
            "{} is not a small regular file",
            path.display()
        ));
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            return SequenceFloorState::Invalid(format!(
                "{} cannot be read: {err}",
                path.display()
            ));
        }
    };
    let Ok(record) = serde_json::from_str::<SequenceFloorRecord>(&text) else {
        return SequenceFloorState::Invalid(format!(
            "{} does not hold a valid sequence-floor JSON record",
            path.display()
        ));
    };
    if record.schema != 1 {
        return SequenceFloorState::Invalid(format!(
            "{} has unsupported schema {}",
            path.display(),
            record.schema
        ));
    }
    if record.sequence == 0 {
        return SequenceFloorState::Invalid(format!(
            "{} holds an invalid sequence 0",
            path.display()
        ));
    }
    SequenceFloorState::Recorded(record.sequence)
}

/// Enforces rule 3: `running_sequence` must be non-zero, `candidate` must be plausible and
/// strictly greater than both `running_sequence` and any recorded sequence floor. Fails closed if
/// the floor file is corrupt.
pub fn check_sequence_upgrade(
    candidate: u64,
    running_sequence: u64,
    floor_path: &Path,
) -> Result<(), ErrorBody> {
    if running_sequence == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "This build has sequence 0 and cannot self-update.",
        )
        .with_hint(
            "Install a stamped build (built with LEGION_BUILD_SEQUENCE) to enable self-updates.",
        ));
    }
    if candidate == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Refusing update set: sequence must be greater than 0.",
        ));
    }
    check_sequence_plausibility(candidate, running_sequence)?;

    match read_sequence_floor(floor_path) {
        SequenceFloorState::Missing => {}
        SequenceFloorState::Invalid(detail) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Cannot check update sequence {candidate} against the recorded sequence floor: {detail}."
                ),
            )
            .with_hint(
                "Inspect the update floor file before installing updates; Legion refuses to bypass a damaged floor.",
            ));
        }
        SequenceFloorState::Recorded(recorded) => {
            if candidate <= recorded {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing update set (sequence {candidate}): it is not newer than the recorded sequence floor {recorded}."
                    ),
                )
                .with_hint("Only strictly newer signed update sets can be installed."));
            }
        }
    }

    if candidate <= running_sequence {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing update set (sequence {candidate}): it is not newer than the running build (sequence {running_sequence})."
            ),
        )
        .with_hint("Only strictly newer signed update sets can be installed."));
    }

    Ok(())
}

/// Atomically raises the recorded sequence floor to `max(existing, sequence)`.
///
/// Never lowers the floor and refuses to overwrite a corrupted floor file.
pub fn commit_sequence_floor(floor_path: &Path, sequence: u64) -> Result<(), ErrorBody> {
    if sequence == 0 {
        return Ok(());
    }
    match read_sequence_floor(floor_path) {
        SequenceFloorState::Invalid(detail) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Sequence floor was not updated because {detail}."),
            ));
        }
        SequenceFloorState::Recorded(existing) if sequence <= existing => {
            return Ok(());
        }
        SequenceFloorState::Missing | SequenceFloorState::Recorded(_) => {}
    }

    write_sequence_floor_file(floor_path, sequence)
}

/// Ensures the sequence floor file is readable and at least `running_sequence`.
///
/// If the file is damaged, malformed, oversized, or a symlink, it is atomically rebuilt at
/// `running_sequence` (which is compiled into the running binary and can never lower protection
/// below the running build) and the original corruption detail is returned so callers can surface
/// it truthfully instead of silently claiming everything is fine.
///
/// Refuses to act on sequence-0 builds or if `floor_path` is a directory.
pub fn recover_sequence_floor(
    floor_path: &Path,
    running_sequence: u64,
) -> Result<(u64, Option<String>), ErrorBody> {
    if running_sequence == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "This build has sequence 0 and cannot maintain a sequence floor.",
        ));
    }
    match read_sequence_floor(floor_path) {
        SequenceFloorState::Missing => {
            write_sequence_floor_file(floor_path, running_sequence)?;
            Ok((running_sequence, None))
        }
        SequenceFloorState::Recorded(existing) => {
            if running_sequence > existing {
                write_sequence_floor_file(floor_path, running_sequence)?;
                Ok((running_sequence, None))
            } else {
                Ok((existing, None))
            }
        }
        SequenceFloorState::Invalid(detail) => {
            if let Ok(meta) = fs::symlink_metadata(floor_path)
                && meta.file_type().is_dir()
            {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Sequence floor path {} is a directory and cannot be rebuilt automatically.",
                        floor_path.display()
                    ),
                ));
            }
            write_sequence_floor_file(floor_path, running_sequence)?;
            Ok((running_sequence, Some(detail)))
        }
    }
}

fn write_sequence_floor_file(floor_path: &Path, sequence: u64) -> Result<(), ErrorBody> {
    let parent = floor_path.parent().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!(
                "Sequence floor path {} has no parent directory.",
                floor_path.display()
            ),
        )
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create update state directory {}: {err}.",
                parent.display()
            ),
        )
    })?;

    let record = SequenceFloorRecord {
        schema: 1,
        sequence,
        updated_at: Timestamp::now().to_string(),
    };
    let mut payload = serde_json::to_vec_pretty(&record).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not serialize sequence floor: {err}."),
        )
    })?;
    payload.push(b'\n');

    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create temporary sequence floor in {}: {err}.",
                parent.display()
            ),
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not set permissions on sequence floor: {err}."),
                )
            })?;
    }

    tmp.write_all(&payload).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not write sequence floor: {err}."),
        )
    })?;
    tmp.as_file().sync_all().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not flush sequence floor: {err}."),
        )
    })?;
    tmp.persist(floor_path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not replace sequence floor {}: {}.",
                floor_path.display(),
                err.error
            ),
        )
    })?;
    Ok(())
}
