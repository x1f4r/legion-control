//! Ticket table for content addresses.
//!
//! Each ticket carries 256 random bits from the operating system CSPRNG, is stored hashed with
//! BLAKE3 so raw ticket tokens do not sit in map keys, and is bound to one holder (bearer token or
//! device pairing), principal, machine, and path. Tickets expire 15 minutes after their last use
//! and 6 hours after creation at most, are capped per holder (evicting the holder's least recently
//! used ticket) and globally, and can be revoked by holder.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use legion_proto::{ErrorBody, ErrorCode};

/// Default sliding lifetime: 15 minutes after last use.
pub const SLIDING_TTL: Duration = Duration::from_secs(15 * 60);

/// Default absolute lifetime: 6 hours from creation regardless of activity.
pub const ABSOLUTE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Default maximum live tickets held by one holder at once.
pub const DEFAULT_MAX_PER_HOLDER: usize = 128;

/// Default maximum live tickets held by the table across all holders.
pub const DEFAULT_MAX_TOTAL: usize = 4096;

/// Limits and lifetimes for a [`TicketTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TicketConfig {
    /// Sliding expiration measured from the ticket's most recent use.
    pub sliding_ttl: Duration,
    /// Hard absolute expiration measured from ticket creation.
    pub absolute_ttl: Duration,
    /// Maximum live tickets per holder; issuing beyond this evicts the holder's least recently
    /// used ticket.
    pub max_per_holder: usize,
    /// Maximum total live tickets across all holders.
    pub max_total: usize,
}

impl Default for TicketConfig {
    fn default() -> Self {
        Self {
            sliding_ttl: SLIDING_TTL,
            absolute_ttl: ABSOLUTE_TTL,
            max_per_holder: DEFAULT_MAX_PER_HOLDER,
            max_total: DEFAULT_MAX_TOTAL,
        }
    }
}

/// Parameters required to issue a new content ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketRequest {
    /// Opaque holder key (such as `<token_name>:<token_hash>` or a device pairing id) used for
    /// per-holder bounds and revocation when the credential disappears.
    pub holder: String,
    /// Principal the ticket acts as on every read.
    pub principal: String,
    /// Target machine name that owns the file.
    pub machine: String,
    /// Absolute (or `~`-prefixed) file path on `machine`.
    pub path: String,
}

/// Validated ticket metadata returned on lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    /// Credential holder that owns this ticket.
    pub holder: String,
    /// Principal authorised to read the file.
    pub principal: String,
    /// Target machine name that owns the file.
    pub machine: String,
    /// File path on `machine`.
    pub path: String,
    /// Basename derived from `path` at ticket creation time.
    pub file_name: String,
}

/// Result of issuing a ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedTicket {
    /// URL-safe 256-bit random ticket identifier (43 base64url characters).
    pub id: String,
    /// Basename derived from `path`, ready to encode into the URL's final segment.
    pub file_name: String,
    /// Seconds until the ticket expires if unused (the sliding lifetime, capped by absolute).
    pub expires_in: u64,
}

struct Entry {
    ticket: Ticket,
    created: Instant,
    last_used: Instant,
    /// Monotonic sequence bumped on create and every use so LRU eviction breaks ties cleanly even
    /// when two operations happen within the same clock tick.
    use_seq: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<[u8; 32], Entry>,
    seq: u64,
}

/// Concurrent in-memory table of content tickets.
pub struct TicketTable {
    config: TicketConfig,
    inner: Mutex<Inner>,
}

impl Default for TicketTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketTable {
    /// Creates a table with the default lifetimes and bounds.
    pub fn new() -> Self {
        Self::with_config(TicketConfig::default())
    }

    /// Creates a table with explicit lifetimes and bounds.
    pub fn with_config(config: TicketConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Issues a new 256-bit random ticket for `req`.
    pub fn issue(&self, req: TicketRequest) -> Result<IssuedTicket, ErrorBody> {
        self.issue_at(req, Instant::now())
    }

    /// Issues a new 256-bit random ticket using an explicit `now` timestamp (for tests).
    pub fn issue_at(&self, req: TicketRequest, now: Instant) -> Result<IssuedTicket, ErrorBody> {
        let file_name = extract_file_name(&req.path)?;
        if self.config.max_per_holder == 0 || self.config.max_total == 0 {
            return Err(ErrorBody::new(
                ErrorCode::Busy,
                "This server does not accept content tickets right now.",
            ));
        }

        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not generate random bytes for a content ticket: {err}."),
            )
        })?;
        let id = URL_SAFE_NO_PAD.encode(random);
        let key = ticket_key(&id);

        let mut inner = self.lock();
        self.prune_expired_locked(&mut inner, now);

        // Enforce per-holder cap by evicting the holder's least-recently-used ticket(s).
        let mut own: Vec<([u8; 32], Instant, u64)> = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.ticket.holder == req.holder)
            .map(|(k, entry)| (*k, entry.last_used, entry.use_seq))
            .collect();
        let surplus = (own.len() + 1).saturating_sub(self.config.max_per_holder);
        if surplus > 0 {
            own.sort_by_key(|&(_, last_used, seq)| (last_used, seq));
            for (evict_key, _, _) in own.into_iter().take(surplus) {
                inner.entries.remove(&evict_key);
            }
        }

        if inner.entries.len() >= self.config.max_total {
            return Err(ErrorBody::new(
                ErrorCode::Busy,
                "The server holds as many content tickets as it allows.",
            )
            .with_hint("Wait for idle tickets to expire and try again."));
        }

        inner.seq = inner.seq.wrapping_add(1);
        let seq = inner.seq;
        inner.entries.insert(
            key,
            Entry {
                ticket: Ticket {
                    holder: req.holder,
                    principal: req.principal,
                    machine: req.machine,
                    path: req.path,
                    file_name: file_name.clone(),
                },
                created: now,
                last_used: now,
                use_seq: seq,
            },
        );
        let expires_in = self
            .config
            .sliding_ttl
            .min(self.config.absolute_ttl)
            .as_secs();
        Ok(IssuedTicket {
            id,
            file_name,
            expires_in,
        })
    }

    /// Looks up `id`, refreshing its sliding expiry on hit and returning `None` if unknown or
    /// expired.
    pub fn lookup(&self, id: &str) -> Option<Ticket> {
        self.lookup_at(id, Instant::now())
    }

    /// Looks up `id` at an explicit `now` timestamp (for tests).
    pub fn lookup_at(&self, id: &str, now: Instant) -> Option<Ticket> {
        // Every issued ticket is 43 base64url chars (256 bits). Reject obviously malformed strings
        // without hashing or locking.
        if id.len() != 43
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        let key = ticket_key(id);
        let mut inner = self.lock();
        let expired = inner
            .entries
            .get(&key)
            .map(|entry| self.is_expired(entry, now))?;
        if expired {
            inner.entries.remove(&key);
            return None;
        }
        inner.seq = inner.seq.wrapping_add(1);
        let seq = inner.seq;
        let entry = inner.entries.get_mut(&key)?;
        entry.last_used = now;
        entry.use_seq = seq;
        Some(entry.ticket.clone())
    }

    /// Removes one ticket by id, returning `true` if it existed.
    pub fn revoke(&self, id: &str) -> bool {
        self.lock().entries.remove(&ticket_key(id)).is_some()
    }

    /// Removes every ticket owned by `holder`, returning how many were removed.
    pub fn revoke_holder(&self, holder: &str) -> usize {
        let mut inner = self.lock();
        let before = inner.entries.len();
        inner
            .entries
            .retain(|_, entry| entry.ticket.holder != holder);
        before - inner.entries.len()
    }

    /// Sweeps expired tickets and any ticket whose holder fails `keep`, returning how many were
    /// removed.
    pub fn retain_holders(&self, keep: impl FnMut(&str) -> bool) -> usize {
        self.retain_holders_at(Instant::now(), keep)
    }

    /// Like [`Self::retain_holders`] at an explicit `now` timestamp (for tests).
    pub fn retain_holders_at(&self, now: Instant, mut keep: impl FnMut(&str) -> bool) -> usize {
        let mut inner = self.lock();
        let before = inner.entries.len();
        inner
            .entries
            .retain(|_, entry| !self.is_expired(entry, now) && keep(&entry.ticket.holder));
        before - inner.entries.len()
    }

    /// Total number of tickets currently stored (including any not yet swept).
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether no tickets are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of tickets currently stored for `holder`.
    pub fn len_for_holder(&self, holder: &str) -> usize {
        self.lock()
            .entries
            .values()
            .filter(|entry| entry.ticket.holder == holder)
            .count()
    }

    fn is_expired(&self, entry: &Entry, now: Instant) -> bool {
        now.saturating_duration_since(entry.last_used) >= self.config.sliding_ttl
            || now.saturating_duration_since(entry.created) >= self.config.absolute_ttl
    }

    fn prune_expired_locked(&self, inner: &mut Inner, now: Instant) {
        inner
            .entries
            .retain(|_, entry| !self.is_expired(entry, now));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn ticket_key(id: &str) -> [u8; 32] {
    *blake3::hash(id.as_bytes()).as_bytes()
}

/// Extracts the file name from a Unix or Windows path, refusing empty, trailing-separator, or dot
/// names.
pub fn extract_file_name(path: &str) -> Result<String, ErrorBody> {
    if path.is_empty() || path.ends_with('/') || path.ends_with('\\') {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Path must name a file, not a directory.",
        )
        .with_hint("Pass the full path to a regular file."));
    }
    let name = path.rsplit(['/', '\\']).next().unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.chars().any(char::is_control) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Path does not end in a valid file name.",
        )
        .with_hint("Pass the full path to a regular file."));
    }
    Ok(name.to_owned())
}

/// Percent-encodes a file name for safe inclusion as a single URL path segment.
pub fn encode_path_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0F) as usize] as char);
        }
    }
    out
}

/// Percent-decodes a single URL path segment into UTF-8, returning `None` on malformed escapes or
/// invalid UTF-8.
pub fn decode_path_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_nibble(bytes[i + 1])?;
            let lo = hex_nibble(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(holder: &str, path: &str) -> TicketRequest {
        TicketRequest {
            holder: holder.to_owned(),
            principal: "phone".to_owned(),
            machine: "tower".to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn tickets_have_256_random_bits_and_obey_sliding_and_absolute_ttls() {
        let table = TicketTable::new();
        let t0 = Instant::now();
        let issued = table.issue_at(req("h1", "/home/me/video.mp4"), t0).unwrap();
        assert_eq!(issued.id.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(&issued.id).unwrap().len(), 32);
        assert_eq!(issued.file_name, "video.mp4");
        assert_eq!(issued.expires_in, SLIDING_TTL.as_secs());

        // Use at 14 minutes extends sliding lifetime.
        let t14 = t0 + Duration::from_secs(14 * 60);
        let found = table.lookup_at(&issued.id, t14).unwrap();
        assert_eq!(found.path, "/home/me/video.mp4");
        assert_eq!(found.machine, "tower");

        // Still valid at 28 minutes (14 minutes after t14).
        let t28 = t14 + Duration::from_secs(14 * 60);
        assert!(table.lookup_at(&issued.id, t28).is_some());

        // Idle for 15 minutes expires it.
        let t_idle = t28 + SLIDING_TTL;
        assert!(table.lookup_at(&issued.id, t_idle).is_none());

        // Absolute 6-hour cap expires a continually used ticket.
        let issued2 = table.issue_at(req("h1", "/home/me/a.png"), t0).unwrap();
        let mut cursor = t0;
        for _ in 0..35 {
            cursor += Duration::from_secs(10 * 60);
            assert!(table.lookup_at(&issued2.id, cursor).is_some());
        }
        assert!(table.lookup_at(&issued2.id, t0 + ABSOLUTE_TTL).is_none());
    }

    #[test]
    fn holder_bound_evicts_lru_and_revocation_clears_only_that_holder() {
        let table = TicketTable::with_config(TicketConfig {
            max_per_holder: 3,
            max_total: 10,
            ..TicketConfig::default()
        });
        let t0 = Instant::now();
        let a = table.issue_at(req("h1", "/a.txt"), t0).unwrap();
        let b = table.issue_at(req("h1", "/b.txt"), t0).unwrap();
        let c = table.issue_at(req("h1", "/c.txt"), t0).unwrap();
        let other = table.issue_at(req("h2", "/other.txt"), t0).unwrap();

        // Touch `a` so `b` becomes least recently used.
        assert!(table.lookup_at(&a.id, t0).is_some());
        let d = table.issue_at(req("h1", "/d.txt"), t0).unwrap();

        assert_eq!(table.len_for_holder("h1"), 3);
        assert!(
            table.lookup_at(&b.id, t0).is_none(),
            "LRU ticket b must be evicted"
        );
        assert!(table.lookup_at(&a.id, t0).is_some());
        assert!(table.lookup_at(&c.id, t0).is_some());
        assert!(table.lookup_at(&d.id, t0).is_some());

        // Revoking h1 removes h1's tickets and leaves h2 untouched.
        assert_eq!(table.revoke_holder("h1"), 3);
        assert_eq!(table.len_for_holder("h1"), 0);
        assert!(table.lookup_at(&a.id, t0).is_none());
        assert!(table.lookup_at(&other.id, t0).is_some());
    }

    #[test]
    fn global_bound_returns_busy_when_other_holders_fill_table() {
        let table = TicketTable::with_config(TicketConfig {
            max_per_holder: 2,
            max_total: 2,
            ..TicketConfig::default()
        });
        let t0 = Instant::now();
        table.issue_at(req("h1", "/a.txt"), t0).unwrap();
        table.issue_at(req("h2", "/b.txt"), t0).unwrap();
        let err = table.issue_at(req("h3", "/c.txt"), t0).unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
    }

    #[test]
    fn path_segment_round_trips_and_extract_validates_names() {
        for name in ["hello.txt", "Holiday 2026.mp4", "café #1?.png", "a+b%c.jpg"] {
            let encoded = encode_path_segment(name);
            assert!(!encoded.contains(['/', ' ', '?', '#']));
            assert_eq!(decode_path_segment(&encoded).as_deref(), Some(name));
        }
        assert_eq!(decode_path_segment("bad%2"), None);
        assert_eq!(decode_path_segment("bad%ZZ"), None);
        assert_eq!(extract_file_name("/home/me/clip.mp4").unwrap(), "clip.mp4");
        assert_eq!(
            extract_file_name(r"C:\Users\me\clip.mp4").unwrap(),
            "clip.mp4"
        );
        assert!(extract_file_name("/home/me/dir/").is_err());
        assert!(extract_file_name("").is_err());
        assert!(extract_file_name("/home/me/..").is_err());
    }
}
