//! Sync folder serving over the existing `legion stream` bulk pipe (`sync.index`, `sync.fetch`,
//! and persistent multi-request `sync.session`).
//!
//! Security & safety guarantees enforced on every request:
//! 1. Only a fleet machine listed in `fleet.toml` AND listed as a current member of the folder in
//!    this node's `sync.toml` may access it.
//! 2. Access is confined strictly inside that folder's configured directory path.
//! 3. `.legion-sync/marker.json` must exist and match `(folder_id, this_node)` (and the active
//!    index incarnation).
//! 4. `.legion-sync` at any depth (including `.legion-sync/bin` and `.legion-sync/tmp`, case-folded
//!    and Unicode NFKC variants) and folder-ignored paths are always refused.
//! 5. Every folder root and every served file path is checked against this node's own `policy.toml`
//!    (both the stream principal's `Grant` and, when `policy.toml` defines an explicit principal
//!    for the calling machine, that machine's `Grant` as well).
//! 6. Pages are capped at [`MAX_PAGE_RECORDS`] and file fetches stream strictly `remaining` bytes
//!    (bounded by the verified file size).

use std::fs::{self, File};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use legion_core::config::ConfigStore;
use legion_core::policy::{OWNER, Policy};
use legion_core::{Access, Grant};
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_sync::{
    FolderMarker, IgnoreMatcher, MAX_PAGE_RECORDS, PeerHandshake, RealFs, RecordsPage,
    RedbIndexStore, SyncFs, check_fetch_file_from_store, is_internal_sync_path,
    serve_handshake_from_store, serve_records_since_from_store, validate_folder_id,
    validate_member_id, validate_sync_path,
};
use legion_tools::sync::config::{SyncFile, SyncFolderEntry, load_sync_file};
use legion_tools::sync::control::{
    BlockingControlClient, ControlRequest, ControlResponse, folder_index_path,
};
use legion_tools::sync::{expand_member_path, sync_error_to_body};
use legion_xfer::Hash;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::FleetFile;
use crate::stream::{Failure, Host};

/// Chunk buffer size for raw disk-to-pipe file streaming (256 KiB).
const FETCH_STREAM_CHUNK: usize = 256 * 1024;

/// Maximum JSON request line size accepted inside `sync.session`.
const MAX_SESSION_REQ_BYTES: usize = 64 * 1024;

/// Timeout for local runner IPC metadata checks.
const RUNNER_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// Reply for a `sync.index` request (either a handshake or a records page).
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum SyncIndexReply {
    Handshake { handshake: PeerHandshake },
    Page { page: RecordsPage },
}

/// Ready line written before raw bytes of a `sync.fetch` (or `sync.session` fetch).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SyncFetchReady {
    pub size: u64,
    pub remaining: u64,
}

/// Ready line written after `sync.session` header is authorised.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SyncSessionReady {
    pub ready: bool,
}

/// One framed request sent on stdin during a persistent `sync.session`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
pub(crate) enum SessionRequest {
    Handshake,
    Index {
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    },
    Fetch {
        path: String,
        expected_hash: Hash,
        #[serde(default)]
        offset: u64,
    },
    Close,
}

/// Authorized, verified folder context for one sync stream operation or session.
struct VerifiedFolder {
    folder_id: String,
    caller: String,
    local_root: PathBuf,
    ignore: IgnoreMatcher,
    grants: Vec<Grant>,
}

impl VerifiedFolder {
    fn open(
        host: &Host,
        stream_grant: &Grant,
        folder_id: &str,
        caller: &str,
    ) -> Result<Self, ErrorBody> {
        validate_folder_id(folder_id).map_err(sync_error_to_body)?;
        validate_member_id(caller).map_err(sync_error_to_body)?;

        let here = host.name();
        if caller == here {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("Machine {caller:?} cannot pull from itself over sync stream."),
            ));
        }

        // 1. Caller must be a known fleet machine in this node's fleet.toml.
        let config = ConfigStore::new(host.paths().config_dir());
        let fleet_file = config.get::<FleetFile>()?;
        if fleet_file.machine(caller).is_none() {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("Machine {caller:?} is not listed in this node's fleet.toml."),
            )
            .with_hint("Only fleet machines listed in fleet.toml and the folder's members may sync."));
        }

        // 2. Folder must exist in this node's sync.toml with BOTH this node and caller as members.
        let sync_file: SyncFile = load_sync_file(host.paths())?;
        let Some(folder): Option<&SyncFolderEntry> = sync_file.folders.get(folder_id) else {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("Sync folder {folder_id:?} is not configured on {here}."),
            ));
        };
        if !folder.members.contains_key(caller) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("Machine {caller:?} is not a member of sync folder {folder_id:?}."),
            )
            .with_hint("Add the machine with `legion sync.add` before syncing."));
        }
        let Some(local_member) = folder.members.get(here) else {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("Machine {here:?} is not a member of sync folder {folder_id:?}."),
            ));
        };

        // 3. Build effective grants from this node's own policy.toml: always enforce the stream's
        //    grant, AND if policy.toml explicitly defines a principal for the calling machine,
        //    enforce that machine's grant as well.
        let mut grants = vec![stream_grant.clone()];
        let policy = config.get::<Policy>()?;
        if stream_grant.principal() == OWNER {
            for candidate in [caller.to_owned(), format!("machine:{caller}")] {
                if policy
                    .principals
                    .keys()
                    .any(|p| p.as_str() == candidate.as_str())
                {
                    let spec = ToolSpec {
                        name: "fs.read".to_owned(),
                        description: String::new(),
                        effect: Effect::Read,
                        input_schema: json!({"type": "object"}),
                    };
                    let machine_grant = policy.authorize(&candidate, &spec, host.paths())?;
                    grants.push(machine_grant);
                    break;
                }
            }
        }

        // 4. Check the folder root path against every effective policy grant.
        let expanded_root = expand_member_path(host.paths(), &local_member.path);
        let mut checked_root = expanded_root.clone();
        for g in &grants {
            checked_root = g.check_path(&expanded_root, Access::Read)?;
        }

        // 5. Verify `.legion-sync/marker.json` exists and matches `(folder_id, here)`.
        verify_disk_marker(&checked_root, folder_id, here)?;

        let ignore = IgnoreMatcher::compile(&folder.ignore).map_err(sync_error_to_body)?;

        Ok(Self {
            folder_id: folder_id.to_owned(),
            caller: caller.to_owned(),
            local_root: checked_root,
            ignore,
            grants,
        })
    }

    /// Re-verifies the on-disk folder marker right before serving data.
    fn recheck_marker(&self, here: &str) -> Result<FolderMarker, ErrorBody> {
        verify_disk_marker(&self.local_root, &self.folder_id, here)
    }

    /// Validates a requested relative path, refuses `.legion-sync` / bin / tmp / ignored paths /
    /// symlink traversal, and checks the full target path against every effective `policy.toml`
    /// grant.
    fn resolve_and_authorize_file(&self, rel_path: &str) -> Result<(String, PathBuf), ErrorBody> {
        let clean = validate_sync_path(rel_path).map_err(sync_error_to_body)?;
        if is_internal_sync_path(&clean) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("Internal sync path {clean:?} is never served to peers."),
            ));
        }
        if self.ignore.is_path_or_ancestor_ignored(&clean, false) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("Path {clean:?} is excluded by this folder's ignore rules."),
            ));
        }

        // Walk component by component under `local_root`, refusing any symlink ancestor and
        // ensuring the resolved path stays strictly inside `local_root`.
        let mut cur = self.local_root.clone();
        let segments: Vec<&str> = clean.split('/').collect();
        for (idx, seg) in segments.iter().enumerate() {
            cur.push(seg);
            let meta = fs::symlink_metadata(&cur).map_err(|err| {
                let code = if err.kind() == io::ErrorKind::NotFound {
                    ErrorCode::NotFound
                } else if err.kind() == io::ErrorKind::PermissionDenied {
                    ErrorCode::Denied
                } else {
                    ErrorCode::Failed
                };
                ErrorBody::new(
                    code,
                    format!("Sync file {clean:?} cannot be accessed: {err}."),
                )
            })?;
            if meta.file_type().is_symlink() {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!("Path {clean:?} traverses or targets a symbolic link."),
                ));
            }
            if idx + 1 < segments.len() {
                if !meta.is_dir() {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("Parent of {clean:?} is not a directory."),
                    ));
                }
            } else if !meta.is_file() {
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("Path {clean:?} is not a regular file."),
                ));
            }
        }

        // Re-check every served file path against this node's own policy.toml grants.
        for g in &self.grants {
            g.check_path(&cur, Access::Read)?;
        }

        Ok((clean, cur))
    }
}

fn verify_disk_marker(root: &Path, folder_id: &str, here: &str) -> Result<FolderMarker, ErrorBody> {
    let real_fs = RealFs::open(root).map_err(sync_error_to_body)?;
    match real_fs.read_marker().map_err(sync_error_to_body)? {
        Some(m) if m.folder_id == folder_id && m.member_id == here && m.incarnation != 0 => Ok(m),
        _ => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Sync folder {folder_id:?} on {here} has a missing or mismatched folder identity marker."
            ),
        )
        .with_hint("Check that the folder or drive is mounted and `.legion-sync/marker.json` is intact.")),
    }
}

/// Index backend used by `sync_stream`: prefers a persistent connection to the local runner's
/// MVCC read endpoint, falling back to opening `RedbIndexStore` directly when no runner is running.
enum IndexBackend {
    Runner(BlockingControlClient),
    Direct {
        fs: RealFs,
        store: RedbIndexStore,
    },
}

impl IndexBackend {
    fn open(host: &Host, folder: &VerifiedFolder) -> Result<Self, ErrorBody> {
        if let Some(client) = BlockingControlClient::connect(host.paths(), RUNNER_CHECK_TIMEOUT)? {
            return Ok(Self::Runner(client));
        }
        let fs = RealFs::open(&folder.local_root).map_err(sync_error_to_body)?;
        let store = RedbIndexStore::open(folder_index_path(host.paths(), &folder.folder_id))
            .map_err(sync_error_to_body)?;
        Ok(Self::Direct { fs, store })
    }

    fn handshake(&mut self, folder: &VerifiedFolder) -> Result<PeerHandshake, ErrorBody> {
        match self {
            Self::Runner(client) => match client.call(ControlRequest::ServeHandshake {
                folder_id: folder.folder_id.clone(),
                caller: folder.caller.clone(),
            })? {
                ControlResponse::Handshake { handshake } => Ok(handshake),
                ControlResponse::Error { error } => Err(error),
                other => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Unexpected runner reply to ServeHandshake: {other:?}."),
                )),
            },
            Self::Direct { fs, store } => {
                serve_handshake_from_store(fs, store, &folder.caller).map_err(sync_error_to_body)
            }
        }
    }

    fn records_since(
        &mut self,
        folder: &VerifiedFolder,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage, ErrorBody> {
        let capped = limit.clamp(1, MAX_PAGE_RECORDS);
        match self {
            Self::Runner(client) => match client.call(ControlRequest::ServeRecordsSince {
                folder_id: folder.folder_id.clone(),
                caller: folder.caller.clone(),
                since_incarnation,
                since_seq,
                limit: capped,
            })? {
                ControlResponse::Records { page } => Ok(page),
                ControlResponse::Error { error } => Err(error),
                other => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Unexpected runner reply to ServeRecordsSince: {other:?}."),
                )),
            },
            Self::Direct { fs, store } => serve_records_since_from_store(
                fs,
                store,
                &folder.caller,
                since_incarnation,
                since_seq,
                capped,
            )
            .map_err(sync_error_to_body),
        }
    }

    fn check_fetch(
        &mut self,
        folder: &VerifiedFolder,
        clean_path: &str,
        expected_hash: Hash,
    ) -> Result<u64, ErrorBody> {
        match self {
            Self::Runner(client) => match client.call(ControlRequest::ServeFetchCheck {
                folder_id: folder.folder_id.clone(),
                caller: folder.caller.clone(),
                path: clean_path.to_owned(),
                expected_hash,
            })? {
                ControlResponse::FetchAllowed { size } => Ok(size),
                ControlResponse::Error { error } => Err(error),
                other => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("Unexpected runner reply to ServeFetchCheck: {other:?}."),
                )),
            },
            Self::Direct { fs, store } => check_fetch_file_from_store(
                fs,
                store,
                &folder.ignore,
                &folder.caller,
                clean_path,
                expected_hash,
            )
            .map_err(sync_error_to_body),
        }
    }
}

/// Serves one-shot `sync.index`: returns either `{ handshake }` (when `handshake_only` is true)
/// or `{ page }` for records after `(since_incarnation, since_seq)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_index<W: Write>(
    host: &Host,
    grant: &Grant,
    folder_id: &str,
    caller: &str,
    handshake_only: bool,
    since_incarnation: u64,
    since_seq: u64,
    limit: usize,
    output: &mut W,
) -> Result<(), Failure> {
    let folder = VerifiedFolder::open(host, grant, folder_id, caller)?;
    let mut backend = IndexBackend::open(host, &folder)?;
    let reply = if handshake_only {
        SyncIndexReply::Handshake {
            handshake: backend.handshake(&folder)?,
        }
    } else {
        SyncIndexReply::Page {
            page: backend.records_since(&folder, since_incarnation, since_seq, limit)?,
        }
    };
    write_json_line(output, &reply)
}

/// Serves one-shot `sync.fetch`: verifies membership, marker, policy, path, and expected hash,
/// writes `{ size, remaining }`, and streams exact `remaining` bytes directly from disk.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_fetch<W: Write>(
    host: &Host,
    grant: &Grant,
    folder_id: &str,
    caller: &str,
    rel_path: &str,
    expected_hash: &str,
    offset: u64,
    output: &mut W,
) -> Result<(), Failure> {
    let hash: Hash = expected_hash.parse().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("expected_hash {expected_hash:?} is not a valid BLAKE3 hash."),
        )
    })?;
    let folder = VerifiedFolder::open(host, grant, folder_id, caller)?;
    let mut backend = IndexBackend::open(host, &folder)?;
    stream_one_file(host, &folder, &mut backend, rel_path, hash, offset, output)
}

/// Serves a persistent multi-request `sync.session` over one `legion stream` pipe so pulling
/// thousands of files pays only a single process spawn and handshake.
pub(crate) fn serve_session<R: BufRead, W: Write>(
    host: &Host,
    grant: &Grant,
    folder_id: &str,
    caller: &str,
    input: &mut R,
    output: &mut W,
) -> Result<(), Failure> {
    let folder = VerifiedFolder::open(host, grant, folder_id, caller)?;
    let mut backend = IndexBackend::open(host, &folder)?;
    write_json_line(output, &SyncSessionReady { ready: true })?;

    let mut line = Vec::new();
    loop {
        line.clear();
        let read = input
            .by_ref()
            .take(MAX_SESSION_REQ_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|err| {
                Failure::Raw(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Sync session input failed: {err}."),
                ))
            })?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_SESSION_REQ_BYTES {
            let err = ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Sync session request exceeded {MAX_SESSION_REQ_BYTES} bytes."),
            );
            let _ = write_json_line(output, &json!({ "error": err }));
            return Err(Failure::Raw(err));
        }

        let req: SessionRequest = match serde_json::from_slice(&line) {
            Ok(req) => req,
            Err(err) => {
                let body = ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Sync session request is not valid JSON: {err}."),
                );
                let _ = write_json_line(output, &json!({ "error": body }));
                return Err(Failure::Raw(body));
            }
        };

        match req {
            SessionRequest::Close => return Ok(()),
            SessionRequest::Handshake => match folder
                .recheck_marker(host.name())
                .and_then(|_| backend.handshake(&folder))
            {
                Ok(hs) => write_json_line(output, &SyncIndexReply::Handshake { handshake: hs })?,
                Err(err) => write_json_line(output, &json!({ "error": err }))?,
            },
            SessionRequest::Index {
                since_incarnation,
                since_seq,
                limit,
            } => match folder
                .recheck_marker(host.name())
                .and_then(|_| backend.records_since(&folder, since_incarnation, since_seq, limit))
            {
                Ok(page) => write_json_line(output, &SyncIndexReply::Page { page })?,
                Err(err) => write_json_line(output, &json!({ "error": err }))?,
            },
            SessionRequest::Fetch {
                path,
                expected_hash,
                offset,
            } => {
                match stream_one_file(
                    host,
                    &folder,
                    &mut backend,
                    &path,
                    expected_hash,
                    offset,
                    output,
                ) {
                    Ok(()) => {}
                    // Pre-body error: report cleanly as JSON line so the session stays framed.
                    Err(Failure::Reply(err)) => {
                        write_json_line(output, &json!({ "error": err }))?;
                    }
                    // Mid-body I/O error: pipe framing is broken; close the stream cleanly.
                    Err(Failure::Raw(err)) => return Err(Failure::Raw(err)),
                }
            }
        }
    }
}

fn stream_one_file<W: Write>(
    host: &Host,
    folder: &VerifiedFolder,
    backend: &mut IndexBackend,
    rel_path: &str,
    expected_hash: Hash,
    offset: u64,
    output: &mut W,
) -> Result<(), Failure> {
    folder.recheck_marker(host.name())?;
    let (clean, full_path) = folder.resolve_and_authorize_file(rel_path)?;
    let indexed_size = backend.check_fetch(folder, &clean, expected_hash)?;

    if offset > indexed_size {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Requested offset {offset} exceeds size {indexed_size} of {clean:?}."),
        )));
    }

    let mut file = File::open(&full_path).map_err(|err| {
        let code = if err.kind() == io::ErrorKind::NotFound {
            ErrorCode::NotFound
        } else if err.kind() == io::ErrorKind::PermissionDenied {
            ErrorCode::Denied
        } else {
            ErrorCode::Failed
        };
        Failure::Reply(ErrorBody::new(
            code,
            format!("Could not open sync file {clean:?}: {err}."),
        ))
    })?;
    let meta = file.metadata().map_err(|err| {
        Failure::Reply(ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not stat sync file {clean:?}: {err}."),
        ))
    })?;
    if !meta.is_file() {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::NotFound,
            format!("Sync path {clean:?} is no longer a regular file."),
        )));
    }
    let actual_size = meta.len();
    if actual_size != indexed_size {
        return Err(Failure::Reply(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Sync file {clean:?} changed on disk before transfer (indexed {indexed_size} bytes, now {actual_size} bytes)."
            ),
        )));
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).map_err(|err| {
            Failure::Reply(ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not seek sync file {clean:?} to offset {offset}: {err}."),
            ))
        })?;
    }

    let remaining = actual_size.saturating_sub(offset);
    write_json_line(
        output,
        &SyncFetchReady {
            size: actual_size,
            remaining,
        },
    )?;

    if remaining == 0 {
        return Ok(());
    }

    let mut left = remaining;
    let mut buf = vec![0u8; FETCH_STREAM_CHUNK];
    while left > 0 {
        let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = file.read(&mut buf[..want]).map_err(|err| {
            Failure::Raw(ErrorBody::new(
                ErrorCode::Failed,
                format!("Reading sync file {clean:?} failed mid-stream: {err}."),
            ))
        })?;
        if n == 0 {
            return Err(Failure::Raw(ErrorBody::new(
                ErrorCode::Conflict,
                format!("Sync file {clean:?} shrank on disk during transfer."),
            )));
        }
        output.write_all(&buf[..n]).map_err(|err| {
            Failure::Raw(ErrorBody::new(
                ErrorCode::Failed,
                format!("The other end of the sync stream went away: {err}."),
            ))
        })?;
        left = left.saturating_sub(n as u64);
    }
    output.flush().map_err(|err| {
        Failure::Raw(ErrorBody::new(
            ErrorCode::Failed,
            format!("Flushing sync stream failed: {err}."),
        ))
    })?;
    Ok(())
}

fn write_json_line<W: Write, T: Serialize>(output: &mut W, value: &T) -> Result<(), Failure> {
    serde_json::to_writer(&mut *output, value)
        .map_err(|err| {
            Failure::Raw(ErrorBody::new(
                ErrorCode::Internal,
                format!("Sync stream reply could not be serialised: {err}."),
            ))
        })?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|err| {
            Failure::Raw(ErrorBody::new(
                ErrorCode::Failed,
                format!("The other end of the sync stream went away: {err}."),
            ))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::Cursor;

    use legion_core::Paths;
    use legion_sync::{FolderConfig, RealFs, RedbIndexStore, SyncEngine};
    use legion_tools::sync::config::{
        DEFAULT_BIN_DAYS, SyncFile, SyncFolderEntry, SyncMemberEntry, format_stamp_time,
    };
    use legion_tools::sync::control::folder_index_path;
    use legion_tools::sync::now_xfer_timestamp;

    use crate::stream::{Host, STREAM_PROTOCOL, serve};

    fn setup_host_with_folder(
        policy_toml: Option<&str>,
    ) -> (tempfile::TempDir, Paths, Host, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::create_dir_all(paths.state_dir()).unwrap();

        let folder_root = dir.path().join("shared");
        std::fs::create_dir_all(&folder_root).unwrap();
        std::fs::write(folder_root.join("hello.txt"), b"hello world!").unwrap();

        std::fs::write(
            paths.config_file("fleet.toml"),
            "[machines.beta]\ncommand = [\"echo\"]\n",
        )
        .unwrap();
        if let Some(pol) = policy_toml {
            std::fs::write(paths.config_file("policy.toml"), pol).unwrap();
        }

        let now_str = format_stamp_time(jiff::Timestamp::now());
        let mut members = std::collections::BTreeMap::new();
        members.insert(
            "alpha".to_owned(),
            SyncMemberEntry {
                path: folder_root.display().to_string(),
                added_at: now_str.clone(),
                added_on: "alpha".to_owned(),
                updated_at: now_str.clone(),
                updated_on: "alpha".to_owned(),
            },
        );
        members.insert(
            "beta".to_owned(),
            SyncMemberEntry {
                path: "/remote/beta".to_owned(),
                added_at: now_str.clone(),
                added_on: "alpha".to_owned(),
                updated_at: now_str.clone(),
                updated_on: "alpha".to_owned(),
            },
        );
        let mut sf = SyncFile::default();
        sf.folders.insert(
            "docs".to_owned(),
            SyncFolderEntry {
                name: "Docs".to_owned(),
                ignore: vec!["*.secret".to_owned()],
                paused: false,
                bin_days: DEFAULT_BIN_DAYS,
                created_at: now_str.clone(),
                created_on: "alpha".to_owned(),
                updated_at: now_str.clone(),
                updated_on: "alpha".to_owned(),
                name_updated_at: None,
                name_updated_on: None,
                ignore_updated_at: None,
                ignore_updated_on: None,
                paused_updated_at: None,
                paused_updated_on: None,
                bin_days_updated_at: None,
                bin_days_updated_on: None,
                members,
                removed_members: std::collections::BTreeMap::new(),
            },
        );
        std::fs::write(
            paths.config_file("sync.toml"),
            sf.to_canonical_toml().unwrap(),
        )
        .unwrap();

        let cfg = FolderConfig::new("docs", "alpha", ["alpha".to_owned(), "beta".to_owned()]);
        let fs = RealFs::open(&folder_root).unwrap();
        let store = RedbIndexStore::open(folder_index_path(&paths, "docs")).unwrap();
        let mut engine = SyncEngine::open(cfg, fs, store).unwrap();
        engine.scan(now_xfer_timestamp()).unwrap();

        let hash = legion_xfer::hash::hash_bytes(b"hello world!").to_string();
        let host = Host::new(paths.clone(), "alpha");
        (dir, paths, host, folder_root, hash)
    }

    fn run_stream(host: &Host, pinned: Option<&str>, input: &str) -> (Result<(), ErrorBody>, Vec<u8>) {
        let mut out = Vec::new();
        let res = serve(host, pinned, Cursor::new(input.as_bytes()), &mut out);
        (res, out)
    }

    #[test]
    fn serves_member_and_refuses_non_member_internal_paths_bad_marker_and_policy() {
        let (_dir, _paths, host, folder_root, hash) = setup_host_with_folder(None);

        // 1. Authorized member gets handshake, index page, and ranged fetch.
        let hs_req = format!(
            "{{\"protocol\":{STREAM_PROTOCOL},\"op\":\"sync.index\",\"folder\":\"docs\",\"caller\":\"beta\",\"handshake_only\":true}}\n"
        );
        let (res, out) = run_stream(&host, None, &hs_req);
        assert!(res.is_ok(), "handshake failed: {res:?}");
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("\"handshake\""));

        let fetch_req = format!(
            "{{\"protocol\":{STREAM_PROTOCOL},\"op\":\"sync.fetch\",\"folder\":\"docs\",\"caller\":\"beta\",\"path\":\"hello.txt\",\"expected_hash\":\"{hash}\",\"offset\":6}}\n"
        );
        let (res, out) = run_stream(&host, None, &fetch_req);
        assert!(res.is_ok(), "fetch failed: {res:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with("world!"), "expected ranged tail, got: {text:?}");

        // 2. Non-fleet / non-member caller is refused.
        let bad_caller = format!(
            "{{\"protocol\":{STREAM_PROTOCOL},\"op\":\"sync.index\",\"folder\":\"docs\",\"caller\":\"stranger\",\"handshake_only\":true}}\n"
        );
        let (res, _) = run_stream(&host, None, &bad_caller);
        assert_eq!(res.unwrap_err().code, ErrorCode::Denied);

        // 3. `.legion-sync/bin`, `.legion-sync/tmp`, `.legion-sync/marker.json`, `..` are refused.
        for forbidden in [
            ".legion-sync/marker.json",
            ".legion-sync/bin/item",
            ".legion-sync/tmp/part",
            "../outside.txt",
            "ignored.secret",
        ] {
            let req = format!(
                "{{\"protocol\":{STREAM_PROTOCOL},\"op\":\"sync.fetch\",\"folder\":\"docs\",\"caller\":\"beta\",\"path\":\"{forbidden}\",\"expected_hash\":\"{hash}\"}}\n"
            );
            let (res, _) = run_stream(&host, None, &req);
            assert!(res.is_err(), "expected {forbidden} to be refused");
        }

        // 4. Missing marker stops serving immediately.
        std::fs::remove_file(folder_root.join(".legion-sync/marker.json")).unwrap();
        let (res, _) = run_stream(&host, None, &hs_req);
        assert!(res.is_err(), "expected missing marker to refuse serving");
    }

    #[test]
    fn enforces_policy_toml_for_calling_machine() {
        let pol = "\
[principals.beta]
role = \"confined\"

[roles.confined]
allow = [\"fs.read\"]
paths = [\"/only/elsewhere/**\"]
";
        let (_dir, _paths, host, _folder_root, _hash) = setup_host_with_folder(Some(pol));
        let hs_req = format!(
            "{{\"protocol\":{STREAM_PROTOCOL},\"op\":\"sync.index\",\"folder\":\"docs\",\"caller\":\"beta\",\"handshake_only\":true}}\n"
        );
        let (res, _) = run_stream(&host, None, &hs_req);
        let err = res.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "expected policy denial: {err:?}");
    }
}
