//! Compact, versioned binary codec for durable index records and auxiliary tables.
//!
//! Format version 2 adds `(member, incarnation)` [`VersionKey`] entries, short content hash
//! ancestry (`recent_hashes`), `from_join`, and local `conflict_dismissed` flags while remaining
//! bounds-checked and zero-`unsafe`.

use std::collections::BTreeMap;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::{Result, SyncError};
use crate::model::IndexRecord;
use crate::version::{VersionKey, VersionVector};

/// Format version prefix stored at byte 0 of every encoded [`IndexRecord`].
const RECORD_CODEC_VERSION: u8 = 2;

const FLAG_DELETED: u8 = 1 << 0;
const FLAG_HAS_DISK_MTIME: u8 = 1 << 1;
const FLAG_HAS_MODE: u8 = 1 << 2;
const FLAG_HAS_HASH: u8 = 1 << 3;
const FLAG_HAS_LINK_TARGET: u8 = 1 << 4;
const FLAG_HAS_CONFLICT_OF: u8 = 1 << 5;
const FLAG_HAS_CHANGE_ID: u8 = 1 << 6;
const FLAG_FROM_JOIN: u8 = 1 << 7;

const FLAG2_CONFLICT_DISMISSED: u8 = 1 << 0;

/// Encodes `record` into a compact self-contained byte vector.
pub fn encode_record(record: &IndexRecord) -> Vec<u8> {
    let mut buf = Vec::with_capacity(112 + record.path.len() + record.recent_hashes.len() * 32);
    buf.push(RECORD_CODEC_VERSION);
    buf.push(match record.kind {
        EntryKind::File => 0,
        EntryKind::Dir => 1,
        EntryKind::Symlink => 2,
    });

    let mut flags = 0u8;
    if record.deleted {
        flags |= FLAG_DELETED;
    }
    if record.disk_mtime.is_some() {
        flags |= FLAG_HAS_DISK_MTIME;
    }
    if record.mode.is_some() {
        flags |= FLAG_HAS_MODE;
    }
    if record.hash.is_some() {
        flags |= FLAG_HAS_HASH;
    }
    if record.link_target.is_some() {
        flags |= FLAG_HAS_LINK_TARGET;
    }
    if record.conflict_of.is_some() {
        flags |= FLAG_HAS_CONFLICT_OF;
    }
    if record.disk_change_id.is_some() {
        flags |= FLAG_HAS_CHANGE_ID;
    }
    if record.from_join {
        flags |= FLAG_FROM_JOIN;
    }
    buf.push(flags);

    let mut flags2 = 0u8;
    if record.conflict_dismissed {
        flags2 |= FLAG2_CONFLICT_DISMISSED;
    }
    buf.push(flags2);

    write_str(&mut buf, &record.path);
    buf.extend_from_slice(&record.size.to_le_bytes());
    buf.extend_from_slice(&record.mtime.secs.to_le_bytes());
    buf.extend_from_slice(&record.mtime.nanos.to_le_bytes());
    if let Some(dm) = record.disk_mtime {
        buf.extend_from_slice(&dm.secs.to_le_bytes());
        buf.extend_from_slice(&dm.nanos.to_le_bytes());
    }
    if let Some(cid) = record.disk_change_id {
        buf.extend_from_slice(&cid.to_le_bytes());
    }
    if let Some(mode) = record.mode {
        buf.extend_from_slice(&mode.to_le_bytes());
    }
    if let Some(hash) = record.hash {
        buf.extend_from_slice(hash.as_bytes());
    }
    if let Some(ref target) = record.link_target {
        write_str(&mut buf, target);
    }
    if let Some(ref conflict_of) = record.conflict_of {
        write_str(&mut buf, conflict_of);
    }
    write_str(&mut buf, &record.modified_by);
    buf.extend_from_slice(&record.seq.to_le_bytes());

    write_u32(&mut buf, record.version.len());
    for (key, counter) in record.version.iter() {
        write_str(&mut buf, &key.member);
        buf.extend_from_slice(&key.incarnation.to_le_bytes());
        buf.extend_from_slice(&counter.to_le_bytes());
    }

    write_u32(&mut buf, record.recent_hashes.len());
    for hash in &record.recent_hashes {
        buf.extend_from_slice(hash.as_bytes());
    }

    buf
}

/// Decodes an [`IndexRecord`] previously produced by [`encode_record`].
pub fn decode_record(bytes: &[u8]) -> Result<IndexRecord> {
    let mut cur = Cursor::new(bytes, "index record");
    let version_byte = cur.read_u8()?;
    if version_byte != RECORD_CODEC_VERSION {
        return Err(SyncError::CorruptData {
            context: "index record".to_string(),
            detail: format!("unsupported record format version {version_byte}"),
        });
    }

    let kind = match cur.read_u8()? {
        0 => EntryKind::File,
        1 => EntryKind::Dir,
        2 => EntryKind::Symlink,
        other => {
            return Err(SyncError::CorruptData {
                context: "index record".to_string(),
                detail: format!("unknown entry kind tag {other}"),
            });
        }
    };

    let flags = cur.read_u8()?;
    let flags2 = cur.read_u8()?;
    let deleted = (flags & FLAG_DELETED) != 0;
    let from_join = (flags & FLAG_FROM_JOIN) != 0;
    let conflict_dismissed = (flags2 & FLAG2_CONFLICT_DISMISSED) != 0;

    let path = cur.read_string()?;
    let size = cur.read_u64()?;
    let mtime = Timestamp::new(cur.read_i64()?, cur.read_u32()?);
    let disk_mtime = if (flags & FLAG_HAS_DISK_MTIME) != 0 {
        Some(Timestamp::new(cur.read_i64()?, cur.read_u32()?))
    } else {
        None
    };
    let disk_change_id = if (flags & FLAG_HAS_CHANGE_ID) != 0 {
        Some(cur.read_u64()?)
    } else {
        None
    };
    let mode = if (flags & FLAG_HAS_MODE) != 0 {
        Some(cur.read_u32()?)
    } else {
        None
    };
    let hash = if (flags & FLAG_HAS_HASH) != 0 {
        Some(Hash::from_bytes(cur.read_array::<32>()?))
    } else {
        None
    };
    let link_target = if (flags & FLAG_HAS_LINK_TARGET) != 0 {
        Some(cur.read_string()?)
    } else {
        None
    };
    let conflict_of = if (flags & FLAG_HAS_CONFLICT_OF) != 0 {
        Some(cur.read_string()?)
    } else {
        None
    };
    let modified_by = cur.read_string()?;
    let seq = cur.read_u64()?;

    let vv_len = cur.read_u32()? as usize;
    let mut vv_map = BTreeMap::new();
    for _ in 0..vv_len {
        let member = cur.read_string()?;
        let incarnation = cur.read_u64()?;
        let counter = cur.read_u64()?;
        vv_map.insert(VersionKey::new(member, incarnation), counter);
    }

    let hist_len = cur.read_u32()? as usize;
    let mut recent_hashes = Vec::with_capacity(hist_len.min(32));
    for _ in 0..hist_len {
        recent_hashes.push(Hash::from_bytes(cur.read_array::<32>()?));
    }

    if !cur.is_empty() {
        return Err(SyncError::CorruptData {
            context: "index record".to_string(),
            detail: format!("{} trailing bytes in encoded record", cur.remaining()),
        });
    }

    Ok(IndexRecord {
        path,
        kind,
        size,
        mtime,
        disk_mtime,
        disk_change_id,
        mode,
        hash,
        link_target,
        deleted,
        version: VersionVector::from_map(vv_map),
        modified_by,
        seq,
        recent_hashes,
        from_join,
        conflict_of,
        conflict_dismissed,
    })
}

/// Encodes a list of exact paths stored under a case-folded key.
pub fn encode_string_list(items: &[String]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    write_u32(&mut buf, items.len());
    for item in items {
        write_str(&mut buf, item);
    }
    buf
}

/// Decodes a list of exact paths stored under a case-folded key.
pub fn decode_string_list(bytes: &[u8]) -> Result<Vec<String>> {
    let mut cur = Cursor::new(bytes, "case map entry");
    let count = cur.read_u32()? as usize;
    let mut out = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        out.push(cur.read_string()?);
    }
    if !cur.is_empty() {
        return Err(SyncError::CorruptData {
            context: "case map entry".to_string(),
            detail: "trailing bytes in string list".to_string(),
        });
    }
    Ok(out)
}

/// Encodes a list of BLAKE3 hashes for deleted-file history storage.
pub fn encode_hash_list(hashes: &[Hash]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + hashes.len() * 32);
    write_u32(&mut buf, hashes.len());
    for h in hashes {
        buf.extend_from_slice(h.as_bytes());
    }
    buf
}

/// Decodes a list of BLAKE3 hashes from deleted-file history storage.
pub fn decode_hash_list(bytes: &[u8]) -> Result<Vec<Hash>> {
    let mut cur = Cursor::new(bytes, "deleted history entry");
    let count = cur.read_u32()? as usize;
    let mut out = Vec::with_capacity(count.min(32));
    for _ in 0..count {
        out.push(Hash::from_bytes(cur.read_array::<32>()?));
    }
    if !cur.is_empty() {
        return Err(SyncError::CorruptData {
            context: "deleted history entry".to_string(),
            detail: "trailing bytes in hash list".to_string(),
        });
    }
    Ok(out)
}

fn write_u32(buf: &mut Vec<u8>, val: usize) {
    let clamped = u32::try_from(val).unwrap_or(u32::MAX);
    buf.extend_from_slice(&clamped.to_le_bytes());
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_u32(buf, s.len());
    buf.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    data: &'a [u8],
    context: &'static str,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], context: &'static str) -> Self {
        Self { data, context }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn remaining(&self) -> usize {
        self.data.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.data.len() < n {
            return Err(SyncError::CorruptData {
                context: self.context.to_string(),
                detail: format!("unexpected EOF: needed {n} bytes, have {}", self.data.len()),
            });
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|err| SyncError::CorruptData {
                context: self.context.to_string(),
                detail: format!("invalid UTF-8 string: {err}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use legion_xfer::hash::hash_bytes;

    use super::*;

    #[test]
    fn record_binary_round_trip() {
        let mut vv = VersionVector::new();
        vv.set(VersionKey::new("tower", 0x1111), 7);
        vv.set(VersionKey::new("pi", 0x2222), 3);

        let rec = IndexRecord {
            path: "docs/notes.txt".to_string(),
            kind: EntryKind::File,
            size: 1234,
            mtime: Timestamp::new(1_795_608_000, 987_654_321),
            disk_mtime: Some(Timestamp::new(1_795_608_000, 0)),
            disk_change_id: Some(0x1234_5678_9abc_def0),
            mode: Some(0o644),
            hash: Some(hash_bytes(b"hello world")),
            link_target: None,
            deleted: false,
            version: vv,
            modified_by: "tower".to_string(),
            seq: 99,
            recent_hashes: vec![hash_bytes(b"v0"), hash_bytes(b"v1")],
            from_join: true,
            conflict_of: Some("docs/orig.txt".to_string()),
            conflict_dismissed: true,
        };

        let encoded = encode_record(&rec);
        let decoded = decode_record(&encoded).unwrap();
        assert_eq!(decoded, rec);
    }

    #[test]
    fn rejects_truncated_or_bad_version_record() {
        assert!(decode_record(&[]).is_err());
        assert!(decode_record(&[99, 0, 0, 0]).is_err());
        assert!(decode_record(&[2, 0, 0, 0, 10, 0, 0, 0]).is_err());
    }
}
