//! Records a build-time UTC timestamp (`LEGION_BUILD_TIME_SECS`) so devices booting with an unset
//! system clock (such as a Raspberry Pi without an RTC at 1970-01-01) never stamp shared
//! `favorites.toml` edits far in the past.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let secs: i64 = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
                .unwrap_or(0)
        });
    println!("cargo:rustc-env=LEGION_BUILD_TIME_SECS={secs}");
}
