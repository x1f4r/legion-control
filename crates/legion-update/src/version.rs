//! Sequence-number versioning and monotonic platform version derivation.
//!
//! `sequence` is a positive whole number that only grows (in normal builds, UTC build time as
//! `YYYYMMDDHHMM`, for example `202611250137`). Platform version numbers are derived strictly
//! monotonically from `sequence` so OS downgrade protection agrees with Legion's own sequence rule:
//!
//! - **Android `versionCode`** (32-bit signed integer `1..=2_100_000_000`): a `YYYYMMDDHHMM`
//!   sequence (`202001010000..=299912312359`) maps to UTC minutes since Unix epoch
//!   (`26_297_280..=541_723_679`). Small synthetic test sequences (`1..=20_000_000`) map directly.
//! - **macOS `CFBundleVersion`**: the decimal `sequence` string itself.
//! - **Desktop bundle version**: `<base_version>+<sequence>` (valid SemVer 2.0.0 and monotonically
//!   ordered by `dpkg --compare-versions`).

use legion_proto::{ErrorBody, ErrorCode};

/// Maximum synthetic sequence mapped 1-to-1 to Android `versionCode` (strictly below the Unix
/// epoch minute for `2020-01-01T00:00Z`, which is `26_297_280`).
const MAX_SMALL_SEQUENCE: u64 = 20_000_000;

/// Minimum calendar sequence accepted in `YYYYMMDDHHMM` form (`2020-01-01 00:00 UTC`).
const MIN_CALENDAR_SEQUENCE: u64 = 202_001_010_000;

/// Maximum calendar sequence accepted in `YYYYMMDDHHMM` form (`2999-12-31 23:59 UTC`).
const MAX_CALENDAR_SEQUENCE: u64 = 299_912_312_359;

/// Maximum calendar horizon (in days) that a candidate calendar sequence may jump ahead of the
/// running build's calendar sequence (10 years). This stops a compromised key from permanently
/// pinning devices at a far-future sequence such as `299912312359` without trusting the device RTC.
pub const MAX_FUTURE_HORIZON_DAYS: i64 = 3650;

/// Build sequence compiled into this crate via `LEGION_BUILD_SEQUENCE`, or `0` when unset.
pub const fn compiled_sequence() -> u64 {
    parse_const_u64(env!("LEGION_UPDATE_SEQUENCE"))
}

static SEQUENCE_MARKER_BYTES: &[u8] = concat!(
    "LEGION_UPDATE_SEQUENCE=",
    env!("LEGION_UPDATE_SEQUENCE"),
    "\0"
)
.as_bytes();

/// NUL-terminated static marker compiled into binaries so packaging tools can verify the binary's
/// embedded update sequence without executing it.
pub fn compiled_sequence_marker() -> &'static [u8] {
    SEQUENCE_MARKER_BYTES
}

/// Parses a sequence number as either a small synthetic test sequence (`Ok(None)`) or a valid UTC
/// `YYYYMMDDHHMM` calendar timestamp (`Ok(Some(unix_minutes))`).
pub fn sequence_to_unix_minutes(sequence: u64) -> Result<Option<i64>, ErrorBody> {
    if sequence == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Update sequence must be greater than 0.",
        ));
    }
    if sequence <= MAX_SMALL_SEQUENCE {
        return Ok(None);
    }
    if (MIN_CALENDAR_SEQUENCE..=MAX_CALENDAR_SEQUENCE).contains(&sequence) {
        let minute = (sequence % 100) as u32;
        let hour = ((sequence / 100) % 100) as u32;
        let day = ((sequence / 10_000) % 100) as u32;
        let month = ((sequence / 1_000_000) % 100) as u32;
        let year = (sequence / 100_000_000) as i64;

        if !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
        {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Sequence {sequence} is not a valid UTC YYYYMMDDHHMM timestamp."),
            )
            .with_hint(
                "Use a UTC timestamp formatted as YYYYMMDDHHMM (for example 202611250130).",
            ));
        }

        let days = days_from_civil(year, month, day);
        let unix_minutes = days * 1440 + i64::from(hour) * 60 + i64::from(minute);
        return Ok(Some(unix_minutes));
    }

    Err(ErrorBody::new(
        ErrorCode::InvalidArgs,
        format!(
            "Sequence {sequence} is not a valid update sequence: pass a UTC YYYYMMDDHHMM timestamp or a test sequence in 1..={MAX_SMALL_SEQUENCE}."
        ),
    ))
}

/// Rejects malformed sequences and implausibly far-future calendar sequences.
///
/// When both `running_sequence` and `candidate` are calendar `YYYYMMDDHHMM` timestamps,
/// `candidate` may be at most [`MAX_FUTURE_HORIZON_DAYS`] (10 years) ahead of `running_sequence`.
/// When `running_sequence` is a small synthetic sequence (`1..=20_000_000`), calendar candidates
/// past year `2099` are also refused.
pub fn check_sequence_plausibility(candidate: u64, running_sequence: u64) -> Result<(), ErrorBody> {
    let cand_minutes = sequence_to_unix_minutes(candidate)?;
    let run_minutes = if running_sequence > 0 {
        sequence_to_unix_minutes(running_sequence)?
    } else {
        None
    };

    match (cand_minutes, run_minutes) {
        (Some(cand_m), Some(run_m)) => {
            let max_delta_minutes = MAX_FUTURE_HORIZON_DAYS * 1440;
            if cand_m - run_m > max_delta_minutes {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing update set (sequence {candidate}): it is more than {MAX_FUTURE_HORIZON_DAYS} days ahead of the running build (sequence {running_sequence})."
                    ),
                )
                .with_hint("Use a current UTC YYYYMMDDHHMM build sequence."));
            }
        }
        (Some(_), None) if candidate >= 210_001_010_000 => {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Refusing update set (sequence {candidate}): calendar sequence is implausibly far in the future."
                ),
            )
            .with_hint("Use a current UTC YYYYMMDDHHMM build sequence."));
        }
        _ => {}
    }

    Ok(())
}

const fn parse_const_u64(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut val = 0u64;
    let mut i = 0;
    while i < bytes.len() {
        let d = bytes[i].wrapping_sub(b'0');
        val = val * 10 + d as u64;
        i += 1;
    }
    val
}

/// Derives a strictly monotonic Android `versionCode` (signed 32-bit integer) from `sequence`.
pub fn android_version_code(sequence: u64) -> Result<i32, ErrorBody> {
    match sequence_to_unix_minutes(sequence)? {
        None => i32::try_from(sequence).map_err(|_| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Sequence {sequence} overflows Android versionCode."),
            )
        }),
        Some(unix_minutes) => i32::try_from(unix_minutes).map_err(|_| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Sequence {sequence} overflows Android versionCode."),
            )
        }),
    }
}

/// Derives the macOS `CFBundleVersion` string from `sequence`.
pub fn macos_bundle_version(sequence: u64) -> Result<String, ErrorBody> {
    if sequence == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Update sequence must be greater than 0.",
        ));
    }
    Ok(sequence.to_string())
}

/// Derives the desktop bundle version string (`<base_version>+<sequence>`) from a base SemVer
/// version and `sequence`.
pub fn desktop_bundle_version(base_version: &str, sequence: u64) -> Result<String, ErrorBody> {
    if sequence == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Update sequence must be greater than 0.",
        ));
    }
    let clean = base_version
        .trim()
        .split_once('+')
        .map_or(base_version.trim(), |(base, _)| base);
    if clean.is_empty()
        || !clean
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Base version {base_version:?} is not valid."),
        ));
    }
    Ok(format!("{clean}+{sequence}"))
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's civil-to-days algorithm: days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
