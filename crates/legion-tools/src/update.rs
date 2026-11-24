//! `meta.update` and shared release artifact download, verification, and caching.
//!
//! Both `meta.update` (self-updating this node) and `fleet.install` (pushing `legion` to another
//! machine over SSH) pull release binaries and `SHA256SUMS` from the same GitHub repository,
//! verify SHA-256 digests before touching any destination, and cache verified downloads under
//! `<state_dir>/dist/<version>/<target>/`.
//!
//! Network I/O shells out to the system `curl` (or PowerShell `Invoke-WebRequest` on Windows when
//! `curl` is absent) through `legion-os::run` rather than embedding an HTTP/TLS stack, keeping
//! the static binary small and inheriting system certificates and proxy configuration.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{BoxFuture, CancellationToken, Ctx, Paths, Registry, ToolError, VERSION};
use legion_os::OsError;
use legion_os::atomic::{WriteOptions, replace_file};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::binfmt::verify_binary_matches_target;
use crate::floor::{SemVer, check_version_transition, commit_floor, node_floor_path};
use crate::trust::{RELEASE_PUBLIC_KEY, SIGNATURE_FILENAME, verify_signature_with_key};

/// The single repository URL constant from which release metadata and binaries are fetched.
pub const RELEASE_REPO_URL: &str = "https://github.com/x1f4r/legion-control";

/// Prefix used by Legion release tags so they stay separate from other releases in the repository.
pub const RELEASE_TAG_PREFIX: &str = "legion-v";

/// Name of the checksum manifest attached to every Legion release.
pub const CHECKSUMS_FILENAME: &str = "SHA256SUMS";

/// Timeout for querying the GitHub releases API.
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for downloading one release artifact or `SHA256SUMS`.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Marker inserted into renamed Windows executables so startup cleanup only removes our backups.
const OLD_BINARY_MARKER: &str = ".old.";

/// Timeout for running a staged `legion --version` preflight check before swapping it into place.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// Fixed PowerShell snippet for fetching the GitHub releases JSON without interpolating any input.
const PS_FETCH_API_SCRIPT: &str = "$ProgressPreference = 'SilentlyContinue'; \
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; \
(Invoke-WebRequest -Uri $env:LEGION_DOWNLOAD_URL -Headers @{ 'Accept'='application/vnd.github+json'; 'User-Agent'='legion' } -UseBasicParsing).Content";

/// Fixed PowerShell snippet for downloading one URL into `$env:LEGION_DOWNLOAD_DEST`.
const PS_DOWNLOAD_FILE_SCRIPT: &str = "$ProgressPreference = 'SilentlyContinue'; \
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; \
Invoke-WebRequest -Uri $env:LEGION_DOWNLOAD_URL -OutFile $env:LEGION_DOWNLOAD_DEST -Headers @{ 'User-Agent'='legion' } -UseBasicParsing";

/// Registers `meta.update` on `registry`.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "meta.update",
        Effect::Admin,
        "Replace this node's legion binary with an offline-signed GitHub release artifact or a local binary passed with `from`. Refuses installations managed by a system package manager (such as `deb` or `pacman`) and points at the package manager's upgrade command instead. Release downloads require a valid Ed25519 SHA256SUMS.sig matching the public key compiled into legion (no flag can bypass it) and SHA-256 verification; passing `from=<local file>` installs an unsigned binary that the admin naming the file vouches for. Checks binary format and architecture, enforces downgrade protection unless `allow_downgrade` is set, stages and runs `--version` preflight, keeps the previous binary as `.prev` (restoring it if the replaced binary does not run), and replaces atomically. On Windows, renames the running executable aside first so it can be cleaned up on the next start.",
        meta_update,
    );
}

/// Arguments of `meta.update`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateArgs {
    /// Release version to install (for example `0.2.0` or `legion-v0.2.0`). Defaults to the
    /// highest published stable `legion-v*` semantic version. Cannot be combined with `from`.
    #[serde(default)]
    pub version: Option<String>,
    /// Local path on this node to a `legion` binary already built for this machine. Cannot be
    /// combined with `version`.
    #[serde(default)]
    pub from: Option<String>,
    /// Resolve the target release or local binary and report what would be replaced without
    /// downloading or writing.
    #[serde(default)]
    pub dry_run: bool,
    /// Permit replacing with a version older than the running binary or the recorded version floor.
    #[serde(default)]
    pub allow_downgrade: bool,
}

/// Result of `meta.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateResult {
    /// Version this node was running before the update.
    pub previous_version: String,
    /// Version installed (or planned when `dry_run` is true).
    pub installed_version: String,
    /// Source revision reported by the newly installed binary, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Release target triple selected for this node.
    pub target: String,
    /// Path to the executable that was (or would be) replaced.
    pub path: String,
    /// Whether this call only reported what would happen without modifying the binary.
    pub dry_run: bool,
    /// Whether the executable on disk was replaced.
    pub updated: bool,
    /// Optional platform note (for example Rosetta 2 native selection or Windows ARM64 emulation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Resolved release architecture, target triple, and optional user-facing platform note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Target architecture of the selected release binary (`x86_64` or `aarch64`).
    pub arch: &'static str,
    /// Release target triple published by CI.
    pub target: &'static str,
    /// Optional explanation when selecting a native or emulated build (Rosetta 2 / Windows ARM64).
    pub note: Option<&'static str>,
}

/// Network boundary for discovering and downloading Legion releases.
///
/// Kept as one small trait so both `meta.update` and `fleet.install` tests inject a fake without
/// making network connections.
pub trait ReleaseFetcher: Send + Sync {
    /// Returns the newest published `legion-v*` version (without the `legion-v` prefix).
    fn latest_version<'a>(
        &'a self,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, ErrorBody>>;

    /// Downloads `url` into `dest`.
    fn download_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<(), ErrorBody>>;
}

/// Production [`ReleaseFetcher`] backed by the system `curl` (and PowerShell fallback on Windows)
/// executed with bounded timeouts through `legion-os::run`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemFetcher;

impl ReleaseFetcher for SystemFetcher {
    fn latest_version<'a>(
        &'a self,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, ErrorBody>> {
        Box::pin(async move {
            let url = releases_api_url();
            let body = fetch_api_text(&url, cancel).await?;
            parse_latest_release_version(&body)
        })
    }

    fn download_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<(), ErrorBody>> {
        Box::pin(async move { download_url_to_file(url, dest, cancel).await })
    }
}

/// GitHub API URL for listing the repository's releases.
pub fn releases_api_url() -> String {
    let slug = RELEASE_REPO_URL
        .strip_prefix("https://github.com/")
        .unwrap_or(RELEASE_REPO_URL);
    format!("https://api.github.com/repos/{slug}/releases?per_page=30")
}

/// Tag name for a normalized release version (for example `"0.1.0"` -> `"legion-v0.1.0"`).
pub fn release_tag(version: &str) -> String {
    format!("{RELEASE_TAG_PREFIX}{version}")
}

/// Asset filename for a release binary matching `version` and `target`.
pub fn artifact_filename(version: &str, target: &str) -> String {
    if target.contains("windows") {
        format!("legion-{version}-{target}.exe")
    } else {
        format!("legion-{version}-{target}")
    }
}

/// Download URL for a release binary matching `version` and `target`.
pub fn artifact_url(version: &str, target: &str) -> String {
    let tag = release_tag(version);
    let file = artifact_filename(version, target);
    format!("{RELEASE_REPO_URL}/releases/download/{tag}/{file}")
}

/// Download URL for a release's `SHA256SUMS` manifest.
pub fn checksums_url(version: &str) -> String {
    let tag = release_tag(version);
    format!("{RELEASE_REPO_URL}/releases/download/{tag}/{CHECKSUMS_FILENAME}")
}

/// Download URL for a release's detached Ed25519 signature over `SHA256SUMS`.
pub fn checksums_sig_url(version: &str) -> String {
    let tag = release_tag(version);
    format!("{RELEASE_REPO_URL}/releases/download/{tag}/{SIGNATURE_FILENAME}")
}

/// Validates and normalizes a release version, stripping an optional `legion-v` or `v` prefix.
pub fn normalize_version(raw: &str) -> Result<String, ErrorBody> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix(RELEASE_TAG_PREFIX)
        .or_else(|| trimmed.strip_prefix('v'))
        .unwrap_or(trimmed);
    if stripped.is_empty()
        || stripped.contains("..")
        || !stripped
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Version {raw:?} is not a valid release version."),
        )
        .with_hint(
            "Pass a version such as `0.1.0` or `legion-v0.1.0` using only letters, digits, `.`, `_`, `+`, and `-`.",
        ));
    }
    Ok(stripped.to_owned())
}

/// Selects the highest published stable `legion-v*` semantic version from GitHub's `/releases`
/// JSON, ignoring drafts, GitHub prereleases, and SemVer pre-release tags regardless of creation
/// order.
pub fn parse_latest_release_version(releases_json: &str) -> Result<String, ErrorBody> {
    #[derive(Deserialize)]
    struct ReleaseEntry {
        tag_name: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
    }

    let entries: Vec<ReleaseEntry> = serde_json::from_str(releases_json).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not parse GitHub releases response: {err}."),
        )
        .with_hint("Check network connectivity to api.github.com or pass an explicit `version`.")
    })?;

    let mut highest: Option<SemVer> = None;
    for entry in entries {
        if entry.draft || entry.prerelease || !entry.tag_name.starts_with(RELEASE_TAG_PREFIX) {
            continue;
        }
        let Ok(ver) = SemVer::parse(&entry.tag_name) else {
            continue;
        };
        if ver.is_prerelease() {
            continue;
        }
        if highest.as_ref().is_none_or(|best| ver > *best) {
            highest = Some(ver);
        }
    }

    if let Some(best) = highest {
        return Ok(best.to_string());
    }

    Err(ErrorBody::new(
        ErrorCode::NotFound,
        format!(
            "No published stable {RELEASE_TAG_PREFIX}* release was found at {RELEASE_REPO_URL}/releases."
        ),
    )
    .with_hint("Specify an explicit `version` or install from a local binary with `from`."))
}

/// Maps a normalized `(os, arch)` pair (plus whether macOS is running under Rosetta 2 translation)
/// to the release target triple published by CI and any user-facing note.
pub fn resolve_target_for(
    os: &str,
    arch: &str,
    macos_translated: bool,
) -> Result<ResolvedTarget, ErrorBody> {
    match (os, arch) {
        ("linux", "x86_64") => Ok(ResolvedTarget {
            arch: "x86_64",
            target: "x86_64-unknown-linux-musl",
            note: None,
        }),
        ("linux", "aarch64") => Ok(ResolvedTarget {
            arch: "aarch64",
            target: "aarch64-unknown-linux-musl",
            note: None,
        }),
        ("macos", "aarch64") => Ok(ResolvedTarget {
            arch: "aarch64",
            target: "aarch64-apple-darwin",
            note: None,
        }),
        ("macos", "x86_64") if macos_translated => Ok(ResolvedTarget {
            arch: "aarch64",
            target: "aarch64-apple-darwin",
            note: Some(
                "Rosetta 2 translation detected; selecting native Apple Silicon (aarch64-apple-darwin) build.",
            ),
        }),
        ("macos", "x86_64") => Ok(ResolvedTarget {
            arch: "x86_64",
            target: "x86_64-apple-darwin",
            note: None,
        }),
        ("windows", "x86_64") => Ok(ResolvedTarget {
            arch: "x86_64",
            target: "x86_64-pc-windows-msvc",
            note: None,
        }),
        ("windows", "aarch64") => Ok(ResolvedTarget {
            arch: "x86_64",
            target: "x86_64-pc-windows-msvc",
            note: Some(
                "Windows ARM64 detected; selecting x86_64-pc-windows-msvc under x64 emulation because native Windows ARM64 release binaries are not published yet.",
            ),
        }),
        ("linux", "arm" | "armv6" | "armv6l" | "armv7" | "armv7l" | "armhf" | "armel") => {
            Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "32-bit ARM Linux ({arch}) is not supported; Legion publishes 64-bit Linux binaries (x86_64 and aarch64)."
                ),
            )
            .with_hint("Use a 64-bit OS image (aarch64) on ARM boards such as Raspberry Pi."))
        }
        _ => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!("No Legion release binary is published for {os}/{arch}."),
        )
        .with_hint(
            "Supported release targets are Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).",
        )),
    }
}

/// Maps a normalized `(os, arch)` pair to the release target triple published by CI.
pub fn target_triple_for(os: &str, arch: &str) -> Result<&'static str, ErrorBody> {
    resolve_target_for(os, arch, false).map(|resolved| resolved.target)
}

/// Computes the lowercase hex SHA-256 digest of a file on disk.
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
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Finds the expected lowercase SHA-256 hex digest for `asset_name` inside a `SHA256SUMS` file.
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
            if hash_raw.len() != 64 || !hash_raw.bytes().all(|b| b.is_ascii_hexdigit()) {
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

/// Directory under `<state_dir>/dist/<version>/<target>/` where downloaded release binaries live.
pub fn dist_cache_dir(paths: &Paths, version: &str, target: &str) -> PathBuf {
    paths.state_dir().join("dist").join(version).join(target)
}

/// Path to the cached release binary for `(version, target)` under `<state_dir>/dist/`.
pub fn dist_cache_binary(paths: &Paths, version: &str, target: &str) -> PathBuf {
    dist_cache_dir(paths, version, target).join(artifact_filename(version, target))
}

/// Returns a verified local path to the release binary for `(version, target)`, using the cache
/// under `<state_dir>/dist/<version>/<target>/` only when a valid signed `SHA256SUMS` +
/// `SHA256SUMS.sig` sits beside it, or downloading and verifying both the Ed25519 signature and
/// SHA-256 checksum through `fetcher`.
pub async fn resolve_release_binary(
    paths: &Paths,
    fetcher: &dyn ReleaseFetcher,
    version: &str,
    target: &str,
    cancel: CancellationToken,
) -> Result<PathBuf, ErrorBody> {
    resolve_release_binary_with_key(paths, fetcher, version, target, &RELEASE_PUBLIC_KEY, cancel)
        .await
}

/// Like [`resolve_release_binary`], parameterized on the Ed25519 public key for cross-crate tests.
#[doc(hidden)]
pub async fn resolve_release_binary_with_key(
    paths: &Paths,
    fetcher: &dyn ReleaseFetcher,
    version: &str,
    target: &str,
    public_key: &[u8; 32],
    cancel: CancellationToken,
) -> Result<PathBuf, ErrorBody> {
    let version = normalize_version(version)?;
    let asset = artifact_filename(&version, target);
    let cache_dir = dist_cache_dir(paths, &version, target);
    let cached_bin = cache_dir.join(&asset);
    let cached_sums = cache_dir.join(CHECKSUMS_FILENAME);
    let cached_sig = cache_dir.join(SIGNATURE_FILENAME);

    // A cached binary is ONLY trusted when its signed SHA256SUMS and SHA256SUMS.sig sit beside it
    // and both the Ed25519 signature and the binary's SHA-256 verify cleanly.
    if let Ok(meta) = fs::metadata(&cached_bin)
        && meta.is_file()
        && meta.len() > 0
        && let Ok(sums_bytes) = fs::read(&cached_sums)
        && let Ok(sig_text) = fs::read_to_string(&cached_sig)
        && verify_signature_with_key(&sums_bytes, &sig_text, public_key).is_ok()
        && let Ok(sums_text) = String::from_utf8(sums_bytes)
    {
        let bin_check = cached_bin.clone();
        let asset_check = asset.clone();
        let verified = tokio::task::spawn_blocking(move || {
            verify_file_checksum(&bin_check, &sums_text, &asset_check)
        })
        .await
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Checksum verification stopped unexpectedly: {err}."),
            )
        })?;
        if verified.is_ok() {
            return Ok(cached_bin);
        }
    }

    fs::create_dir_all(&cache_dir).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot create dist cache directory {}: {err}.",
                cache_dir.display()
            ),
        )
        .with_hint("Make Legion's state directory writable by the account legion runs as.")
    })?;

    let token = random_hex(8)?;
    let tmp_sums = cache_dir.join(format!(".{CHECKSUMS_FILENAME}.tmp.{token}"));
    let tmp_sig = cache_dir.join(format!(".{SIGNATURE_FILENAME}.tmp.{token}"));
    let tmp_bin = cache_dir.join(format!(".{asset}.tmp.{token}"));
    let mut sums_guard = TempCleanup::new(tmp_sums.clone());
    let mut sig_guard = TempCleanup::new(tmp_sig.clone());
    let mut bin_guard = TempCleanup::new(tmp_bin.clone());

    let sums_url = checksums_url(&version);
    fetcher
        .download_to_file(&sums_url, &tmp_sums, cancel.clone())
        .await?;

    let sig_url = checksums_sig_url(&version);
    if let Err(err) = fetcher
        .download_to_file(&sig_url, &tmp_sig, cancel.clone())
        .await
    {
        if err.code == ErrorCode::Cancelled || err.code == ErrorCode::Timeout {
            return Err(err);
        }
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing unsigned release {version}: {SIGNATURE_FILENAME} could not be fetched from {sig_url} ({}).",
                err.message
            ),
        )
        .with_hint(
            "Sign the release offline with scripts/sign-legion-release.mjs to upload SHA256SUMS.sig, or pass `from=<local path>` to install a local binary you vouch for.",
        ));
    }

    // Verify the detached Ed25519 signature over the exact bytes of SHA256SUMS before downloading
    // or trusting any release binary.
    let sums_bytes = fs::read(&tmp_sums).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not read downloaded {CHECKSUMS_FILENAME}: {err}."),
        )
    })?;
    let sig_text = fs::read_to_string(&tmp_sig).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Refusing release {version}: {SIGNATURE_FILENAME} is not readable UTF-8 ({err})."
            ),
        )
        .with_hint(
            "Upload a valid base64 Ed25519 SHA256SUMS.sig from scripts/sign-legion-release.mjs.",
        )
    })?;
    verify_signature_with_key(&sums_bytes, &sig_text, public_key)?;

    let sums_text = String::from_utf8(sums_bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Signed {CHECKSUMS_FILENAME} is not valid UTF-8: {err}."),
        )
    })?;

    let bin_url = artifact_url(&version, target);
    fetcher.download_to_file(&bin_url, &tmp_bin, cancel).await?;

    let bin_verify = tmp_bin.clone();
    let asset_verify = asset.clone();
    let sums_verify = sums_text.clone();
    tokio::task::spawn_blocking(move || {
        verify_file_checksum(&bin_verify, &sums_verify, &asset_verify)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Checksum verification stopped unexpectedly: {err}."),
        )
    })??;

    // Commit manifest, signature, and binary only after both signature and SHA-256 pass so the
    // dist cache never holds an unverified artifact.
    replace_file(&tmp_sums, &cached_sums).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not commit {} into cache: {err}.",
                cached_sums.display()
            ),
        )
    })?;
    sums_guard.disarm();

    replace_file(&tmp_sig, &cached_sig).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not commit {} into cache: {err}.",
                cached_sig.display()
            ),
        )
    })?;
    sig_guard.disarm();

    replace_file(&tmp_bin, &cached_bin).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not commit {} into cache: {err}.",
                cached_bin.display()
            ),
        )
    })?;
    bin_guard.disarm();

    Ok(cached_bin)
}

/// Generates `byte_len * 2` lowercase hex characters from the OS CSPRNG.
pub fn random_hex(byte_len: usize) -> Result<String, ErrorBody> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not read random bytes from the operating system: {err}."),
        )
    })?;
    let mut out = String::with_capacity(byte_len * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

/// Best-effort cleanup of renamed `.old.<hex>` backups left behind after a Windows self-update.
///
/// Called once at process startup so a previous `meta.update` that renamed a running executable
/// aside leaves no stale binaries in the install directory.
pub fn cleanup_old_binary() {
    if let Ok(exe) = legion_core::current_exe() {
        cleanup_old_binaries_next_to(&exe);
    }
}

/// Removes sibling `<exe_name>.old.<suffix>` files in the directory containing `exe`, ignoring
/// any errors (such as another instance still holding a handle on Windows).
pub fn cleanup_old_binaries_next_to(exe: &Path) {
    let Some(parent) = exe.parent() else {
        return;
    };
    let Some(file_name) = exe.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{file_name}{OLD_BINARY_MARKER}");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with(&prefix)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Sibling one-generation backup path (`<exe>.prev`) kept beside the live binary during updates.
pub fn prev_binary_path(target_exe: &Path) -> Option<PathBuf> {
    let parent = target_exe.parent()?;
    let file_name = target_exe.file_name()?.to_str()?;
    Some(parent.join(format!("{file_name}.prev")))
}

/// Parsed version and optional build string from `legion --version` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedVersion {
    /// Bare semantic version string (for example `"0.1.0"`).
    pub version: String,
    /// Optional build revision inside trailing parentheses (for example `"a59ec0d12345"` or
    /// `"a59ec0d12345, modified"`).
    pub build: Option<String>,
}

/// Parses `legion --version` output in either the old (`"legion 0.2.0"`) or new
/// (`"legion 0.2.0 (a59ec0d12345[, modified])"`) form into a [`ParsedVersion`].
pub fn parse_legion_version_line(output: &str) -> Option<ParsedVersion> {
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("legion ") {
            let rest = rest.trim_start();
            let (ver, tail) = match rest.find(char::is_whitespace) {
                Some(idx) => (&rest[..idx], rest[idx..].trim()),
                None => (rest, ""),
            };
            if !ver.is_empty() && normalize_version(ver).is_ok() {
                let build = tail
                    .strip_prefix('(')
                    .and_then(|inner| inner.strip_suffix(')'))
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(str::to_owned);
                return Some(ParsedVersion {
                    version: ver.to_owned(),
                    build,
                });
            }
        }
    }
    None
}

/// Parses `legion --version` output (`"legion 0.2.0"` or `"legion 0.2.0 (<rev>[, modified])"`)
/// into the bare version string.
pub fn parse_legion_version_output(output: &str) -> Option<String> {
    parse_legion_version_line(output).map(|parsed| parsed.version)
}

/// Resolves and validates a local `from` binary path (absolute or starting with `~`), ensuring it
/// points to an existing non-empty regular file.
pub fn resolve_local_binary(paths: &Paths, raw: &str) -> Result<PathBuf, ErrorBody> {
    let path = expand_local_path(paths, raw)?;
    let meta = fs::metadata(&path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("Local binary {} cannot be read: {err}.", path.display()),
        )
        .with_hint("Pass an existing compiled legion binary to `from`.")
    })?;
    if !meta.is_file() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("`from` path {} is not a regular file.", path.display()),
        )
        .with_hint("Pass the path to the legion executable itself, not a directory."));
    }
    if meta.len() == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("`from` file {} is empty.", path.display()),
        )
        .with_hint("Build or download a valid legion binary first."));
    }
    Ok(path)
}

fn expand_local_path(paths: &Paths, raw: &str) -> Result<PathBuf, ErrorBody> {
    if raw.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "`from` path cannot be empty.",
        ));
    }
    let resolved = if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        let home = paths.home().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                "Cannot expand `~` in `from`: home directory is not set.",
            )
            .with_hint("Pass an absolute path to `from`.")
        })?;
        if raw == "~" {
            home.to_path_buf()
        } else {
            home.join(&raw[2..])
        }
    } else {
        PathBuf::from(raw)
    };
    if !resolved.is_absolute() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("`from` path {raw:?} must be absolute (or start with `~`)."),
        )
        .with_hint("Pass an absolute path or a path starting with `~`."));
    }
    Ok(resolved)
}

/// Runs `binary --version` with a bounded timeout and parses the reported version and optional
/// build revision.
pub async fn probe_binary_version(
    binary: &Path,
    cancel: CancellationToken,
) -> Result<ParsedVersion, ErrorBody> {
    let options = RunOptions::new(CommandSpec::argv(binary, ["--version"]))
        .c_locale()
        .timeout(PREFLIGHT_TIMEOUT)
        .cancel(cancel);
    let output = run(options).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Staged binary {} could not be executed for --version preflight: {err}.",
                binary.display()
            ),
        )
        .with_hint("The binary may not match this platform; nothing was replaced.")
    })?;
    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            "Preflight check of the staged legion binary was cancelled.",
        ));
    }
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "Staged binary {} timed out while running --version.",
                binary.display()
            ),
        )
        .with_hint("Nothing was replaced."));
    }
    if !output.success() {
        let detail = output.stderr.text.trim();
        let reason = if detail.is_empty() {
            format!("exited with {:?}", output.exit_code)
        } else {
            detail.to_owned()
        };
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Staged binary {} failed --version preflight ({reason}).",
                binary.display()
            ),
        )
        .with_hint("Nothing was replaced."));
    }
    parse_legion_version_line(&output.stdout.text).ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Staged binary {} did not print a valid `legion <version>` line on --version.",
                binary.display()
            ),
        )
        .with_hint("Nothing was replaced.")
    })
}

/// Runs a staged `legion` binary with `--version` and verifies that it exits 0 and reports
/// `expected_version`.
pub async fn verify_staged_binary_version(
    staged: &Path,
    expected_version: &str,
    cancel: CancellationToken,
) -> Result<(), ErrorBody> {
    let reported = probe_binary_version(staged, cancel).await?;
    let matches = match (
        SemVer::parse(&reported.version),
        SemVer::parse(expected_version),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => reported.version == expected_version,
    };
    if !matches {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Staged binary {} reported version {}, expected {expected_version}.",
                staged.display(),
                reported.version
            ),
        )
        .with_hint("Nothing was replaced."));
    }
    Ok(())
}

/// Copies `new_binary` to a sibling temp file beside `target_exe`, fsyncs it, and marks it
/// executable on Unix. The returned guard unlinks the staged file unless disarmed on commit.
fn stage_executable_beside(target_exe: &Path, new_binary: &Path) -> Result<TempCleanup, ErrorBody> {
    let parent = target_exe.parent().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot determine the directory containing {}.",
                target_exe.display()
            ),
        )
    })?;
    let file_name = target_exe
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Executable path {} has no valid filename.",
                    target_exe.display()
                ),
            )
        })?;

    let token = random_hex(8)?;
    let staged_path = parent.join(format!(".{file_name}.tmp.{token}"));
    let staged_guard = TempCleanup::new(staged_path.clone());

    let mut src = File::open(new_binary).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not open new binary {}: {err}.", new_binary.display()),
        )
    })?;
    let mut dst = File::create(&staged_path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not create staged binary {}: {err}.",
                staged_path.display()
            ),
        )
        .with_hint("Ensure the directory holding the legion binary is writable by this account.")
    })?;
    io::copy(&mut src, &mut dst).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not copy new binary into {}: {err}.",
                staged_path.display()
            ),
        )
    })?;
    dst.flush().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not flush staged binary {}: {err}.",
                staged_path.display()
            ),
        )
    })?;
    dst.sync_all().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not sync staged binary {}: {err}.",
                staged_path.display()
            ),
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        dst.set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Could not mark staged binary {} executable: {err}.",
                        staged_path.display()
                    ),
                )
            })?;
    }
    drop(dst);
    Ok(staged_guard)
}

/// Saves a one-generation copy of `target_exe` as `<exe>.prev` beside it atomically.
fn save_prev_backup(target_exe: &Path) -> Result<(), ErrorBody> {
    if !target_exe.exists() {
        return Ok(());
    }
    let Some(prev_path) = prev_binary_path(target_exe) else {
        return Ok(());
    };
    let mut backup_guard = stage_executable_beside(target_exe, target_exe)?;
    replace_file(&backup_guard.path, &prev_path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not save previous binary backup {}: {err}.",
                prev_path.display()
            ),
        )
        .with_hint("Ensure the directory holding legion is writable.")
    })?;
    backup_guard.disarm();
    Ok(())
}

/// Commits an already-staged sibling binary over `target_exe`, keeping a one-generation backup
/// (`<exe>.prev`) beside it.
fn commit_staged_executable(
    mut staged_guard: TempCleanup,
    target_exe: &Path,
    rename_old_first: bool,
) -> Result<(), ErrorBody> {
    let parent = target_exe.parent().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot determine the directory containing {}.",
                target_exe.display()
            ),
        )
    })?;
    let file_name = target_exe
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Executable path {} has no valid filename.",
                    target_exe.display()
                ),
            )
        })?;

    save_prev_backup(target_exe)?;

    if rename_old_first && target_exe.exists() {
        let token = random_hex(8)?;
        let old_path = parent.join(format!("{file_name}{OLD_BINARY_MARKER}{token}"));
        replace_file(target_exe, &old_path).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not move running binary {} aside before updating: {err}.",
                    target_exe.display()
                ),
            )
            .with_hint("Ensure this account has write permission in the legion install directory.")
        })?;
        if let Err(err) = replace_file(&staged_guard.path, target_exe) {
            // Roll back so the machine is never left without its legion executable.
            let _ = replace_file(&old_path, target_exe);
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not move updated binary into {}: {err}. The previous binary was restored.",
                    target_exe.display()
                ),
            ));
        }
        staged_guard.disarm();
        // Best-effort removal: on Windows the running executable stays locked until exit and is
        // removed on the next start by `cleanup_old_binary`.
        let _ = fs::remove_file(&old_path);
    } else {
        let _ = WriteOptions::with_mode(0o755);
        replace_file(&staged_guard.path, target_exe).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not replace {} with updated binary: {err}.",
                    target_exe.display()
                ),
            )
            .with_hint("Ensure this account has write permission in the legion install directory.")
        })?;
        staged_guard.disarm();
    }

    Ok(())
}

/// Restores `<exe>.prev` over `target_exe` after a failed post-replacement verification.
fn restore_prev_backup(target_exe: &Path, rename_old_first: bool) -> Result<(), ErrorBody> {
    let prev_path = prev_binary_path(target_exe).ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot determine `.prev` backup path for {}.",
                target_exe.display()
            ),
        )
    })?;
    if !prev_path.is_file() {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "Previous binary backup {} does not exist.",
                prev_path.display()
            ),
        ));
    }
    let parent = target_exe.parent().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot determine the directory containing {}.",
                target_exe.display()
            ),
        )
    })?;
    let file_name = target_exe
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Executable path {} has no valid filename.",
                    target_exe.display()
                ),
            )
        })?;

    if rename_old_first && target_exe.exists() {
        let token = random_hex(8)?;
        let old_path = parent.join(format!("{file_name}{OLD_BINARY_MARKER}{token}"));
        replace_file(target_exe, &old_path).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not move broken binary {} aside during rollback: {err}.",
                    target_exe.display()
                ),
            )
        })?;
        if let Err(err) = replace_file(&prev_path, target_exe) {
            let _ = replace_file(&old_path, target_exe);
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not restore {} from {}: {err}.",
                    target_exe.display(),
                    prev_path.display()
                ),
            ));
        }
        let _ = fs::remove_file(&old_path);
    } else {
        replace_file(&prev_path, target_exe).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not restore {} from {}: {err}.",
                    target_exe.display(),
                    prev_path.display()
                ),
            )
        })?;
    }
    Ok(())
}

/// Replaces `target_exe` with the contents of `new_binary` atomically, keeping one previous
/// generation beside it as `<exe>.prev`.
pub fn replace_executable(
    target_exe: &Path,
    new_binary: &Path,
    rename_old_first: bool,
) -> Result<(), ErrorBody> {
    let staged_guard = stage_executable_beside(target_exe, new_binary)?;
    commit_staged_executable(staged_guard, target_exe, rename_old_first)
}

trait BinaryProber: Send + Sync {
    fn probe<'a>(
        &'a self,
        binary: &'a Path,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ParsedVersion, ErrorBody>>;
}

struct SystemProber;

impl BinaryProber for SystemProber {
    fn probe<'a>(
        &'a self,
        binary: &'a Path,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ParsedVersion, ErrorBody>> {
        Box::pin(async move { probe_binary_version(binary, cancel).await })
    }
}

fn versions_match(actual: &str, expected: &str) -> bool {
    match (SemVer::parse(actual), SemVer::parse(expected)) {
        (Ok(a), Ok(b)) => a == b,
        _ => actual.trim() == expected.trim(),
    }
}

async fn meta_update(ctx: &Ctx, args: UpdateArgs) -> Result<UpdateResult, ToolError> {
    let current_exe = legion_core::current_exe()?;
    let fetcher = SystemFetcher;
    run_meta_update(
        ctx.paths(),
        &fetcher,
        ctx.os(),
        ctx.arch(),
        VERSION,
        &current_exe,
        cfg!(windows),
        args,
        ctx.cancel_token().clone(),
    )
    .await
}

/// Core implementation of `meta.update`, parameterized over fetcher, platform, executable path, and
/// replacement mode so tests exercise self-replacement on a temp copy without touching the network
/// or the test runner binary. Always verifies against the compiled-in [`RELEASE_PUBLIC_KEY`].
#[allow(clippy::too_many_arguments)]
pub async fn run_meta_update(
    paths: &Paths,
    fetcher: &dyn ReleaseFetcher,
    os: &str,
    arch: &str,
    current_version: &str,
    current_exe: &Path,
    rename_old_first: bool,
    args: UpdateArgs,
    cancel: CancellationToken,
) -> Result<UpdateResult, ErrorBody> {
    let prober = SystemProber;
    run_meta_update_inner(
        paths,
        fetcher,
        os,
        arch,
        current_version,
        current_exe,
        rename_old_first,
        &RELEASE_PUBLIC_KEY,
        Some(&prober),
        args,
        cancel,
    )
    .await
}

async fn detect_macos_rosetta(os: &str, arch: &str, cancel: CancellationToken) -> bool {
    if !cfg!(target_os = "macos") || os != "macos" || arch != "x86_64" {
        return false;
    }
    let opts = RunOptions::new(CommandSpec::argv(
        "sysctl",
        ["-in", "sysctl.proc_translated"],
    ))
    .c_locale()
    .timeout(Duration::from_secs(5))
    .cancel(cancel);
    match run(opts).await {
        Ok(out) => out.success() && out.stdout.text.trim() == "1",
        Err(_) => false,
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn run_meta_update_with_key(
    paths: &Paths,
    fetcher: &dyn ReleaseFetcher,
    os: &str,
    arch: &str,
    current_version: &str,
    current_exe: &Path,
    rename_old_first: bool,
    public_key: &[u8; 32],
    run_staged_preflight: bool,
    args: UpdateArgs,
    cancel: CancellationToken,
) -> Result<UpdateResult, ErrorBody> {
    let prober = SystemProber;
    let prober_ref: Option<&dyn BinaryProber> = if run_staged_preflight {
        Some(&prober)
    } else {
        None
    };
    run_meta_update_inner(
        paths,
        fetcher,
        os,
        arch,
        current_version,
        current_exe,
        rename_old_first,
        public_key,
        prober_ref,
        args,
        cancel,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_meta_update_inner(
    paths: &Paths,
    fetcher: &dyn ReleaseFetcher,
    os: &str,
    arch: &str,
    current_version: &str,
    current_exe: &Path,
    rename_old_first: bool,
    public_key: &[u8; 32],
    prober: Option<&dyn BinaryProber>,
    args: UpdateArgs,
    cancel: CancellationToken,
) -> Result<UpdateResult, ErrorBody> {
    if args.from.is_some() && args.version.is_some() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "meta.update accepts either `from` or `version`, not both.",
        )
        .with_hint("Pass `from` for a local binary or `version` for a GitHub release."));
    }

    let current_exe = legion_core::validate_recorded_exe(current_exe)?;
    if let Some(manager) = crate::package::detect_package_manager(&current_exe) {
        return Err(crate::package::managed_update_error(&manager, &current_exe));
    }
    let macos_translated = detect_macos_rosetta(os, arch, cancel.clone()).await;
    let resolved = resolve_target_for(os, arch, macos_translated)?;
    let target = resolved.target;
    let note = resolved.note.map(str::to_owned);
    let current_semver = SemVer::parse(current_version).ok();
    let floor_path = node_floor_path(paths);

    if let Some(from_raw) = args.from {
        let local_bin = resolve_local_binary(paths, &from_raw)?;
        verify_binary_matches_target(&local_bin, target)?;

        if args.dry_run {
            let (planned_version, planned_build) = if let Some(prober) = prober {
                match prober.probe(&local_bin, cancel).await {
                    Ok(parsed) => {
                        let semver = SemVer::parse(&parsed.version)?;
                        check_version_transition(
                            &semver,
                            current_semver.as_ref(),
                            &floor_path,
                            args.allow_downgrade,
                        )?;
                        (parsed.version, parsed.build)
                    }
                    Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
                    Err(_) => ("local".to_owned(), None),
                }
            } else {
                ("local".to_owned(), None)
            };
            return Ok(UpdateResult {
                previous_version: current_version.to_owned(),
                installed_version: planned_version,
                build: planned_build,
                target: target.to_owned(),
                path: current_exe.display().to_string(),
                dry_run: true,
                updated: false,
                note,
            });
        }

        let target_exe_stage = current_exe.clone();
        let staged_guard = tokio::task::spawn_blocking(move || {
            stage_executable_beside(&target_exe_stage, &local_bin)
        })
        .await
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Staging the legion binary stopped unexpectedly: {err}."),
            )
        })??;

        let reported = match prober {
            Some(prober) => prober.probe(&staged_guard.path, cancel.clone()).await?,
            None => ParsedVersion {
                version: "local".to_owned(),
                build: None,
            },
        };
        let desired_semver = if reported.version == "local" && prober.is_none() {
            None
        } else {
            let semver = SemVer::parse(&reported.version)?;
            check_version_transition(
                &semver,
                current_semver.as_ref(),
                &floor_path,
                args.allow_downgrade,
            )?;
            Some(semver)
        };

        let target_exe_commit = current_exe.clone();
        tokio::task::spawn_blocking(move || {
            commit_staged_executable(staged_guard, &target_exe_commit, rename_old_first)
        })
        .await
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Replacing the legion binary stopped unexpectedly: {err}."),
            )
        })??;

        let installed = if let Some(prober) = prober {
            match prober.probe(&current_exe, cancel).await {
                Ok(parsed) if versions_match(&parsed.version, &reported.version) => ParsedVersion {
                    version: parsed.version,
                    build: parsed.build.or(reported.build),
                },
                Ok(parsed) => {
                    let rollback_note =
                        attempt_local_rollback(&current_exe, rename_old_first).await;
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Replaced {} with legion {}, but running it reported version {}.{rollback_note}",
                            current_exe.display(),
                            reported.version,
                            parsed.version
                        ),
                    ));
                }
                Err(err) => {
                    let rollback_note =
                        attempt_local_rollback(&current_exe, rename_old_first).await;
                    return Err(ErrorBody::new(
                        err.code,
                        format!(
                            "Replaced {}, but the new binary did not run: {}{rollback_note}",
                            current_exe.display(),
                            err.message
                        ),
                    ));
                }
            }
        } else {
            reported
        };

        if let Some(semver) = desired_semver {
            commit_floor(&floor_path, &semver, args.allow_downgrade)?;
        }

        return Ok(UpdateResult {
            previous_version: current_version.to_owned(),
            installed_version: installed.version,
            build: installed.build,
            target: target.to_owned(),
            path: current_exe.display().to_string(),
            dry_run: false,
            updated: true,
            note,
        });
    }

    let explicit_version = args.version.is_some();
    let desired_version = match args.version {
        Some(raw) => normalize_version(&raw)?,
        None => fetcher.latest_version(cancel.clone()).await?,
    };
    let desired_semver = SemVer::parse(&desired_version)?;

    // When asking for "latest" and already at or above it, check only if a downgrade would happen:
    // if the node is newer than the highest published release, refuse a silent downgrade unless
    // allow_downgrade is explicitly set.
    if !explicit_version
        && current_semver
            .as_ref()
            .is_some_and(|cur| *cur == desired_semver)
    {
        return Ok(UpdateResult {
            previous_version: current_version.to_owned(),
            installed_version: desired_version,
            build: None,
            target: target.to_owned(),
            path: current_exe.display().to_string(),
            dry_run: args.dry_run,
            updated: false,
            note,
        });
    }

    check_version_transition(
        &desired_semver,
        current_semver.as_ref(),
        &floor_path,
        args.allow_downgrade,
    )?;

    if args.dry_run {
        return Ok(UpdateResult {
            previous_version: current_version.to_owned(),
            installed_version: desired_version,
            build: None,
            target: target.to_owned(),
            path: current_exe.display().to_string(),
            dry_run: true,
            updated: false,
            note,
        });
    }

    let cached_bin = resolve_release_binary_with_key(
        paths,
        fetcher,
        &desired_version,
        target,
        public_key,
        cancel.clone(),
    )
    .await?;

    // Check executable header (ELF/Mach-O/PE + arch) before staging over the live binary.
    verify_binary_matches_target(&cached_bin, target)?;

    let target_exe_stage = current_exe.clone();
    let staged_guard = tokio::task::spawn_blocking(move || {
        stage_executable_beside(&target_exe_stage, &cached_bin)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Staging the legion binary stopped unexpectedly: {err}."),
        )
    })??;

    let mut installed_build = None;
    if let Some(prober) = prober {
        let reported = prober.probe(&staged_guard.path, cancel.clone()).await?;
        if !versions_match(&reported.version, &desired_version) {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Staged binary {} reported version {}, expected {desired_version}.",
                    staged_guard.path.display(),
                    reported.version
                ),
            )
            .with_hint("Nothing was replaced."));
        }
        installed_build = reported.build;
    }

    let target_exe_commit = current_exe.clone();
    tokio::task::spawn_blocking(move || {
        commit_staged_executable(staged_guard, &target_exe_commit, rename_old_first)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Replacing the legion binary stopped unexpectedly: {err}."),
        )
    })??;

    if let Some(prober) = prober {
        match prober.probe(&current_exe, cancel).await {
            Ok(parsed) if versions_match(&parsed.version, &desired_version) => {
                if parsed.build.is_some() {
                    installed_build = parsed.build;
                }
            }
            Ok(parsed) => {
                let rollback_note = attempt_local_rollback(&current_exe, rename_old_first).await;
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Replaced {} with legion {desired_version}, but running it reported version {}.{rollback_note}",
                        current_exe.display(),
                        parsed.version
                    ),
                ));
            }
            Err(err) => {
                let rollback_note = attempt_local_rollback(&current_exe, rename_old_first).await;
                return Err(ErrorBody::new(
                    err.code,
                    format!(
                        "Replaced {}, but the new binary did not run: {}{rollback_note}",
                        current_exe.display(),
                        err.message
                    ),
                ));
            }
        }
    }

    commit_floor(&floor_path, &desired_semver, args.allow_downgrade)?;

    Ok(UpdateResult {
        previous_version: current_version.to_owned(),
        installed_version: desired_version,
        build: installed_build,
        target: target.to_owned(),
        path: current_exe.display().to_string(),
        dry_run: false,
        updated: true,
        note,
    })
}

async fn attempt_local_rollback(target_exe: &Path, rename_old_first: bool) -> String {
    let exe = target_exe.to_path_buf();
    let prev_display = prev_binary_path(target_exe)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| format!("{}.prev", target_exe.display()));
    match tokio::task::spawn_blocking(move || restore_prev_backup(&exe, rename_old_first)).await {
        Ok(Ok(())) => {
            format!(" The previous binary was automatically restored from {prev_display}.")
        }
        Ok(Err(err)) => format!(
            " Automatic rollback from {prev_display} failed: {}",
            err.message
        ),
        Err(err) => format!(" Automatic rollback from {prev_display} failed: {err}."),
    }
}

fn curl_api_args(url: &str) -> [&str; 20] {
    [
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--retry",
        "2",
        "--connect-timeout",
        "15",
        "--max-time",
        "30",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: legion",
        url,
    ]
}

fn curl_download_args<'a>(url: &'a str, dest_str: &'a str) -> [&'a str; 20] {
    [
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--retry",
        "2",
        "--connect-timeout",
        "15",
        "--max-time",
        "120",
        "-H",
        "User-Agent: legion",
        "-o",
        dest_str,
        url,
    ]
}

async fn fetch_api_text(url: &str, cancel: CancellationToken) -> Result<String, ErrorBody> {
    let curl_opts = RunOptions::new(CommandSpec::argv("curl", curl_api_args(url)))
        .c_locale()
        .timeout(API_TIMEOUT)
        .cancel(cancel.clone());

    match run(curl_opts).await {
        Ok(output) => check_download_output("curl", url, output).map(|out| out.stdout.text),
        Err(OsError::ProgramNotFound { .. }) if cfg!(windows) => {
            let ps_opts = RunOptions::new(CommandSpec::argv(
                "powershell",
                [
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    PS_FETCH_API_SCRIPT,
                ],
            ))
            .env("LEGION_DOWNLOAD_URL", url)
            .env_remove("PSModulePath")
            .timeout(API_TIMEOUT)
            .cancel(cancel);
            let output = run(ps_opts).await.map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not start PowerShell to query {url}: {err}."),
                )
                .with_hint("Install `curl` or ensure Windows PowerShell is on PATH.")
            })?;
            check_download_output("powershell", url, output).map(|out| out.stdout.text)
        }
        Err(OsError::ProgramNotFound { .. }) => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "Cannot query releases: `curl` is not installed or not on PATH.",
        )
        .with_hint("Install `curl`, or provide a local binary with `from`.")),
        Err(err) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not start `curl` for {url}: {err}."),
        )),
    }
}

async fn download_url_to_file(
    url: &str,
    dest: &Path,
    cancel: CancellationToken,
) -> Result<(), ErrorBody> {
    let dest_str = dest.to_str().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Download path {} is not valid UTF-8.", dest.display()),
        )
    })?;
    let curl_opts = RunOptions::new(CommandSpec::argv("curl", curl_download_args(url, dest_str)))
        .c_locale()
        .timeout(DOWNLOAD_TIMEOUT)
        .cancel(cancel.clone());

    match run(curl_opts).await {
        Ok(output) => {
            check_download_output("curl", url, output)?;
            Ok(())
        }
        Err(OsError::ProgramNotFound { .. }) if cfg!(windows) => {
            let ps_opts = RunOptions::new(CommandSpec::argv(
                "powershell",
                [
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    PS_DOWNLOAD_FILE_SCRIPT,
                ],
            ))
            .env("LEGION_DOWNLOAD_URL", url)
            .env("LEGION_DOWNLOAD_DEST", dest.as_os_str())
            .env_remove("PSModulePath")
            .timeout(DOWNLOAD_TIMEOUT)
            .cancel(cancel);
            let output = run(ps_opts).await.map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not start PowerShell to download {url}: {err}."),
                )
                .with_hint("Install `curl` or ensure Windows PowerShell is on PATH.")
            })?;
            check_download_output("powershell", url, output)?;
            Ok(())
        }
        Err(OsError::ProgramNotFound { .. }) => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "Cannot download release artifacts: `curl` is not installed or not on PATH.",
        )
        .with_hint("Install `curl`, or provide a local binary with `from`.")),
        Err(err) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not start `curl` to download {url}: {err}."),
        )),
    }
}

fn check_download_output(
    program: &str,
    url: &str,
    output: legion_os::run::RunOutput,
) -> Result<legion_os::run::RunOutput, ErrorBody> {
    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!("Downloading {url} was cancelled."),
        ));
    }
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!("Downloading {url} with `{program}` timed out."),
        )
        .with_hint("Check network connectivity to GitHub or try again."));
    }
    if !output.success() {
        let detail = output.stderr.text.trim();
        let message = if detail.is_empty() {
            format!(
                "Downloading {url} failed ({program} exited with {:?}).",
                output.exit_code
            )
        } else {
            format!("Downloading {url} failed: {detail}")
        };
        return Err(ErrorBody::new(ErrorCode::Failed, message)
            .with_hint("Check that the release tag and asset exist on GitHub and that this machine can reach github.com."));
    }
    Ok(output)
}

struct TempCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::tempdir;

    use super::*;

    struct FakeFetcher {
        latest: String,
        files: HashMap<String, Vec<u8>>,
        downloads: AtomicUsize,
    }

    impl FakeFetcher {
        fn new(latest: &str, files: HashMap<String, Vec<u8>>) -> Self {
            Self {
                latest: latest.to_owned(),
                files,
                downloads: AtomicUsize::new(0),
            }
        }

        fn download_count(&self) -> usize {
            self.downloads.load(Ordering::SeqCst)
        }
    }

    impl ReleaseFetcher for FakeFetcher {
        fn latest_version<'a>(
            &'a self,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<String, ErrorBody>> {
            Box::pin(async move { Ok(self.latest.clone()) })
        }

        fn download_to_file<'a>(
            &'a self,
            url: &'a str,
            dest: &'a Path,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<(), ErrorBody>> {
            Box::pin(async move {
                self.downloads.fetch_add(1, Ordering::SeqCst);
                let Some(bytes) = self.files.get(url) else {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("Fake release server has no file at {url}."),
                    ));
                };
                fs::write(dest, bytes).map_err(|err| {
                    ErrorBody::new(ErrorCode::Failed, format!("write failed: {err}"))
                })?;
                Ok(())
            })
        }
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut hex = String::with_capacity(64);
        for byte in digest {
            let _ = write!(&mut hex, "{byte:02x}");
        }
        hex
    }

    #[test]
    fn checksum_verification_accepts_good_and_rejects_bad_or_missing() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("legion-0.2.0-x86_64-unknown-linux-musl");
        let payload = b"legion binary payload";
        fs::write(&file, payload).unwrap();
        let good_hash = sha256_bytes(payload);
        let asset = "legion-0.2.0-x86_64-unknown-linux-musl";

        // Standard two-space format plus Windows CRLF and binary `*` marker.
        let good_sums = format!(
            "# comment\r\n{}  other-asset\r\n{} *{asset}\r\n",
            "0".repeat(64),
            good_hash.to_uppercase()
        );
        assert_eq!(
            verify_file_checksum(&file, &good_sums, asset).unwrap(),
            good_hash
        );

        let bad_sums = format!("{}  {asset}\n", "f".repeat(64));
        let bad_err = verify_file_checksum(&file, &bad_sums, asset).unwrap_err();
        assert_eq!(bad_err.code, ErrorCode::Failed);
        assert!(bad_err.message.contains("mismatch"), "{}", bad_err.message);

        let missing_sums = format!("{good_hash}  legion-0.2.0-aarch64-unknown-linux-musl\n");
        let missing_err = verify_file_checksum(&file, &missing_sums, asset).unwrap_err();
        assert_eq!(missing_err.code, ErrorCode::Failed);
        assert!(
            missing_err.message.contains("does not contain"),
            "{}",
            missing_err.message
        );
    }

    #[test]
    fn latest_release_parser_picks_highest_stable_semver_not_first_created() {
        // Simulates a maintenance patch `0.1.9` created AFTER `0.10.0` (so GitHub lists `0.1.9`
        // first), plus an unflagged SemVer prerelease `0.11.0-rc.1`: highest stable must be 0.10.0.
        let json = r#"[
            {"tag_name": "v2.0.0", "draft": false, "prerelease": false},
            {"tag_name": "legion-v0.1.9", "draft": false, "prerelease": false},
            {"tag_name": "legion-v0.11.0-rc.1", "draft": false, "prerelease": false},
            {"tag_name": "legion-v0.12.0", "draft": true, "prerelease": false},
            {"tag_name": "legion-v0.10.0", "draft": false, "prerelease": false},
            {"tag_name": "legion-v0.9.5", "draft": false, "prerelease": false}
        ]"#;
        assert_eq!(parse_latest_release_version(json).unwrap(), "0.10.0");

        let empty = r#"[{"tag_name": "v1.0.0", "draft": false, "prerelease": false}]"#;
        assert_eq!(
            parse_latest_release_version(empty).unwrap_err().code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn target_triple_refuses_32bit_arm_with_clear_message() {
        assert_eq!(
            target_triple_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            target_triple_for("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(
            target_triple_for("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            target_triple_for("windows", "x86_64").unwrap(),
            "x86_64-pc-windows-msvc"
        );

        let err = target_triple_for("linux", "armv7l").unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("32-bit ARM"), "{}", err.message);
    }

    #[tokio::test]
    async fn meta_update_replaces_temp_binary_copy_and_uses_cache() {
        use ed25519_dalek::{Signer, SigningKey};

        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let install_dir = dir.path().join("install");
        fs::create_dir_all(&install_dir).unwrap();

        // Copy the running test executable so we replace a real binary in a temp directory.
        let current_copy = install_dir.join(if cfg!(windows) {
            "legion.exe"
        } else {
            "legion"
        });
        let test_bin = std::env::current_exe().unwrap();
        fs::copy(&test_bin, &current_copy).unwrap();

        let signing = SigningKey::from_bytes(&[0x42; 32]);
        let pubkey = signing.verifying_key().to_bytes();

        let mut new_bytes = crate::binfmt::synthetic_header_for_target("x86_64-unknown-linux-musl");
        new_bytes.extend_from_slice(b"new-legion-0.2.0-binary-content");
        let target = "x86_64-unknown-linux-musl";
        let asset = artifact_filename("0.2.0", target);
        let sums = format!("{}  {asset}\n", sha256_bytes(&new_bytes));
        let sig = crate::trust::encode_signature_bytes(&signing.sign(sums.as_bytes()).to_bytes());
        let mut files = HashMap::new();
        files.insert(checksums_url("0.2.0"), sums.into_bytes());
        files.insert(checksums_sig_url("0.2.0"), sig.into_bytes());
        files.insert(artifact_url("0.2.0", target), new_bytes.clone());
        let fetcher = FakeFetcher::new("0.2.0", files);

        // Dry run touches neither the network downloads nor the binary on disk.
        let dry = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &current_copy,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: None,
                from: None,
                dry_run: true,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(dry.dry_run);
        assert!(!dry.updated);
        assert_eq!(dry.previous_version, "0.1.0");
        assert_eq!(dry.installed_version, "0.2.0");
        assert_eq!(fetcher.download_count(), 0);
        assert_ne!(fs::read(&current_copy).unwrap(), new_bytes);

        let original_bytes = fs::read(&current_copy).unwrap();

        // Real update downloads SHA256SUMS + SHA256SUMS.sig + binary, verifies, caches, saves
        // `.prev`, and replaces atomically.
        let updated = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &current_copy,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: None,
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!updated.dry_run);
        assert!(updated.updated);
        assert_eq!(updated.previous_version, "0.1.0");
        assert_eq!(updated.installed_version, "0.2.0");
        assert_eq!(fetcher.download_count(), 3);
        assert_eq!(fs::read(&current_copy).unwrap(), new_bytes);
        assert_eq!(
            fs::read(prev_binary_path(&current_copy).unwrap()).unwrap(),
            original_bytes
        );

        // A second explicit call reuses the verified signed dist cache without re-downloading.
        let cached = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &current_copy,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(cached.updated);
        assert_eq!(fetcher.download_count(), 3);
    }

    #[test]
    fn windows_rename_aside_replacement_and_startup_cleanup() {
        let dir = tempdir().unwrap();
        let exe = dir.path().join("legion.exe");
        let incoming = dir.path().join("downloaded.exe");
        fs::write(&exe, b"old-version-binary").unwrap();
        fs::write(&incoming, b"new-version-binary").unwrap();

        replace_executable(&exe, &incoming, true).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new-version-binary");

        // Simulate a stale `.old.<token>` file left when Windows held the old running exe locked,
        // and verify startup cleanup removes only our backup files.
        let stale = dir.path().join("legion.exe.old.deadbeef");
        let unrelated = dir.path().join("legion.toml");
        fs::write(&stale, b"locked-at-update-time").unwrap();
        fs::write(&unrelated, b"keep me").unwrap();

        cleanup_old_binaries_next_to(&exe);
        assert!(!stale.exists());
        assert!(unrelated.exists());
        assert!(exe.exists());
    }

    #[tokio::test]
    async fn bad_checksum_leaves_existing_binary_untouched() {
        use ed25519_dalek::{Signer, SigningKey};

        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let exe = dir.path().join("legion");
        fs::write(&exe, b"original-good-binary").unwrap();

        let signing = SigningKey::from_bytes(&[0x55; 32]);
        let pubkey = signing.verifying_key().to_bytes();

        let target = "x86_64-unknown-linux-musl";
        let asset = artifact_filename("0.2.0", target);
        let sums = format!("{}  {asset}\n", "0".repeat(64));
        let sig = crate::trust::encode_signature_bytes(&signing.sign(sums.as_bytes()).to_bytes());
        let mut files = HashMap::new();
        files.insert(checksums_url("0.2.0"), sums.into_bytes());
        files.insert(checksums_sig_url("0.2.0"), sig.into_bytes());
        files.insert(artifact_url("0.2.0", target), b"corrupted".to_vec());
        let fetcher = FakeFetcher::new("0.2.0", files);

        let err = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert_eq!(fs::read(&exe).unwrap(), b"original-good-binary");
        assert!(!dist_cache_binary(&paths, "0.2.0", target).exists());
    }

    #[tokio::test]
    async fn missing_or_invalid_signature_and_unsigned_cache_are_denied() {
        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let exe = dir.path().join("legion");
        fs::write(&exe, b"original-good-binary").unwrap();

        let target = "x86_64-unknown-linux-musl";
        let asset = artifact_filename("0.2.0", target);
        let payload = b"unsigned-release-binary";
        let sums = format!("{}  {asset}\n", sha256_bytes(payload));

        // Pre-populate an unsigned binary in the dist cache (no SHA256SUMS.sig).
        let cache_dir = dist_cache_dir(&paths, "0.2.0", target);
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join(&asset), payload).unwrap();
        fs::write(cache_dir.join(CHECKSUMS_FILENAME), &sums).unwrap();

        // Missing SHA256SUMS.sig on the server must also be denied (never trusts unsigned cache).
        let mut files = HashMap::new();
        files.insert(checksums_url("0.2.0"), sums.as_bytes().to_vec());
        files.insert(artifact_url("0.2.0", target), payload.to_vec());
        let fetcher = FakeFetcher::new("0.2.0", files.clone());

        let err = run_meta_update(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &exe,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(err.message.contains(SIGNATURE_FILENAME), "{}", err.message);
        assert_eq!(fs::read(&exe).unwrap(), b"original-good-binary");

        // Wrong-key signature is also denied before downloading the binary.
        use ed25519_dalek::{Signer, SigningKey};
        let wrong_signing = SigningKey::from_bytes(&[0x99; 32]);
        let bad_sig =
            crate::trust::encode_signature_bytes(&wrong_signing.sign(sums.as_bytes()).to_bytes());
        files.insert(checksums_sig_url("0.2.0"), bad_sig.into_bytes());
        let bad_fetcher = FakeFetcher::new("0.2.0", files);
        let err = run_meta_update(
            &paths,
            &bad_fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &exe,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert_eq!(bad_fetcher.download_count(), 2);
        assert_eq!(fs::read(&exe).unwrap(), b"original-good-binary");
    }

    #[tokio::test]
    async fn meta_update_refuses_downgrade_unless_allow_downgrade_is_set() {
        use ed25519_dalek::{Signer, SigningKey};

        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let exe = dir.path().join("legion");
        fs::write(&exe, b"legion-0.3.0-binary").unwrap();

        let signing = SigningKey::from_bytes(&[0x77; 32]);
        let pubkey = signing.verifying_key().to_bytes();
        let target = "x86_64-unknown-linux-musl";
        let mut old_bytes = crate::binfmt::synthetic_header_for_target(target);
        old_bytes.extend_from_slice(b"legion-0.2.0-binary");
        let asset = artifact_filename("0.2.0", target);
        let sums = format!("{}  {asset}\n", sha256_bytes(&old_bytes));
        let sig = crate::trust::encode_signature_bytes(&signing.sign(sums.as_bytes()).to_bytes());
        let mut files = HashMap::new();
        files.insert(checksums_url("0.2.0"), sums.into_bytes());
        files.insert(checksums_sig_url("0.2.0"), sig.into_bytes());
        files.insert(artifact_url("0.2.0", target), old_bytes.clone());
        let fetcher = FakeFetcher::new("0.2.0", files);

        // Node running 0.3.0 refuses silent downgrade to 0.2.0 (whether explicit or via "latest").
        let err = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.3.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: None,
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert_eq!(fetcher.download_count(), 0);
        assert_eq!(fs::read(&exe).unwrap(), b"legion-0.3.0-binary");

        // Explicit allow_downgrade permits downgrading and records the new floor.
        let ok = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.3.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(ok.updated);
        assert_eq!(fs::read(&exe).unwrap(), old_bytes);
        assert_eq!(
            crate::floor::read_floor(&node_floor_path(&paths)),
            crate::floor::FloorState::Recorded(SemVer::parse("0.2.0").unwrap())
        );
    }

    #[tokio::test]
    async fn consecutive_meta_updates_replace_recorded_path_and_refuse_missing_file() {
        use ed25519_dalek::{Signer, SigningKey};

        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let exe = dir.path().join("legion");
        fs::write(&exe, b"legion-0.1.0").unwrap();

        let signing = SigningKey::from_bytes(&[0x66; 32]);
        let pubkey = signing.verifying_key().to_bytes();
        let target = "x86_64-unknown-linux-musl";

        let mut bytes_020 = crate::binfmt::synthetic_header_for_target(target);
        bytes_020.extend_from_slice(b"legion-0.2.0-bytes");
        let mut bytes_030 = crate::binfmt::synthetic_header_for_target(target);
        bytes_030.extend_from_slice(b"legion-0.3.0-bytes");

        let mut files = HashMap::new();
        for (ver, bytes) in [
            ("0.2.0", bytes_020.as_slice()),
            ("0.3.0", bytes_030.as_slice()),
        ] {
            let asset = artifact_filename(ver, target);
            let sums = format!("{}  {asset}\n", sha256_bytes(bytes));
            let sig =
                crate::trust::encode_signature_bytes(&signing.sign(sums.as_bytes()).to_bytes());
            files.insert(checksums_url(ver), sums.into_bytes());
            files.insert(checksums_sig_url(ver), sig.into_bytes());
            files.insert(artifact_url(ver, target), bytes.to_vec());
        }
        let fetcher = FakeFetcher::new("0.3.0", files);

        // Two updates in a row against the same recorded path keep updating `legion` itself.
        run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&exe).unwrap(), bytes_020);
        assert_eq!(
            fs::read(prev_binary_path(&exe).unwrap()).unwrap(),
            b"legion-0.1.0"
        );

        run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.3.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&exe).unwrap(), bytes_030);
        assert_eq!(
            fs::read(prev_binary_path(&exe).unwrap()).unwrap(),
            bytes_020
        );
        assert!(!dir.path().join("legion (deleted)").exists());

        // Removing the recorded file causes a subsequent update to fail cleanly rather than
        // creating an unrelated file.
        fs::remove_file(&exe).unwrap();
        let err = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.3.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.3.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(!exe.exists());
    }

    #[tokio::test]
    async fn meta_update_refuses_wrong_arch_binary_and_bad_staged_version() {
        use ed25519_dalek::{Signer, SigningKey};

        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let exe = dir.path().join("legion");
        fs::write(&exe, b"original-good-binary").unwrap();

        let signing = SigningKey::from_bytes(&[0x55; 32]);
        let pubkey = signing.verifying_key().to_bytes();
        let target = "x86_64-unknown-linux-musl";
        // Ship an aarch64 ELF under the x86_64 asset name: header verification must reject it.
        let wrong_arch = crate::binfmt::synthetic_header_for_target("aarch64-unknown-linux-musl");
        let asset = artifact_filename("0.2.0", target);
        let sums = format!("{}  {asset}\n", sha256_bytes(&wrong_arch));
        let sig = crate::trust::encode_signature_bytes(&signing.sign(sums.as_bytes()).to_bytes());
        let mut files = HashMap::new();
        files.insert(checksums_url("0.2.0"), sums.into_bytes());
        files.insert(checksums_sig_url("0.2.0"), sig.into_bytes());
        files.insert(artifact_url("0.2.0", target), wrong_arch);
        let fetcher = FakeFetcher::new("0.2.0", files);

        let err = run_meta_update_with_key(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &exe,
            false,
            &pubkey,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("aarch64"), "{}", err.message);
        assert_eq!(fs::read(&exe).unwrap(), b"original-good-binary");

        // Staged `--version` preflight rejects a binary that exits non-zero or prints the wrong version.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let bad_script = dir.path().join("staged-bad");
            fs::write(&bad_script, "#!/bin/sh\necho 'legion 0.9.9'\n").unwrap();
            fs::set_permissions(&bad_script, fs::Permissions::from_mode(0o755)).unwrap();
            let mismatch =
                verify_staged_binary_version(&bad_script, "0.2.0", CancellationToken::new())
                    .await
                    .unwrap_err();
            assert_eq!(mismatch.code, ErrorCode::Failed);
            assert!(mismatch.message.contains("0.9.9"), "{}", mismatch.message);

            let crash_script = dir.path().join("staged-crash");
            fs::write(&crash_script, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(&crash_script, fs::Permissions::from_mode(0o755)).unwrap();
            let crashed =
                verify_staged_binary_version(&crash_script, "0.2.0", CancellationToken::new())
                    .await
                    .unwrap_err();
            assert_eq!(crashed.code, ErrorCode::Failed);

            let good_script = dir.path().join("staged-good");
            fs::write(&good_script, "#!/bin/sh\necho 'legion 0.2.0'\n").unwrap();
            fs::set_permissions(&good_script, fs::Permissions::from_mode(0o755)).unwrap();
            verify_staged_binary_version(&good_script, "0.2.0", CancellationToken::new())
                .await
                .unwrap();
        }
    }

    #[test]
    fn download_commands_enforce_https_tls12_and_silent_powershell_progress() {
        let api = curl_api_args("https://api.github.com/repos/example/repo/releases");
        assert!(api.windows(2).any(|w| w == ["--proto", "=https"]));
        assert!(api.windows(2).any(|w| w == ["--proto-redir", "=https"]));
        assert!(api.contains(&"--tlsv1.2"));
        assert!(api.windows(2).any(|w| w == ["--max-time", "30"]));

        let dl = curl_download_args(
            "https://github.com/example/repo/releases/download/a/b",
            "/tmp/out",
        );
        assert!(dl.windows(2).any(|w| w == ["--proto", "=https"]));
        assert!(dl.windows(2).any(|w| w == ["--proto-redir", "=https"]));
        assert!(dl.contains(&"--tlsv1.2"));
        assert!(dl.windows(2).any(|w| w == ["--max-time", "120"]));

        assert!(PS_FETCH_API_SCRIPT.contains("$ProgressPreference = 'SilentlyContinue'"));
        assert!(PS_FETCH_API_SCRIPT.contains("Tls12"));
        assert!(PS_DOWNLOAD_FILE_SCRIPT.contains("$ProgressPreference = 'SilentlyContinue'"));
        assert!(PS_DOWNLOAD_FILE_SCRIPT.contains("Tls12"));
    }

    #[test]
    fn resolve_target_handles_rosetta_and_windows_arm64_emulation() {
        let rosetta = resolve_target_for("macos", "x86_64", true).unwrap();
        assert_eq!(rosetta.arch, "aarch64");
        assert_eq!(rosetta.target, "aarch64-apple-darwin");
        assert!(rosetta.note.unwrap().contains("Rosetta 2"));

        let intel = resolve_target_for("macos", "x86_64", false).unwrap();
        assert_eq!(intel.arch, "x86_64");
        assert_eq!(intel.target, "x86_64-apple-darwin");
        assert_eq!(intel.note, None);

        let win_arm = resolve_target_for("windows", "aarch64", false).unwrap();
        assert_eq!(win_arm.arch, "x86_64");
        assert_eq!(win_arm.target, "x86_64-pc-windows-msvc");
        assert!(win_arm.note.unwrap().contains("emulation"));
    }

    struct ScriptedProber {
        outcomes: std::sync::Mutex<std::collections::VecDeque<Result<ParsedVersion, ErrorBody>>>,
    }

    impl ScriptedProber {
        fn new(outcomes: Vec<Result<ParsedVersion, ErrorBody>>) -> Self {
            Self {
                outcomes: std::sync::Mutex::new(outcomes.into()),
            }
        }
    }

    impl BinaryProber for ScriptedProber {
        fn probe<'a>(
            &'a self,
            _binary: &'a Path,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<ParsedVersion, ErrorBody>> {
            Box::pin(async move {
                self.outcomes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| {
                        Err(ErrorBody::new(ErrorCode::Internal, "unexpected probe call"))
                    })
            })
        }
    }

    #[test]
    fn parse_legion_version_accepts_old_and_new_forms() {
        assert_eq!(
            parse_legion_version_line("legion 0.1.0\n"),
            Some(ParsedVersion {
                version: "0.1.0".to_owned(),
                build: None,
            })
        );
        assert_eq!(
            parse_legion_version_line("legion 0.1.0 (a59ec0d12345)\n"),
            Some(ParsedVersion {
                version: "0.1.0".to_owned(),
                build: Some("a59ec0d12345".to_owned()),
            })
        );
        assert_eq!(
            parse_legion_version_line(
                "MOTD banner\r\nlegion 0.2.0-rc.1 (a59ec0d12345, modified)\r\n"
            ),
            Some(ParsedVersion {
                version: "0.2.0-rc.1".to_owned(),
                build: Some("a59ec0d12345, modified".to_owned()),
            })
        );
        assert_eq!(
            parse_legion_version_output("legion 0.1.0 (a59ec0d12345, modified)\n").as_deref(),
            Some("0.1.0")
        );
        assert_eq!(parse_legion_version_line("not legion\n"), None);
    }

    #[tokio::test]
    async fn meta_update_from_local_file_checks_format_downgrade_dry_run_and_rollback() {
        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let install_dir = dir.path().join("install");
        fs::create_dir_all(&install_dir).unwrap();
        let exe = install_dir.join("legion");
        let original = b"original-legion-0.2.0".to_vec();
        fs::write(&exe, &original).unwrap();

        let target = "x86_64-unknown-linux-musl";
        let mut local_bytes = crate::binfmt::synthetic_header_for_target(target);
        local_bytes.extend_from_slice(b"local-legion-0.3.0");
        let local_bin = dir.path().join("legion-local");
        fs::write(&local_bin, &local_bytes).unwrap();

        let fetcher = FakeFetcher::new("0.9.9", HashMap::new());

        // 1. `from` and `version` together are invalid_args.
        let both = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            None,
            UpdateArgs {
                version: Some("0.3.0".to_owned()),
                from: Some(local_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(both.code, ErrorCode::InvalidArgs);

        // 2. Relative, missing, directory, and empty `from` paths are refused.
        for (bad_path, expected_code) in [
            ("relative/legion".to_owned(), ErrorCode::InvalidArgs),
            (
                dir.path().join("does-not-exist").display().to_string(),
                ErrorCode::NotFound,
            ),
            (dir.path().display().to_string(), ErrorCode::InvalidArgs),
        ] {
            let err = run_meta_update_inner(
                &paths,
                &fetcher,
                "linux",
                "x86_64",
                "0.2.0",
                &exe,
                false,
                &RELEASE_PUBLIC_KEY,
                None,
                UpdateArgs {
                    version: None,
                    from: Some(bad_path),
                    dry_run: false,
                    allow_downgrade: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, expected_code);
        }

        // 3. Wrong architecture local binary is refused before staging.
        let wrong_arch_bin = dir.path().join("legion-arm64");
        fs::write(
            &wrong_arch_bin,
            crate::binfmt::synthetic_header_for_target("aarch64-unknown-linux-musl"),
        )
        .unwrap();
        let wrong_arch_err = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            None,
            UpdateArgs {
                version: None,
                from: Some(wrong_arch_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(wrong_arch_err.code, ErrorCode::InvalidArgs);
        assert!(wrong_arch_err.message.contains("aarch64"));
        assert_eq!(fs::read(&exe).unwrap(), original);

        // 4. Dry run reports planned version and build without modifying exe, .prev, or downloads.
        let parsed_030 = || ParsedVersion {
            version: "0.3.0".to_owned(),
            build: Some("a59ec0d12345".to_owned()),
        };
        let dry_prober = ScriptedProber::new(vec![Ok(parsed_030())]);
        let dry = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            Some(&dry_prober),
            UpdateArgs {
                version: None,
                from: Some(local_bin.display().to_string()),
                dry_run: true,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(dry.dry_run && !dry.updated);
        assert_eq!(dry.installed_version, "0.3.0");
        assert_eq!(dry.build.as_deref(), Some("a59ec0d12345"));
        assert_eq!(fetcher.download_count(), 0);
        assert_eq!(fs::read(&exe).unwrap(), original);
        assert!(!prev_binary_path(&exe).unwrap().exists());

        // 5. Downgrade via `from` is refused unless `allow_downgrade` is set.
        let down_prober = ScriptedProber::new(vec![Ok(ParsedVersion {
            version: "0.1.0".to_owned(),
            build: None,
        })]);
        let down_err = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            Some(&down_prober),
            UpdateArgs {
                version: None,
                from: Some(local_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(down_err.code, ErrorCode::InvalidArgs);
        assert_eq!(fs::read(&exe).unwrap(), original);

        // 6. If the newly placed binary fails to run after swap, `.prev` is restored automatically.
        let rollback_prober = ScriptedProber::new(vec![
            Ok(parsed_030()),
            Err(ErrorBody::new(
                ErrorCode::Failed,
                "exec failed after commit",
            )),
        ]);
        let rollback_err = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            Some(&rollback_prober),
            UpdateArgs {
                version: None,
                from: Some(local_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(rollback_err.code, ErrorCode::Failed);
        assert!(
            rollback_err.message.contains("restored") && rollback_err.message.contains(".prev"),
            "{}",
            rollback_err.message
        );
        assert_eq!(fs::read(&exe).unwrap(), original);

        // 7. Successful local install saves `.prev`, replaces `exe`, reports `build`, and commits
        // the version floor.
        let ok_prober = ScriptedProber::new(vec![Ok(parsed_030()), Ok(parsed_030())]);
        let ok = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.2.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            Some(&ok_prober),
            UpdateArgs {
                version: None,
                from: Some(local_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(ok.updated && !ok.dry_run);
        assert_eq!(ok.previous_version, "0.2.0");
        assert_eq!(ok.installed_version, "0.3.0");
        assert_eq!(ok.build.as_deref(), Some("a59ec0d12345"));
        assert_eq!(fetcher.download_count(), 0);
        assert_eq!(fs::read(&exe).unwrap(), local_bytes);
        assert_eq!(fs::read(prev_binary_path(&exe).unwrap()).unwrap(), original);
        assert_eq!(
            crate::floor::read_floor(&node_floor_path(&paths)),
            crate::floor::FloorState::Recorded(SemVer::parse("0.3.0").unwrap())
        );

        // 8. Downgrade below the recorded floor with `from` succeeds when `allow_downgrade=true`.
        let parsed_010 = || ParsedVersion {
            version: "0.1.0".to_owned(),
            build: None,
        };
        let allow_prober = ScriptedProber::new(vec![Ok(parsed_010()), Ok(parsed_010())]);
        let downgraded = run_meta_update_inner(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.3.0",
            &exe,
            false,
            &RELEASE_PUBLIC_KEY,
            Some(&allow_prober),
            UpdateArgs {
                version: None,
                from: Some(local_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(downgraded.updated);
        assert_eq!(downgraded.installed_version, "0.1.0");
        assert_eq!(downgraded.build, None);
        assert_eq!(
            crate::floor::read_floor(&node_floor_path(&paths)),
            crate::floor::FloorState::Recorded(SemVer::parse("0.1.0").unwrap())
        );
    }

    #[tokio::test]
    async fn meta_update_refuses_package_managed_installations() {
        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let usr_bin = dir.path().join("usr").join("bin");
        let usr_lib = dir.path().join("usr").join("lib").join("legion");
        fs::create_dir_all(&usr_bin).unwrap();
        fs::create_dir_all(&usr_lib).unwrap();
        let exe = usr_bin.join("legion");
        fs::write(&exe, b"packaged-binary").unwrap();
        let local_bin = dir.path().join("custom-legion");
        fs::write(&local_bin, b"custom-binary").unwrap();
        let fetcher = FakeFetcher::new("0.2.0", HashMap::new());

        // Debian/Ubuntu package marker refuses normal, dry-run, and `from` updates.
        fs::write(usr_lib.join("managed-by"), "deb\n").unwrap();
        for args in [
            UpdateArgs {
                version: None,
                from: None,
                dry_run: false,
                allow_downgrade: false,
            },
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                from: None,
                dry_run: true,
                allow_downgrade: false,
            },
            UpdateArgs {
                version: None,
                from: Some(local_bin.display().to_string()),
                dry_run: false,
                allow_downgrade: false,
            },
        ] {
            let err = run_meta_update(
                &paths,
                &fetcher,
                "linux",
                "x86_64",
                "0.1.0",
                &exe,
                false,
                args,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Unsupported);
            assert!(err.message.contains("deb (apt)"), "{}", err.message);
            assert!(
                err.hint.as_deref().unwrap().contains("apt-get install"),
                "{:?}",
                err.hint
            );
            assert_eq!(err.details.as_ref().unwrap()["managed_by"], "deb");
        }
        assert_eq!(fetcher.download_count(), 0);
        assert_eq!(fs::read(&exe).unwrap(), b"packaged-binary");

        // Arch pacman marker refuses with pacman hint.
        fs::write(usr_lib.join("managed-by"), "pacman\n").unwrap();
        let err = run_meta_update(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &exe,
            false,
            UpdateArgs::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("pacman"), "{}", err.message);
        assert!(
            err.hint.as_deref().unwrap().contains("pacman"),
            "{:?}",
            err.hint
        );
        assert_eq!(err.details.as_ref().unwrap()["managed_by"], "pacman");
    }
}
