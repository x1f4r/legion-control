//! Update set manifest (`update.json`), platform descriptors, and artifact selection.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};

use crate::checksums::validate_entry_name;
use crate::trust::{CHECKSUMS_FILENAME, MANIFEST_FILENAME, SIGNATURE_FILENAME};

/// Maximum accepted size of `update.json` (256 KiB).
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Maximum accepted size of release `notes` inside `update.json` (16 KiB).
pub const MAX_NOTES_BYTES: usize = 16 * 1024;

/// Maximum number of artifacts listed in one update set.
pub const MAX_ARTIFACTS: usize = 64;

/// Maximum accepted size of a single update artifact (512 MiB).
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Update channel for a signed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Official releases signed offline with the release key.
    Stable,
    /// Preview builds signed with the preview key compiled via `LEGION_PREVIEW_UPDATE_KEY`.
    Preview,
}

impl Channel {
    /// Lowercase wire and directory name (`"stable"` or `"preview"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }

    /// All supported channels in deterministic order.
    pub const ALL: [Channel; 2] = [Channel::Stable, Channel::Preview];
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Channel {
    type Err = ErrorBody;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stable" => Ok(Self::Stable),
            "preview" => Ok(Self::Preview),
            other => Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Unknown update channel {other:?}."),
            )
            .with_hint("Use \"stable\" or \"preview\".")),
        }
    }
}

/// Which Legion program an artifact installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Program {
    /// The `legion` node binary or package.
    Node,
    /// The `Legion` native app for phones, tablets, or desktops.
    App,
}

impl Program {
    /// Lowercase wire name (`"node"` or `"app"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::App => "app",
        }
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Operating system targeted by an artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Os {
    /// Linux desktop or server.
    Linux,
    /// macOS desktop.
    Macos,
    /// Windows desktop or server.
    Windows,
    /// Android phone or tablet.
    Android,
    /// iPhone or iPad (cannot self-update outside App Store / TestFlight).
    Ios,
}

impl Os {
    /// Lowercase wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }

    /// Operating system of the currently running binary.
    pub const fn current() -> Option<Self> {
        if cfg!(target_os = "android") {
            Some(Self::Android)
        } else if cfg!(target_os = "ios") {
            Some(Self::Ios)
        } else if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CPU architecture targeted by an artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum Arch {
    /// 64-bit x86 (`amd64` / `x64`).
    #[serde(rename = "x86_64")]
    X86_64,
    /// 64-bit ARM (`arm64` / `arm64-v8a`).
    #[serde(rename = "aarch64")]
    Aarch64,
    /// Multi-architecture bundle (for example a universal Android APK or macOS universal bundle).
    #[serde(rename = "universal")]
    Universal,
}

impl Arch {
    /// Wire name (`"x86_64"`, `"aarch64"`, or `"universal"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Universal => "universal",
        }
    }

    /// CPU architecture of the currently running binary.
    pub const fn current() -> Option<Self> {
        if cfg!(target_arch = "x86_64") {
            Some(Self::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Aarch64)
        } else {
            None
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an artifact is installed on its target platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum InstallKind {
    /// Standalone `legion` node executable.
    #[serde(rename = "binary")]
    Binary,
    /// Debian/Ubuntu `.deb` package (never self-replaced; reports the apt command to run).
    #[serde(rename = "deb")]
    Deb,
    /// Signed Android `.apk`.
    #[serde(rename = "apk")]
    Apk,
    /// Linux self-contained `.AppImage`.
    #[serde(rename = "appimage")]
    AppImage,
    /// Packed macOS `.app` bundle (`.tar` or `.tar.gz`).
    #[serde(rename = "app-tar")]
    AppTar,
    /// Windows NSIS installer `.exe`.
    #[serde(rename = "nsis")]
    Nsis,
}

impl InstallKind {
    /// Wire name (`"binary"`, `"deb"`, `"apk"`, `"appimage"`, `"app-tar"`, `"nsis"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Deb => "deb",
            Self::Apk => "apk",
            Self::AppImage => "appimage",
            Self::AppTar => "app-tar",
            Self::Nsis => "nsis",
        }
    }

    /// Whether this install kind can replace itself directly without an external package manager.
    pub const fn is_self_replaceable(self) -> bool {
        matches!(
            self,
            Self::Binary | Self::Apk | Self::AppImage | Self::AppTar | Self::Nsis
        )
    }
}

impl fmt::Display for InstallKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One installable file declared inside `update.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Bare filename inside the update set directory, also listed in `SHA256SUMS`.
    pub name: String,
    /// Whether this file is a `node` or `app` artifact.
    pub program: Program,
    /// Target operating system.
    pub os: Os,
    /// Target CPU architecture.
    pub arch: Arch,
    /// Installation mechanism for this artifact.
    pub kind: InstallKind,
    /// Exact artifact file size in bytes (covered by `SHA256SUMS` signature via `update.json`).
    pub size: u64,
}

impl Artifact {
    /// Validates the artifact's filename, size bounds, and `(program, os, kind)` compatibility.
    pub fn validate(&self) -> Result<(), ErrorBody> {
        validate_entry_name(&self.name)?;
        if self.size == 0 || self.size > MAX_ARTIFACT_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Artifact {:?} declares invalid size {} bytes (must be 1..={MAX_ARTIFACT_BYTES}).",
                    self.name, self.size
                ),
            ));
        }
        if matches!(
            self.name.as_str(),
            MANIFEST_FILENAME | CHECKSUMS_FILENAME | SIGNATURE_FILENAME
        ) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Artifact name {:?} is reserved for update set metadata.",
                    self.name
                ),
            ));
        }
        let valid_combo = matches!(
            (self.program, self.os, self.kind),
            (
                Program::Node,
                Os::Linux | Os::Macos | Os::Windows,
                InstallKind::Binary
            ) | (Program::Node, Os::Linux, InstallKind::Deb)
                | (Program::App, Os::Android, InstallKind::Apk)
                | (
                    Program::App,
                    Os::Linux,
                    InstallKind::AppImage | InstallKind::Deb
                )
                | (Program::App, Os::Macos, InstallKind::AppTar)
                | (Program::App, Os::Windows, InstallKind::Nsis)
        );
        if !valid_combo {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "Artifact {:?} declares unsupported combination program={} os={} kind={}.",
                    self.name, self.program, self.os, self.kind
                ),
            ));
        }
        Ok(())
    }
}

/// Parsed `update.json` metadata describing one update set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    /// Update channel (`stable` or `preview`).
    pub channel: Channel,
    /// Human-readable semantic version (for example `0.2.0`).
    pub version: String,
    /// Source revision or build identifier (for example `a59ec0d12345`).
    pub build: String,
    /// Strictly increasing whole number (UTC build time as `YYYYMMDDHHMM`).
    pub sequence: u64,
    /// RFC 3339 UTC timestamp when this set was published.
    pub published_at: String,
    /// Plain-language release notes shown to the user before or after updating.
    pub notes: String,
    /// Installable artifacts included in this set.
    pub artifacts: Vec<Artifact>,
}

/// Platform and installation kind requested when selecting an artifact from a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetPlatform {
    /// Program to update (`node` or `app`).
    pub program: Program,
    /// Running operating system.
    pub os: Os,
    /// Running CPU architecture.
    pub arch: Arch,
    /// Running installation kind.
    pub kind: InstallKind,
}

impl UpdateManifest {
    /// Parses and strictly validates `update.json` bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, ErrorBody> {
        if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{MANIFEST_FILENAME} is empty or exceeds {MAX_MANIFEST_BYTES} bytes."),
            ));
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{MANIFEST_FILENAME} is not valid JSON: {err}."),
            )
            .with_hint("Regenerate the update set with scripts/make-update-set.mjs.")
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates every semantic invariant of `update.json`.
    pub fn validate(&self) -> Result<(), ErrorBody> {
        if self.sequence == 0 {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{MANIFEST_FILENAME} sequence must be a positive integer."),
            ));
        }
        let version = self.version.trim();
        if version.is_empty()
            || version.len() > 64
            || version != self.version
            || version.contains("..")
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
        {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{MANIFEST_FILENAME} has an invalid version {:?}.",
                    self.version
                ),
            ));
        }
        if self.build.trim().is_empty() || self.build.len() > 128 {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{MANIFEST_FILENAME} must carry a non-empty build identifier (at most 128 bytes)."
                ),
            ));
        }
        if self.published_at.len() > 64 || self.published_at.parse::<Timestamp>().is_err() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{MANIFEST_FILENAME} published_at {:?} is not a valid RFC 3339 UTC timestamp.",
                    self.published_at
                ),
            ));
        }
        if self.notes.len() > MAX_NOTES_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{MANIFEST_FILENAME} notes exceed the {MAX_NOTES_BYTES}-byte limit."),
            ));
        }
        if self.artifacts.is_empty() || self.artifacts.len() > MAX_ARTIFACTS {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{MANIFEST_FILENAME} must list between 1 and {MAX_ARTIFACTS} artifacts (found {}).",
                    self.artifacts.len()
                ),
            ));
        }
        let mut seen_names = BTreeSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !seen_names.insert(artifact.name.as_str()) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "{MANIFEST_FILENAME} lists artifact {:?} more than once.",
                        artifact.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Selects the single artifact matching `target`, preferring an exact architecture match over
    /// a universal bundle (and allowing x86_64 emulation on Windows ARM64 when no native arm64
    /// artifact is present). Refuses ambiguous sets where more than one artifact matches.
    pub fn select_artifact(&self, target: TargetPlatform) -> Result<&Artifact, ErrorBody> {
        let same_shape = |a: &&Artifact| {
            a.program == target.program && a.os == target.os && a.kind == target.kind
        };

        let exact: Vec<&Artifact> = self
            .artifacts
            .iter()
            .filter(|a| same_shape(a) && a.arch == target.arch)
            .collect();
        if let Some(chosen) = pick_unique_artifact(&exact, self, target, "exact")? {
            return Ok(chosen);
        }

        if matches!(target.os, Os::Android | Os::Macos) {
            let universal: Vec<&Artifact> = self
                .artifacts
                .iter()
                .filter(|a| same_shape(a) && a.arch == Arch::Universal)
                .collect();
            if let Some(chosen) = pick_unique_artifact(&universal, self, target, "universal")? {
                return Ok(chosen);
            }
        }

        if target.os == Os::Windows && target.arch == Arch::Aarch64 {
            let emulated: Vec<&Artifact> = self
                .artifacts
                .iter()
                .filter(|a| same_shape(a) && a.arch == Arch::X86_64)
                .collect();
            if let Some(chosen) = pick_unique_artifact(&emulated, self, target, "x86_64")? {
                return Ok(chosen);
            }
        }

        Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "Update set {} (sequence {}) does not contain an artifact for {} on {}/{} ({}).",
                self.version, self.sequence, target.program, target.os, target.arch, target.kind
            ),
        )
        .with_hint(
            "Build and include an artifact for this platform and install kind in the update set.",
        ))
    }
}

fn pick_unique_artifact<'a>(
    matches: &[&'a Artifact],
    manifest: &UpdateManifest,
    target: TargetPlatform,
    tier: &str,
) -> Result<Option<&'a Artifact>, ErrorBody> {
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        n => Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Update set {} (sequence {}) has {n} ambiguous {tier} artifacts for {} on {}/{} ({}).",
                manifest.version,
                manifest.sequence,
                target.program,
                target.os,
                target.arch,
                target.kind
            ),
        )
        .with_hint(
            "Include at most one artifact per platform, architecture, and install kind.",
        )),
    }
}
