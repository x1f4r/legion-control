//! Streaming BLAKE3 hashing, wire formatting, and pass-through stream wrappers.

use std::cmp::Ordering;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result, classify_io};

/// Algorithm prefix carried by every formatted digest so future algorithms remain distinguishable.
pub const PREFIX: &str = "blake3:";

/// Fixed chunk size used for streaming file hashes to keep memory usage bounded.
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

/// A 256-bit BLAKE3 digest formatted on the wire as `blake3:<64 lowercase hex chars>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(blake3::Hash);

impl Hash {
    /// Wraps raw 32-byte digest material directly.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(blake3::Hash::from_bytes(bytes))
    }

    /// Returns the raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Returns the 64-character lowercase hexadecimal representation without the algorithm prefix.
    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

impl PartialOrd for Hash {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hash {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<[u8; 32]> for Hash {
    fn from(bytes: [u8; 32]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<blake3::Hash> for Hash {
    fn from(hash: blake3::Hash) -> Self {
        Self(hash)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", self.0.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

impl FromStr for Hash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let Some(hex) = s.strip_prefix(PREFIX) else {
            return Err(Error::InvalidHash {
                input: s.to_string(),
                reason: format!("expected '{PREFIX}' prefix"),
            });
        };
        if hex.len() != 64 {
            return Err(Error::InvalidHash {
                input: s.to_string(),
                reason: format!("expected 64 hex characters, got {}", hex.len()),
            });
        }
        let mut bytes = [0u8; 32];
        let raw = hex.as_bytes();
        for (i, slot) in bytes.iter_mut().enumerate() {
            let hi = decode_hex_nibble(raw[i * 2]).ok_or_else(|| Error::InvalidHash {
                input: s.to_string(),
                reason: "contains non-hex character".to_string(),
            })?;
            let lo = decode_hex_nibble(raw[i * 2 + 1]).ok_or_else(|| Error::InvalidHash {
                input: s.to_string(),
                reason: "contains non-hex character".to_string(),
            })?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self::from_bytes(bytes))
    }
}

fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl Serialize for Hash {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Computes the BLAKE3 digest of an in-memory slice.
pub fn hash_bytes(bytes: &[u8]) -> Hash {
    Hash(blake3::hash(bytes))
}

/// Streams `reader` to EOF in fixed chunks, returning the digest and total byte count.
pub fn hash_reader<R: Read>(reader: &mut R) -> Result<(Hash, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((Hash(hasher.finalize()), total))
}

/// Computes the BLAKE3 digest of a regular file on disk using constant memory without blocking on
/// FIFOs or special files.
pub fn hash_file(path: &Path) -> Result<Hash> {
    let (mut file, _meta) =
        legion_os::open_regular::open_regular_read(path).map_err(|err| match err {
            legion_os::open_regular::OpenRegularError::IsDirectory
            | legion_os::open_regular::OpenRegularError::NotRegularFile => Error::Conflict {
                path: path.to_path_buf(),
                reason: "source path is not a regular file".to_string(),
            },
            legion_os::open_regular::OpenRegularError::Io(io_err) => classify_io(path, io_err),
        })?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
    loop {
        let n = file.read(&mut buf).map_err(|e| classify_io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Hash(hasher.finalize()))
}

/// Pass-through [`Read`] wrapper that feeds every byte read into a running BLAKE3 digest.
///
/// Useful when a pipeline wants to verify or record a stream's hash in one pass without buffering.
#[derive(Debug)]
pub struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl<R> HashingReader<R> {
    /// Wraps `inner` with a fresh hasher state.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            bytes: 0,
        }
    }

    /// Snapshots the digest of all bytes read so far without consuming the reader.
    pub fn hash(&self) -> Hash {
        Hash(self.hasher.finalize())
    }

    /// Returns the total number of bytes read through this wrapper so far.
    pub fn bytes_read(&self) -> u64 {
        self.bytes
    }

    /// Borrows the wrapped reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrows the wrapped reader.
    ///
    /// Reading directly from the inner reference bypasses digest tracking.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Drops the hasher and returns the wrapped reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Consumes the wrapper and returns the inner reader, final digest, and byte count.
    pub fn finish(self) -> (R, Hash, u64) {
        let hash = Hash(self.hasher.finalize());
        (self.inner, hash, self.bytes)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

/// Pass-through [`Write`] wrapper that feeds every written byte into a running BLAKE3 digest.
///
/// Only the bytes actually accepted by the inner writer are hashed so short writes stay accurate.
#[derive(Debug)]
pub struct HashingWriter<W> {
    inner: W,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl<W> HashingWriter<W> {
    /// Wraps `inner` with a fresh hasher state.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            bytes: 0,
        }
    }

    /// Snapshots the digest of all bytes written so far without consuming the writer.
    pub fn hash(&self) -> Hash {
        Hash(self.hasher.finalize())
    }

    /// Returns the total number of bytes written through this wrapper so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    /// Borrows the wrapped writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Mutably borrows the wrapped writer.
    ///
    /// Writing directly to the inner reference bypasses digest tracking.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Drops the hasher and returns the wrapped writer.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Consumes the wrapper and returns the inner writer, final digest, and byte count.
    pub fn finish(self) -> (W, Hash, u64) {
        let hash = Hash(self.hasher.finalize());
        (self.inner, hash, self.bytes)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn format_parse_and_compare() {
        let empty = hash_bytes(b"");
        let expected_empty =
            "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
        assert_eq!(empty.to_string(), expected_empty);

        let parsed: Hash = expected_empty.parse().unwrap();
        assert_eq!(parsed, empty);

        // Uppercase hex parses into the same digest and formats back lowercase.
        let upper = format!("blake3:{}", empty.to_hex().to_uppercase());
        let parsed_upper: Hash = upper.parse().unwrap();
        assert_eq!(parsed_upper, empty);
        assert_eq!(parsed_upper.to_string(), expected_empty);

        let hello = hash_bytes(b"hello");
        assert_ne!(empty, hello);
    }

    #[test]
    fn rejects_malformed_hash_strings() {
        for bad in [
            "",
            "sha256:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "blake3:abcd",
            "blake3:zz1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        ] {
            let err = bad.parse::<Hash>().unwrap_err();
            assert!(
                matches!(err, Error::InvalidHash { .. }),
                "expected InvalidHash for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn serde_round_trip() {
        let h = hash_bytes(b"legion");
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{h}\""));
        let decoded: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn hash_file_streams_large_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let data: Vec<u8> = (0..(256 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let from_file = hash_file(&path).unwrap();
        assert_eq!(from_file, hash_bytes(&data));
    }

    #[test]
    fn hash_file_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let err = hash_file(&missing).unwrap_err();
        assert!(matches!(err, Error::NotFound { path, .. } if path == missing));
    }

    #[test]
    fn hashing_reader_and_writer_match_direct_hash() {
        let data: Vec<u8> = (0..100_000).map(|i| (i * 31 % 256) as u8).collect();
        let expected = hash_bytes(&data);

        let mut reader = HashingReader::new(Cursor::new(data.as_slice()));
        let mut sink = HashingWriter::new(Vec::new());
        let copied = io::copy(&mut reader, &mut sink).unwrap();

        assert_eq!(copied, data.len() as u64);
        assert_eq!(reader.bytes_read(), data.len() as u64);
        assert_eq!(sink.bytes_written(), data.len() as u64);
        assert_eq!(reader.hash(), expected);
        assert_eq!(sink.hash(), expected);

        let (_, read_hash, read_len) = reader.finish();
        let (out, write_hash, write_len) = sink.finish();
        assert_eq!(read_hash, expected);
        assert_eq!(write_hash, expected);
        assert_eq!(read_len, data.len() as u64);
        assert_eq!(write_len, data.len() as u64);
        assert_eq!(out, data);
    }

    #[test]
    fn hashing_writer_handles_short_writes() {
        struct ShortWriter {
            buf: Vec<u8>,
            max_chunk: usize,
        }

        impl Write for ShortWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let take = buf.len().min(self.max_chunk);
                self.buf.extend_from_slice(&buf[..take]);
                Ok(take)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let payload = b"short writes must only hash accepted bytes";
        let mut w = HashingWriter::new(ShortWriter {
            buf: Vec::new(),
            max_chunk: 5,
        });
        w.write_all(payload).unwrap();
        let (inner, hash, len) = w.finish();
        assert_eq!(inner.buf, payload);
        assert_eq!(len, payload.len() as u64);
        assert_eq!(hash, hash_bytes(payload));
    }
}
