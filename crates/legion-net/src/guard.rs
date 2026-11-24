//! An archive's input, checked against the plan before the unpack sees it.
//!
//! A destination agrees to take a list of entries: each name with its kind and, for a file, its
//! size, for a link its target, and with `checksum` its hash. The unpack itself only knows what a
//! tar header tells it, so this reader stands in front of it and reads every header first, the way
//! the `tar` crate will (a GNU long name, else a PAX `path`, else the ustar prefix and name; a PAX
//! `size` over the header's), and refuses an entry that was not planned, came a second time, or
//! differs from the plan, before a byte of it reaches the unpack. A file's last bytes are held
//! back until its hash has been checked, so a file that does not match is cut short in its
//! temporary place and never renamed over what the destination had.
//!
//! A refusal is kept in the shared [`Watch`] and every read after it fails, so the unpack stops at
//! the first one instead of draining the rest of the archive.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::rc::Rc;

use legion_xfer::{EntryKind, Hash, HashingWriter};

/// Tar's block.
const BLOCK: usize = 512;

/// The longest extension entry (a long name or link target, or extended header records) read.
const MOST_EXTENSION: u64 = 64 * 1024;

/// How much of a file is passed on at a time.
const CHUNK: usize = 64 * 1024;

/// What the plan says of one entry.
#[derive(Clone, Debug)]
pub(crate) struct Expected {
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub hash: Option<Hash>,
    pub target: Option<String>,
}

/// What the reader and the unpack's filter share.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    plan: HashMap<String, Expected>,
    seen: HashSet<String>,
    /// The entry whose header passed last, which the unpack is about to act on.
    pub current: Option<(String, EntryKind)>,
    /// The first refusal, as the entry's name and why.
    pub refused: Option<(String, String)>,
}

impl Watch {
    pub(crate) fn new(plan: HashMap<String, Expected>) -> Watch {
        Watch {
            plan,
            ..Watch::default()
        }
    }

    /// Records a refusal, keeping the first, and the error that stops the unpack.
    pub(crate) fn refuse(&mut self, name: &str, reason: String) -> io::Error {
        let error = io::Error::other(format!("{name} {reason}"));
        if self.refused.is_none() {
            self.refused = Some((name.to_owned(), reason));
        }
        error
    }

    /// The planned names, for checking afterwards that each came.
    pub(crate) fn planned(&self) -> impl Iterator<Item = &String> {
        self.plan.keys()
    }

    /// What the plan says of one name.
    pub(crate) fn expected(&self, name: &str) -> Option<&Expected> {
        self.plan.get(name)
    }

    /// Checks a real entry's header against the plan, and makes it the current one.
    fn admit(
        &mut self,
        path: &[u8],
        kind: EntryKind,
        size: u64,
        target: Option<&[u8]>,
    ) -> io::Result<Option<Hash>> {
        let shown = String::from_utf8_lossy(path).into_owned();
        let name = std::str::from_utf8(path)
            .ok()
            .and_then(|name| legion_xfer::validate_relative_path(name).ok());
        let Some(name) = name else {
            return Err(self.refuse(&shown, "is not a name a transfer sends".to_owned()));
        };
        let Some(expected) = self.plan.get(&name).cloned() else {
            return Err(self.refuse(
                &name,
                "was not among the entries planned for this transfer".to_owned(),
            ));
        };
        if !self.seen.insert(name.clone()) {
            return Err(self.refuse(&name, "came a second time".to_owned()));
        }
        if expected.kind != kind {
            return Err(self.refuse(
                &name,
                format!(
                    "was planned as a {} and came as a {}",
                    label(expected.kind),
                    label(kind)
                ),
            ));
        }
        match kind {
            EntryKind::File if expected.size != Some(size) => {
                return Err(self.refuse(
                    &name,
                    format!(
                        "was planned with {} bytes and came with {size}",
                        expected.size.unwrap_or_default()
                    ),
                ));
            }
            EntryKind::File => {}
            _ if size != 0 => {
                return Err(self.refuse(&name, format!("came with {size} bytes of content")));
            }
            EntryKind::Symlink => {
                let came = target.map(String::from_utf8_lossy).unwrap_or_default();
                if expected.target.as_deref() != Some(came.as_ref()) {
                    return Err(self.refuse(
                        &name,
                        format!(
                            "was planned as a link to {} and came as a link to {came}",
                            expected.target.as_deref().unwrap_or("nothing")
                        ),
                    ));
                }
            }
            EntryKind::Dir => {}
        }
        self.current = Some((name, kind));
        Ok(expected.hash)
    }
}

fn label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "directory",
        EntryKind::Symlink => "symbolic link",
    }
}

/// Where the reader is in the archive.
enum State {
    /// At a header.
    Header,
    /// In an extension entry of this type, which holds `size` bytes in `padded`.
    Extension { kind: u8, size: u64, padded: u64 },
    /// In a file's content: what is left of it, the padding after it, and the hash it must have.
    Data {
        left: u64,
        pad: u64,
        hashing: Option<Box<Hashing>>,
    },
    /// In the padding after a file's content.
    Pad(u64),
    /// Past the block that ends the archive, which the unpack no longer reads as entries.
    Ended,
}

/// A file's content being hashed as it passes: the hash it must come to, and the file's name.
struct Hashing {
    hasher: HashingWriter<io::Sink>,
    expected: Hash,
    name: String,
}

/// The extensions that describe the next real entry.
#[derive(Default)]
struct Pending {
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
    pax: Option<Vec<u8>>,
}

/// The archive's input, passed on only as far as it was checked.
pub(crate) struct Guarded<R> {
    inner: R,
    watch: Rc<RefCell<Watch>>,
    state: State,
    pending: Pending,
    out: Vec<u8>,
    at: usize,
}

impl<R: Read> Guarded<R> {
    pub(crate) fn new(inner: R, watch: Rc<RefCell<Watch>>) -> Guarded<R> {
        Guarded {
            inner,
            watch,
            state: State::Header,
            pending: Pending::default(),
            out: Vec::new(),
            at: 0,
        }
    }

    /// Reads the next part of the archive into `out`, checked; `false` at its end.
    fn step(&mut self) -> io::Result<bool> {
        match std::mem::replace(&mut self.state, State::Header) {
            State::Header => self.header(),
            State::Extension { kind, size, padded } => {
                let mut data = vec![0u8; usize::try_from(padded).unwrap_or(usize::MAX)];
                self.inner.read_exact(&mut data)?;
                let body = data[..usize::try_from(size).unwrap_or(usize::MAX)].to_vec();
                let slot = match kind {
                    b'L' => &mut self.pending.long_name,
                    b'K' => &mut self.pending.long_link,
                    _ => &mut self.pending.pax,
                };
                if slot.replace(body).is_some() {
                    return Err(self
                        .watch
                        .borrow_mut()
                        .refuse("the archive", "describes one entry twice".to_owned()));
                }
                self.out = data;
                Ok(true)
            }
            State::Data { left, pad, hashing } => {
                let want = usize::try_from(left).unwrap_or(CHUNK).min(CHUNK);
                let mut chunk = vec![0u8; want];
                let read = self.inner.read(&mut chunk)?;
                if read == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                chunk.truncate(read);
                let left = left - read as u64;
                let hashing = match hashing {
                    Some(mut hashing) => {
                        hashing.hasher.write_all(&chunk)?;
                        if left == 0 && hashing.hasher.hash() != hashing.expected {
                            // Withheld, these last bytes leave the file short, so the unpack
                            // drops it where it was being written.
                            return Err(self.watch.borrow_mut().refuse(
                                &hashing.name,
                                "is not what the source listed: its hash differs".to_owned(),
                            ));
                        }
                        Some(hashing)
                    }
                    None => None,
                };
                self.state = if left > 0 {
                    State::Data { left, pad, hashing }
                } else if pad > 0 {
                    State::Pad(pad)
                } else {
                    State::Header
                };
                self.out = chunk;
                Ok(true)
            }
            State::Pad(pad) => {
                let mut data = vec![0u8; usize::try_from(pad).unwrap_or(BLOCK)];
                self.inner.read_exact(&mut data)?;
                self.out = data;
                Ok(true)
            }
            State::Ended => {
                self.state = State::Ended;
                let mut chunk = vec![0u8; CHUNK];
                let read = self.inner.read(&mut chunk)?;
                chunk.truncate(read);
                self.out = chunk;
                Ok(read > 0)
            }
        }
    }

    fn header(&mut self) -> io::Result<bool> {
        let mut block = vec![0u8; BLOCK];
        let mut filled = 0;
        while filled < BLOCK {
            let read = self.inner.read(&mut block[filled..])?;
            if read == 0 {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            filled += read;
        }
        if block.iter().all(|byte| *byte == 0) {
            self.state = State::Ended;
            self.out = block;
            return Ok(true);
        }
        let recognised = (&block[257..263] == b"ustar\0" && &block[263..265] == b"00")
            || (&block[257..263] == b"ustar " && &block[263..265] == b" \0");
        let size = number(&block[124..136]);
        let typeflag = block[156];
        let mut watch = self.watch.borrow_mut();
        match typeflag {
            b'L' | b'K' | b'x' => {
                let Some(size) = size.filter(|size| recognised && *size <= MOST_EXTENSION) else {
                    return Err(watch.refuse(
                        "the archive",
                        "carries an extended header a transfer never sends".to_owned(),
                    ));
                };
                self.state = State::Extension {
                    kind: typeflag,
                    size,
                    padded: size.div_ceil(BLOCK as u64) * BLOCK as u64,
                };
            }
            b'0' | 0 | b'7' | b'5' | b'2' => {
                let kind = match typeflag {
                    b'5' => EntryKind::Dir,
                    b'2' => EntryKind::Symlink,
                    _ => EntryKind::File,
                };
                let pending = std::mem::take(&mut self.pending);
                let pax = match pending.pax.as_deref().map(records) {
                    None => HashMap::new(),
                    Some(Some(pax)) => pax,
                    Some(None) => {
                        return Err(watch.refuse(
                            "the archive",
                            "has extended header records it cannot read".to_owned(),
                        ));
                    }
                };
                let path = match pending.long_name {
                    Some(name) => strip_nul(name),
                    None => match pax.get("path") {
                        Some(path) => path.clone(),
                        None => header_path(&block),
                    },
                };
                let target = match pending.long_link {
                    Some(target) => Some(strip_nul(target)),
                    None => pax
                        .get("linkpath")
                        .cloned()
                        .or_else(|| (block[157] != 0).then(|| truncated(&block[157..257]))),
                };
                let size = match pax.get("size") {
                    Some(size) => std::str::from_utf8(size)
                        .ok()
                        .and_then(|size| size.parse().ok()),
                    None => size,
                };
                let Some(size) = size else {
                    return Err(watch.refuse("the archive", "has a size it cannot read".to_owned()));
                };
                let expected = watch.admit(&path, kind, size, target.as_deref())?;
                let name = watch.current.as_ref().map(|(name, _)| name.clone());
                drop(watch);
                if kind == EntryKind::File {
                    let hashing = expected.zip(name).map(|(expected, name)| {
                        Box::new(Hashing {
                            hasher: HashingWriter::new(io::sink()),
                            expected,
                            name,
                        })
                    });
                    if size == 0 {
                        if let Some(hashing) = hashing
                            && hashing.hasher.hash() != hashing.expected
                        {
                            return Err(self.watch.borrow_mut().refuse(
                                &hashing.name,
                                "is not what the source listed: its hash differs".to_owned(),
                            ));
                        }
                    } else {
                        self.state = State::Data {
                            left: size,
                            pad: (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64,
                            hashing,
                        };
                    }
                }
            }
            other => {
                return Err(watch.refuse(
                    "the archive",
                    format!(
                        "carries an entry of type {:?}, which a transfer never sends",
                        char::from(other)
                    ),
                ));
            }
        }
        self.out = block;
        Ok(true)
    }
}

impl<R: Read> Read for Guarded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some((name, reason)) = &self.watch.borrow().refused {
                return Err(io::Error::other(format!("{name} {reason}")));
            }
            if self.at < self.out.len() {
                let n = buf.len().min(self.out.len() - self.at);
                buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
                self.at += n;
                return Ok(n);
            }
            self.out.clear();
            self.at = 0;
            if !self.step()? {
                return Ok(0);
            }
        }
    }
}

/// A header's numeric field: octal text, or big-endian binary when its first bit is set.
fn number(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        let mut value = u64::from(field[0] & 0x7f);
        for byte in &field[1..] {
            value = value.checked_mul(256)? | u64::from(*byte);
        }
        return Some(value);
    }
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c| c == ' ' || c == '\0');
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

/// A header's own name: the ustar prefix and name, or the name alone, each up to its first NUL.
fn header_path(block: &[u8]) -> Vec<u8> {
    let name = truncated(&block[0..100]);
    let ustar = &block[257..263] == b"ustar\0" && &block[263..265] == b"00";
    let prefix = truncated(&block[345..500]);
    if ustar && !prefix.is_empty() {
        let mut path = prefix;
        path.push(b'/');
        path.extend_from_slice(&name);
        path
    } else {
        name
    }
}

fn truncated(field: &[u8]) -> Vec<u8> {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    field[..end].to_vec()
}

/// A long name or link as the `tar` crate reads it: one trailing NUL dropped.
fn strip_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

/// Extended header records (`<length> <key>=<value>\n`), which must fill the data exactly and
/// name each key once: records read any other way could be read differently by the unpack.
fn records(data: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    let mut found = HashMap::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|b| *b == b' ')?;
        let length: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        if length <= space || length > rest.len() {
            return None;
        }
        let record = rest[space + 1..length].strip_suffix(b"\n")?;
        let equals = record.iter().position(|b| *b == b'=')?;
        let key = std::str::from_utf8(&record[..equals]).ok()?.to_owned();
        if found.insert(key, record[equals + 1..].to_vec()).is_some() {
            return None;
        }
        rest = &rest[length..];
    }
    Some(found)
}
