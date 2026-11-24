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

/// Fixed PowerShell snippet for fetching the GitHub releases JSON without interpolating any input.
const PS_FETCH_API_SCRIPT: &str = "[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; \
(Invoke-WebRequest -Uri $env:LEGION_DOWNLOAD_URL -Headers @{ 'Accept'='application/vnd.github+json'; 'User-Agent'='legion' } -UseBasicParsing).Content";

/// Fixed PowerShell snippet for downloading one URL into `$env:LEGION_DOWNLOAD_DEST`.
const PS_DOWNLOAD_FILE_SCRIPT: &str = "[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; \
Invoke-WebRequest -Uri $env:LEGION_DOWNLOAD_URL -OutFile $env:LEGION_DOWNLOAD_DEST -Headers @{ 'User-Agent'='legion' } -UseBasicParsing";

/// Registers `meta.update` on `registry`.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "meta.update",
        Effect::Admin,
        "Replace this node's legion binary with a verified GitHub release artifact (downloading and checking SHA256SUMS, then replacing atomically). On Windows, renames the running executable aside first so it can be cleaned up on the next start.",
        meta_update,
    );
}

/// Arguments of `meta.update`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateArgs {
    /// Release version to install (for example `0.2.0` or `legion-v0.2.0`). Defaults to the latest
    /// published `legion-v*` release.
    #[serde(default)]
    pub version: Option<String>,
    /// Resolve the target release and report what would be replaced without downloading or writing.
    #[serde(default)]
    pub dry_run: bool,
}

/// Result of `meta.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateResult {
    /// Version this node was running before the update.
    pub previous_version: String,
    /// Version installed (or planned when `dry_run` is true).
    pub installed_version: String,
    /// Release target triple selected for this node.
    pub target: String,
    /// Path to the executable that was (or would be) replaced.
    pub path: String,
    /// Whether this call only reported what would happen without modifying the binary.
    pub dry_run: bool,
    /// Whether the executable on disk was replaced.
    pub updated: bool,
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

/// Selects the newest non-draft, non-prerelease `legion-v*` version from GitHub's `/releases` JSON.
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

    for entry in entries {
        if !entry.draft && !entry.prerelease && entry.tag_name.starts_with(RELEASE_TAG_PREFIX) {
            return normalize_version(&entry.tag_name);
        }
    }

    Err(ErrorBody::new(
        ErrorCode::NotFound,
        format!(
            "No published {RELEASE_TAG_PREFIX}* release was found at {RELEASE_REPO_URL}/releases."
        ),
    )
    .with_hint("Specify an explicit `version` or install from a local binary with `from`."))
}

/// Maps a normalized `(os, arch)` pair to the release target triple published by CI.
pub fn target_triple_for(os: &str, arch: &str) -> Result<&'static str, ErrorBody> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("linux", "arm" | "armv6" | "armv6l" | "armv7" | "armv7l" | "armhf" | "armel") => {
            Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "32-bit ARM Linux ({arch}) is not supported; Legion publishes 64-bit Linux binaries (x86_64 and aarch64)."
                ),
            )
            .with_hint("Use a 64-bit OS image (aarch64) on ARM boards such as Raspberry Pi."))
        }
        ("windows", "aarch64") => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "Native Windows ARM64 release binaries are not published yet.",
        )
        .with_hint(
            "Pass a local binary via `from`, or build locally for aarch64-pc-windows-msvc.",
        )),
        _ => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!("No Legion release binary is published for {os}/{arch}."),
        )
        .with_hint(
            "Supported release targets are Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).",
        )),
    }
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
/// under `<state_dir>/dist/<version>/<target>/` when already present or downloading and verifying
/// against `SHA256SUMS` through `fetcher`.
pub async fn resolve_release_binary(
    paths: &Paths,
    fetcher: &dyn ReleaseFetcher,
    version: &str,
    target: &str,
    cancel: CancellationToken,
) -> Result<PathBuf, ErrorBody> {
    let version = normalize_version(version)?;
    let asset = artifact_filename(&version, target);
    let cache_dir = dist_cache_dir(paths, &version, target);
    let cached_bin = cache_dir.join(&asset);
    let cached_sums = cache_dir.join(CHECKSUMS_FILENAME);

    if let Ok(meta) = fs::metadata(&cached_bin)
        && meta.is_file()
        && meta.len() > 0
    {
        if let Ok(sums_text) = fs::read_to_string(&cached_sums) {
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
        } else {
            // An operator may pre-populate dist/<version>/<target>/ on an air-gapped node without
            // SHA256SUMS; accept a non-empty cached binary when no manifest sits beside it.
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
    let tmp_bin = cache_dir.join(format!(".{asset}.tmp.{token}"));
    let mut sums_guard = TempCleanup::new(tmp_sums.clone());
    let mut bin_guard = TempCleanup::new(tmp_bin.clone());

    let sums_url = checksums_url(&version);
    fetcher
        .download_to_file(&sums_url, &tmp_sums, cancel.clone())
        .await?;
    let bin_url = artifact_url(&version, target);
    fetcher.download_to_file(&bin_url, &tmp_bin, cancel).await?;

    let sums_text = fs::read_to_string(&tmp_sums).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not read downloaded {CHECKSUMS_FILENAME}: {err}."),
        )
    })?;

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

    // Commit both files atomically only after the digest matches so the cache never exposes
    // an unverified or partially downloaded binary.
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
    if let Ok(exe) = std::env::current_exe() {
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

/// Replaces `target_exe` with the contents of `new_binary` atomically.
///
/// Stages `new_binary` beside `target_exe` on the same filesystem so renames never cross mounts.
/// When `rename_old_first` is `true` (used on Windows because a running executable cannot be
/// overwritten in place), `target_exe` is moved to a sibling `.old.<hex>` path first; if placing
/// the staged binary fails, the previous binary is moved back.
pub fn replace_executable(
    target_exe: &Path,
    new_binary: &Path,
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

    let token = random_hex(8)?;
    let staged_path = parent.join(format!(".{file_name}.tmp.{token}"));
    let mut staged_guard = TempCleanup::new(staged_path.clone());

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

    if rename_old_first && target_exe.exists() {
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
        if let Err(err) = replace_file(&staged_path, target_exe) {
            // Roll back so the machine is never left without its legion executable.
            let _ = replace_file(&old_path, target_exe);
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not move updated binary into {}: {err}.",
                    target_exe.display()
                ),
            ));
        }
        staged_guard.disarm();
        // Best-effort removal: on Windows the running executable stays locked until exit and is
        // removed on the next start by `cleanup_old_binary`.
        let _ = fs::remove_file(&old_path);
    } else {
        // Use WriteOptions-compatible atomic replacement on the same filesystem.
        let _ = WriteOptions::with_mode(0o755);
        replace_file(&staged_path, target_exe).map_err(|err| {
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

async fn meta_update(ctx: &Ctx, args: UpdateArgs) -> Result<UpdateResult, ToolError> {
    let current_exe = std::env::current_exe().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not locate the running legion executable: {err}."),
        )
    })?;
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
/// or the test runner binary.
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
    let target = target_triple_for(os, arch)?;
    let explicit_version = args.version.is_some();
    let desired_version = match args.version {
        Some(raw) => normalize_version(&raw)?,
        None => fetcher.latest_version(cancel.clone()).await?,
    };

    if args.dry_run {
        return Ok(UpdateResult {
            previous_version: current_version.to_owned(),
            installed_version: desired_version,
            target: target.to_owned(),
            path: current_exe.display().to_string(),
            dry_run: true,
            updated: false,
        });
    }

    if !explicit_version && desired_version == current_version {
        return Ok(UpdateResult {
            previous_version: current_version.to_owned(),
            installed_version: desired_version,
            target: target.to_owned(),
            path: current_exe.display().to_string(),
            dry_run: false,
            updated: false,
        });
    }

    let cached_bin =
        resolve_release_binary(paths, fetcher, &desired_version, target, cancel).await?;
    let target_exe = current_exe.to_path_buf();
    tokio::task::spawn_blocking(move || {
        replace_executable(&target_exe, &cached_bin, rename_old_first)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Replacing the legion binary stopped unexpectedly: {err}."),
        )
    })??;

    Ok(UpdateResult {
        previous_version: current_version.to_owned(),
        installed_version: desired_version,
        target: target.to_owned(),
        path: current_exe.display().to_string(),
        dry_run: false,
        updated: true,
    })
}

async fn fetch_api_text(url: &str, cancel: CancellationToken) -> Result<String, ErrorBody> {
    let curl_opts = RunOptions::new(CommandSpec::argv(
        "curl",
        [
            "-fsSL",
            "--retry",
            "2",
            "--connect-timeout",
            "15",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: legion",
            url,
        ],
    ))
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
    let curl_opts = RunOptions::new(CommandSpec::argv(
        "curl",
        [
            "-fsSL",
            "--retry",
            "2",
            "--connect-timeout",
            "15",
            "-H",
            "User-Agent: legion",
            "-o",
            dest_str,
            url,
        ],
    ))
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
    fn latest_release_parser_skips_drafts_prereleases_and_non_legion_tags() {
        let json = r#"[
            {"tag_name": "v2.0.0", "draft": false, "prerelease": false},
            {"tag_name": "legion-v0.4.0", "draft": true, "prerelease": false},
            {"tag_name": "legion-v0.3.0-rc1", "draft": false, "prerelease": true},
            {"tag_name": "legion-v0.2.1", "draft": false, "prerelease": false},
            {"tag_name": "legion-v0.1.0", "draft": false, "prerelease": false}
        ]"#;
        assert_eq!(parse_latest_release_version(json).unwrap(), "0.2.1");

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

        let new_bytes = b"new-legion-0.2.0-binary-content".to_vec();
        let target = "x86_64-unknown-linux-musl";
        let asset = artifact_filename("0.2.0", target);
        let sums = format!("{}  {asset}\n", sha256_bytes(&new_bytes));
        let mut files = HashMap::new();
        files.insert(checksums_url("0.2.0"), sums.into_bytes());
        files.insert(artifact_url("0.2.0", target), new_bytes.clone());
        let fetcher = FakeFetcher::new("0.2.0", files);

        // Dry run touches neither the network downloads nor the binary on disk.
        let dry = run_meta_update(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &current_copy,
            false,
            UpdateArgs {
                version: None,
                dry_run: true,
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

        // Real update downloads, verifies, caches, and atomically replaces the copy.
        let updated = run_meta_update(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &current_copy,
            false,
            UpdateArgs {
                version: None,
                dry_run: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!updated.dry_run);
        assert!(updated.updated);
        assert_eq!(updated.previous_version, "0.1.0");
        assert_eq!(updated.installed_version, "0.2.0");
        assert_eq!(fetcher.download_count(), 2);
        assert_eq!(fs::read(&current_copy).unwrap(), new_bytes);

        // A second explicit call reuses the verified dist cache without re-downloading.
        let cached = run_meta_update(
            &paths,
            &fetcher,
            "linux",
            "x86_64",
            "0.1.0",
            &current_copy,
            false,
            UpdateArgs {
                version: Some("0.2.0".to_owned()),
                dry_run: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(cached.updated);
        assert_eq!(fetcher.download_count(), 2);
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
        let dir = tempdir().unwrap();
        let paths = Paths::under(dir.path().join("home"));
        let exe = dir.path().join("legion");
        fs::write(&exe, b"original-good-binary").unwrap();

        let target = "x86_64-unknown-linux-musl";
        let asset = artifact_filename("0.2.0", target);
        let mut files = HashMap::new();
        files.insert(
            checksums_url("0.2.0"),
            format!("{}  {asset}\n", "0".repeat(64)).into_bytes(),
        );
        files.insert(artifact_url("0.2.0", target), b"corrupted".to_vec());
        let fetcher = FakeFetcher::new("0.2.0", files);

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
                dry_run: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert_eq!(fs::read(&exe).unwrap(), b"original-good-binary");
        assert!(!dist_cache_binary(&paths, "0.2.0", target).exists());
    }
}
