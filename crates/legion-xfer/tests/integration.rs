//! End-to-end integration tests exercising the public `legion_xfer` API.

use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::sync::mpsc;
use std::thread;

use legion_xfer::hash::{hash_bytes, hash_file};
use legion_xfer::{
    Error, HashingReader, HashingWriter, ManifestOptions, PlanOptions, get, manifest, pack,
    part_path, plan, put, resume_offset, unpack,
};
use tar::{EntryType, Header};

/// In-memory blocking byte pipe built from `mpsc` channels so tests can stream `pack -> unpack`
/// and `get -> put` concurrently across two threads without OS-specific pipe wrappers.
struct PipeWriter(mpsc::SyncSender<Vec<u8>>);

struct PipeReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

fn byte_pipe() -> (PipeWriter, PipeReader) {
    let (tx, rx) = mpsc::sync_channel(8);
    (
        PipeWriter(tx),
        PipeReader {
            rx,
            buf: Vec::new(),
            pos: 0,
        },
    )
}

impl Write for PipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.0
            .send(data.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pipe reader closed"))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn build_raw_tar(entries: &[(&str, EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, entry_type, data, link_target) in entries {
        let mut header = Header::new_gnu();
        header.set_entry_type(*entry_type);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        let raw_header = header.as_mut_bytes();
        let path_bytes = path.as_bytes();
        raw_header[..path_bytes.len()].copy_from_slice(path_bytes);
        if let Some(target) = link_target {
            let target_bytes = target.as_bytes();
            raw_header[157..157 + target_bytes.len()].copy_from_slice(target_bytes);
        }
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            out.resize(out.len() + (512 - rem), 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    out
}

#[test]
fn full_tree_round_trip_and_idempotent_second_plan() {
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path();
    let dst = dst_dir.path();

    // Nested dirs, empty dir, empty file, large file (> 2 MiB), spaces, unicode, leading dashes.
    fs::create_dir_all(src.join("nested/deeper/empty dir")).unwrap();
    fs::create_dir_all(src.join("café/日本語")).unwrap();
    fs::write(src.join("empty.txt"), b"").unwrap();
    fs::write(src.join("-leading-dash.txt"), b"dash file").unwrap();
    fs::create_dir_all(src.join("space name")).unwrap();
    fs::write(src.join("space name/my notes.txt"), b"spaces work").unwrap();
    fs::write(src.join("café/日本語/résumé.txt"), "héllo 🌍".as_bytes()).unwrap();

    let large_payload: Vec<u8> = (0..(2 * 1024 * 1024 + 123_456))
        .map(|i| ((i * 131 + 7) % 256) as u8)
        .collect();
    fs::write(src.join("nested/large.bin"), &large_payload).unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink("-leading-dash.txt", src.join("dash-link")).unwrap();

    // First plan against empty destination.
    let src_manifest = manifest::build(src, &ManifestOptions::default()).unwrap();
    let dst_manifest = manifest::build(dst, &ManifestOptions::default()).unwrap();
    let first_plan = plan::plan(&src_manifest, &dst_manifest, &PlanOptions::default());

    assert_eq!(first_plan.totals.files_to_send, 5);
    assert_eq!(first_plan.totals.files_unchanged, 0);
    assert!(!first_plan.is_empty());

    // Stream pack -> unpack concurrently over a blocking pipe, verifying stream hash matches both
    // sides.
    let (writer, reader) = byte_pipe();
    let send_names = first_plan.send.clone();
    let src_root = src.to_path_buf();
    let producer = thread::spawn(move || {
        let mut hw = HashingWriter::new(writer);
        let report = pack(&src_root, &send_names, &mut hw).unwrap();
        let (_, stream_hash, _) = hw.finish();
        (report, stream_hash)
    });

    let mut hr = HashingReader::new(reader);
    let unpack_report = unpack(dst, &mut hr).unwrap();
    // `tar::Archive` stops on the first 512-byte zero terminator block; drain the second zero
    // block written by `tar::Builder::finish` before comparing whole-stream hashes.
    io::copy(&mut hr, &mut io::sink()).unwrap();
    let (_, recv_hash, _) = hr.finish();
    let (pack_report, send_hash) = producer.join().unwrap();

    assert_eq!(send_hash, recv_hash);
    assert_eq!(pack_report.files, 5);
    assert_eq!(unpack_report.files.len(), 5);
    assert!(dst.join("nested/deeper/empty dir").is_dir());
    assert_eq!(fs::read(dst.join("empty.txt")).unwrap(), b"");
    assert_eq!(
        fs::read(dst.join("-leading-dash.txt")).unwrap(),
        b"dash file"
    );
    assert_eq!(
        fs::read(dst.join("space name/my notes.txt")).unwrap(),
        b"spaces work"
    );
    assert_eq!(
        fs::read(dst.join("café/日本語/résumé.txt")).unwrap(),
        "héllo 🌍".as_bytes()
    );
    assert_eq!(
        hash_file(&dst.join("nested/large.bin")).unwrap(),
        hash_bytes(&large_payload)
    );

    // Second plan by size+mtime sends nothing.
    let src_after = manifest::build(src, &ManifestOptions::default()).unwrap();
    let dst_after = manifest::build(dst, &ManifestOptions::default()).unwrap();
    let second_plan = plan::plan(&src_after, &dst_after, &PlanOptions::default());
    assert!(
        second_plan.is_empty(),
        "expected empty second plan, got {second_plan:?}"
    );
    assert_eq!(second_plan.totals.files_unchanged, 5);

    // Second plan by checksum also sends nothing.
    let checksum_opts = ManifestOptions {
        checksum: true,
        exclude: Vec::new(),
    };
    let src_hashed = manifest::build(src, &checksum_opts).unwrap();
    let dst_hashed = manifest::build(dst, &checksum_opts).unwrap();
    let checksum_plan = plan::plan(
        &src_hashed,
        &dst_hashed,
        &PlanOptions {
            checksum: true,
            delete: true,
            ..Default::default()
        },
    );
    assert!(
        checksum_plan.is_empty(),
        "expected empty checksum plan, got {checksum_plan:?}"
    );
    assert_eq!(checksum_plan.totals.files_unchanged, 5);
}

#[test]
fn delete_plan_removes_stale_nested_entries_cleanly() {
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();

    fs::write(src_dir.path().join("keep.txt"), b"stay").unwrap();

    // Populate destination with extra nested directories and files.
    fs::write(dst_dir.path().join("keep.txt"), b"stay").unwrap();
    fs::create_dir_all(dst_dir.path().join("stale/sub")).unwrap();
    fs::write(dst_dir.path().join("stale/sub/a.txt"), b"old").unwrap();
    fs::write(dst_dir.path().join("stale/b.txt"), b"old").unwrap();

    let src_m = manifest::build(src_dir.path(), &ManifestOptions::default()).unwrap();
    let dst_m = manifest::build(dst_dir.path(), &ManifestOptions::default()).unwrap();

    let p = plan::plan(
        &src_m,
        &dst_m,
        &PlanOptions {
            checksum: false,
            delete: true,
            ..Default::default()
        },
    );
    assert_eq!(
        p.delete,
        vec!["stale/sub/a.txt", "stale/b.txt", "stale/sub", "stale"]
    );

    // Applying the planned deletions in order removes files before their parent directories.
    for rel in &p.delete {
        let full = dst_dir.path().join(rel);
        let meta = fs::symlink_metadata(&full).unwrap();
        if meta.file_type().is_dir() {
            fs::remove_dir(&full).unwrap();
        } else {
            fs::remove_file(&full).unwrap();
        }
    }

    let dst_after = manifest::build(dst_dir.path(), &ManifestOptions::default()).unwrap();
    let after_plan = plan::plan(
        &src_m,
        &dst_after,
        &PlanOptions {
            checksum: true,
            delete: true,
            ..Default::default()
        },
    );
    assert!(after_plan.delete.is_empty());
}

#[test]
fn hostile_archives_are_rejected_without_touching_outside_paths() {
    let dst = tempfile::tempdir().unwrap();

    // Absolute path.
    let abs_tar = build_raw_tar(&[("/etc/passwd", EntryType::Regular, b"x", None)]);
    assert!(matches!(
        unpack(dst.path(), Cursor::new(abs_tar)),
        Err(Error::UnsafeArchiveEntry { .. })
    ));

    // Parent traversal.
    let dotdot_tar = build_raw_tar(&[("nested/../../escape.txt", EntryType::Regular, b"x", None)]);
    assert!(matches!(
        unpack(dst.path(), Cursor::new(dotdot_tar)),
        Err(Error::UnsafeArchiveEntry { .. })
    ));

    // Escaping hard link.
    let hardlink_tar = build_raw_tar(&[(
        "hardlink.txt",
        EntryType::Link,
        b"",
        Some("../../etc/passwd"),
    )]);
    assert!(matches!(
        unpack(dst.path(), Cursor::new(hardlink_tar)),
        Err(Error::UnsafeArchiveEntry { .. })
    ));

    // Symlink escape on Unix.
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        let out_str = outside.path().to_str().unwrap();
        let sym_tar = build_raw_tar(&[
            ("out", EntryType::Symlink, b"", Some(out_str)),
            ("out/compromised.txt", EntryType::Regular, b"x", None),
        ]);
        assert!(matches!(
            unpack(dst.path(), Cursor::new(sym_tar)),
            Err(Error::UnsafeArchiveEntry { .. })
        ));
        assert!(!outside.path().join("compromised.txt").exists());
    }
}

#[test]
fn large_file_resumes_after_interruption_and_handles_errors() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("archive.bin");
    let dst = dir.path().join("archive-copy.bin");

    let payload: Vec<u8> = (0..(1024 * 1024 + 54_321))
        .map(|i| ((i * 29 + 3) % 256) as u8)
        .collect();
    let expected_hash = hash_bytes(&payload);
    fs::write(&src, &payload).unwrap();

    // First attempt drops at 250 KiB.
    let cut1 = 250 * 1024usize;
    let err = put(
        &dst,
        payload.len() as u64,
        expected_hash,
        0,
        Cursor::new(&payload[..cut1]),
        Some(0o600),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Io { .. }));
    assert_eq!(resume_offset(&dst).unwrap(), cut1 as u64);

    // Caller tries wrong offset -> typed BadOffset reports real partial length.
    let err = put(
        &dst,
        payload.len() as u64,
        expected_hash,
        1234,
        Cursor::new(&payload[1234..]),
        Some(0o600),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::BadOffset {
            requested: 1234,
            actual,
            ..
        } if actual == cut1 as u64
    ));

    // Second attempt resumes from cut1 and drops again at 700 KiB.
    let cut2 = 700 * 1024usize;
    let err = put(
        &dst,
        payload.len() as u64,
        expected_hash,
        cut1 as u64,
        Cursor::new(&payload[cut1..cut2]),
        Some(0o600),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Io { .. }));
    assert_eq!(resume_offset(&dst).unwrap(), cut2 as u64);

    // Final attempt streams the remainder live from `get` through a blocking pipe into `put`.
    let offset = resume_offset(&dst).unwrap();
    let (writer, reader) = byte_pipe();
    let src_path = src.clone();
    let sender = thread::spawn(move || get(&src_path, offset, writer).unwrap());

    let report = put(
        &dst,
        payload.len() as u64,
        expected_hash,
        offset,
        reader,
        Some(0o600),
    )
    .unwrap();
    let total_size = sender.join().unwrap();

    assert_eq!(total_size, payload.len() as u64);
    assert_eq!(report.size, payload.len() as u64);
    assert_eq!(report.bytes_written, (payload.len() - cut2) as u64);
    assert_eq!(report.hash, expected_hash);
    assert_eq!(hash_file(&dst).unwrap(), expected_hash);
    assert_eq!(resume_offset(&dst).unwrap(), 0);
    assert!(!part_path(&dst).unwrap().exists());

    // Hash mismatch during a resumed put deletes the corrupted partial file.
    let dst_corrupt = dir.path().join("corrupt.bin");
    let _ = put(
        &dst_corrupt,
        payload.len() as u64,
        expected_hash,
        0,
        Cursor::new(&payload[..cut1]),
        None,
    );
    let mut bad_tail = payload[cut1..].to_vec();
    bad_tail[42] ^= 0xFF;
    let err = put(
        &dst_corrupt,
        payload.len() as u64,
        expected_hash,
        cut1 as u64,
        Cursor::new(&bad_tail),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, Error::HashMismatch { .. }), "{err:?}");
    assert!(!dst_corrupt.exists());
    assert!(!part_path(&dst_corrupt).unwrap().exists());
    assert_eq!(resume_offset(&dst_corrupt).unwrap(), 0);
}
