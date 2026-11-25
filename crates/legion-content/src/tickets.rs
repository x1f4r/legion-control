//! Ticket table for content addresses.
//!
//! Each ticket carries 256 random bits from the operating system CSPRNG, is stored hashed with
//! BLAKE3 so raw ticket tokens do not sit in map keys, and is bound to one holder (bearer token or
//! device pairing), principal, machine, and path. Tickets expire 15 minutes after their last use
//! and 6 hours after creation at most using a suspend-safe clock (taking the maximum of monotonic
//! and wall-clock elapsed time so sleeping a laptop cannot stretch expiry and NTP backward jumps
//! cannot stall it), are capped per holder (evicting the holder's least recently used ticket) and
//! globally, and can be revoked by holder.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

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

/// Maximum length in bytes of a file path stored in a content ticket (4 KiB).
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum length in bytes of ticket metadata fields (`holder`, `principal`, `machine`).
pub const MAX_FIELD_BYTES: usize = 256;

/// Suspend-safe timestamp pairing wall-clock time with a monotonic clock.
///
/// Elapsed time between two snapshots is `max(wall_elapsed, mono_elapsed)`:
/// - Across host suspend/sleep, [`Instant`] may pause while [`SystemTime`] advances, so tickets
///   still expire on schedule.
/// - Across a backward wall-clock jump (for example NTP adjustment), [`SystemTime::duration_since`]
///   clamps to zero while [`Instant`] continues to advance monotonically.
#[derive(Clone, Copy, Debug)]
pub struct TicketNow {
    wall: SystemTime,
    mono: Instant,
}

impl TicketNow {
    /// Captures the current wall-clock and monotonic timestamps.
    pub fn now() -> Self {
        Self {
            wall: SystemTime::now(),
            mono: Instant::now(),
        }
    }

    /// Constructs a timestamp from explicit wall-clock and monotonic readings (for tests).
    pub const fn from_parts(wall: SystemTime, mono: Instant) -> Self {
        Self { wall, mono }
    }

    /// Suspend-safe elapsed duration since `earlier`.
    pub fn elapsed_since(self, earlier: TicketNow) -> Duration {
        let wall = self
            .wall
            .duration_since(earlier.wall)
            .unwrap_or(Duration::ZERO);
        let mono = self.mono.saturating_duration_since(earlier.mono);
        wall.max(mono)
    }

    /// Advances both clocks by `delta` (convenience helper for tests).
    pub fn advance(self, delta: Duration) -> Self {
        Self {
            wall: self.wall + delta,
            mono: self.mono + delta,
        }
    }
}

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
    /// BLAKE3 hash of the raw ticket id, safe for internal indexing and audit coalescing without
    /// retaining or logging the secret id.
    pub key_hash: [u8; 32],
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
    /// BLAKE3 hash of [`Self::id`] for internal correlation without storing the secret.
    pub key_hash: [u8; 32],
    /// Basename derived from `path`, ready to encode into the URL's final segment.
    pub file_name: String,
    /// Seconds until the ticket expires if unused (the sliding lifetime, capped by absolute).
    pub expires_in: u64,
}

struct Entry {
    ticket: Ticket,
    created: TicketNow,
    last_used: TicketNow,
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
        self.issue_at(req, TicketNow::now())
    }

    /// Issues a new 256-bit random ticket using an explicit `now` timestamp (for tests).
    pub fn issue_at(&self, req: TicketRequest, now: TicketNow) -> Result<IssuedTicket, ErrorBody> {
        validate_request(&req)?;
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
        let mut own: Vec<([u8; 32], Duration, u64)> = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.ticket.holder == req.holder)
            .map(|(k, entry)| (*k, now.elapsed_since(entry.last_used), entry.use_seq))
            .collect();
        let surplus = (own.len() + 1).saturating_sub(self.config.max_per_holder);
        if surplus > 0 {
            // Largest elapsed time since last use (oldest) first; break ties by oldest use_seq.
            own.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
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
                    key_hash: key,
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
            key_hash: key,
            file_name,
            expires_in,
        })
    }

    /// Looks up `id`, refreshing its sliding expiry on hit and returning `None` if unknown or
    /// expired.
    pub fn lookup(&self, id: &str) -> Option<Ticket> {
        self.lookup_at(id, TicketNow::now())
    }

    /// Looks up `id` at an explicit `now` timestamp (for tests).
    pub fn lookup_at(&self, id: &str, now: TicketNow) -> Option<Ticket> {
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

    /// Checks whether the ticket identified by `key_hash` is still present and within its hard
    /// lifetime, refreshing its sliding activity timestamp when still live.
    ///
    /// Used by in-flight streams so revoking a ticket, evicting it, or crossing the 6-hour hard
    /// cap stops an active download promptly.
    pub fn is_live(&self, key_hash: &[u8; 32]) -> bool {
        self.is_live_at(key_hash, TicketNow::now())
    }

    /// Like [`Self::is_live`] at an explicit `now` timestamp (for tests).
    pub fn is_live_at(&self, key_hash: &[u8; 32], now: TicketNow) -> bool {
        let mut inner = self.lock();
        let Some(expired) = inner
            .entries
            .get(key_hash)
            .map(|entry| self.is_expired(entry, now))
        else {
            return false;
        };
        if expired {
            inner.entries.remove(key_hash);
            return false;
        }
        if let Some(entry) = inner.entries.get_mut(key_hash) {
            entry.last_used = now;
        }
        true
    }

    /// Removes one ticket by id, returning `true` if it existed.
    pub fn revoke(&self, id: &str) -> bool {
        self.revoke_hash(&ticket_key(id))
    }

    /// Removes one ticket by its key hash, returning `true` if it existed.
    pub fn revoke_hash(&self, key_hash: &[u8; 32]) -> bool {
        self.lock().entries.remove(key_hash).is_some()
    }

    /// Removes every ticket owned by `holder`, returning how many were removed.
    pub fn revoke_holder(&self, holder: &str) -> usize {
        self.revoke_holder_hashes(holder).len()
    }

    /// Removes every ticket owned by `holder`, returning the key hashes of removed tickets.
    pub fn revoke_holder_hashes(&self, holder: &str) -> Vec<[u8; 32]> {
        let mut inner = self.lock();
        let mut removed = Vec::new();
        inner.entries.retain(|key, entry| {
            if entry.ticket.holder == holder {
                removed.push(*key);
                false
            } else {
                true
            }
        });
        removed
    }

    /// Sweeps expired tickets and any ticket whose holder fails `keep`, returning how many were
    /// removed.
    pub fn retain_holders(&self, keep: impl FnMut(&str) -> bool) -> usize {
        self.sweep_holders_at(TicketNow::now(), keep).len()
    }

    /// Like [`Self::retain_holders`] at an explicit `now` timestamp (for tests).
    pub fn retain_holders_at(&self, now: TicketNow, keep: impl FnMut(&str) -> bool) -> usize {
        self.sweep_holders_at(now, keep).len()
    }

    /// Sweeps expired tickets and any ticket whose holder fails `keep`, returning the key hashes
    /// of removed tickets so callers can flush per-ticket state (such as coalesced audit entries).
    pub fn sweep_holders(&self, keep: impl FnMut(&str) -> bool) -> Vec<[u8; 32]> {
        self.sweep_holders_at(TicketNow::now(), keep)
    }

    /// Like [`Self::sweep_holders`] at an explicit `now` timestamp (for tests).
    pub fn sweep_holders_at(
        &self,
        now: TicketNow,
        mut keep: impl FnMut(&str) -> bool,
    ) -> Vec<[u8; 32]> {
        let mut inner = self.lock();
        let mut removed = Vec::new();
        inner.entries.retain(|key, entry| {
            let alive = !self.is_expired(entry, now) && keep(&entry.ticket.holder);
            if !alive {
                removed.push(*key);
            }
            alive
        });
        removed
    }

    /// Total number of tickets currently stored (including any not yet swept).
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether a ticket with `key_hash` is currently present in the table (without modifying its
    /// last-used timestamp).
    pub fn contains_hash(&self, key_hash: &[u8; 32]) -> bool {
        self.lock().entries.contains_key(key_hash)
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

    fn is_expired(&self, entry: &Entry, now: TicketNow) -> bool {
        now.elapsed_since(entry.last_used) >= self.config.sliding_ttl
            || now.elapsed_since(entry.created) >= self.config.absolute_ttl
    }

    fn prune_expired_locked(&self, inner: &mut Inner, now: TicketNow) {
        inner
            .entries
            .retain(|_, entry| !self.is_expired(entry, now));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn validate_request(req: &TicketRequest) -> Result<(), ErrorBody> {
    if req.path.len() > MAX_PATH_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("path is longer than the {MAX_PATH_BYTES}-byte limit."),
        )
        .with_hint("Pass a shorter file path."));
    }
    if req.holder.is_empty() || req.holder.len() > MAX_FIELD_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Ticket holder must be 1..={MAX_FIELD_BYTES} bytes."),
        ));
    }
    if req.principal.is_empty() || req.principal.len() > MAX_FIELD_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Ticket principal must be 1..={MAX_FIELD_BYTES} bytes."),
        ));
    }
    if req.machine.is_empty() || req.machine.len() > MAX_FIELD_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Ticket machine must be 1..={MAX_FIELD_BYTES} bytes."),
        ));
    }
    Ok(())
}

fn ticket_key(id: &str) -> [u8; 32] {
    *blake3::hash(id.as_bytes()).as_bytes()
}

/// Extracts the file name from a Unix or Windows path, refusing empty, overlong,
/// trailing-separator, or dot names.
pub fn extract_file_name(path: &str) -> Result<String, ErrorBody> {
    if path.len() > MAX_PATH_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("path is longer than the {MAX_PATH_BYTES}-byte limit."),
        )
        .with_hint("Pass a shorter file path."));
    }
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
        let t0 = TicketNow::now();
        let issued = table.issue_at(req("h1", "/home/me/video.mp4"), t0).unwrap();
        assert_eq!(issued.id.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(&issued.id).unwrap().len(), 32);
        assert_eq!(issued.file_name, "video.mp4");
        assert_eq!(issued.expires_in, SLIDING_TTL.as_secs());

        // Use at 14 minutes extends sliding lifetime.
        let t14 = t0.advance(Duration::from_secs(14 * 60));
        let found = table.lookup_at(&issued.id, t14).unwrap();
        assert_eq!(found.path, "/home/me/video.mp4");
        assert_eq!(found.machine, "tower");
        assert_eq!(found.key_hash, issued.key_hash);

        // Still valid at 28 minutes (14 minutes after t14).
        let t28 = t14.advance(Duration::from_secs(14 * 60));
        assert!(table.lookup_at(&issued.id, t28).is_some());

        // Idle for 15 minutes expires it.
        let t_idle = t28.advance(SLIDING_TTL);
        assert!(table.lookup_at(&issued.id, t_idle).is_none());

        // Absolute 6-hour cap expires a continually used ticket.
        let issued2 = table.issue_at(req("h1", "/home/me/a.png"), t0).unwrap();
        let mut cursor = t0;
        for _ in 0..35 {
            cursor = cursor.advance(Duration::from_secs(10 * 60));
            assert!(table.lookup_at(&issued2.id, cursor).is_some());
        }
        assert!(
            table
                .lookup_at(&issued2.id, t0.advance(ABSOLUTE_TTL))
                .is_none()
        );
    }

    #[test]
    fn suspend_safe_clock_expires_across_sleep_and_survives_backward_wall_jump() {
        let table = TicketTable::new();
        let wall0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mono0 = Instant::now();
        let t0 = TicketNow::from_parts(wall0, mono0);

        let issued = table.issue_at(req("h1", "/home/me/sleep.txt"), t0).unwrap();

        // Simulate host suspend: monotonic clock barely moves, wall clock jumps 20 minutes.
        let after_sleep = TicketNow::from_parts(
            wall0 + Duration::from_secs(20 * 60),
            mono0 + Duration::from_secs(1),
        );
        assert!(
            table.lookup_at(&issued.id, after_sleep).is_none(),
            "wall-clock sleep past sliding TTL must expire the ticket"
        );

        // Simulate backward NTP jump: wall clock moves backward by 1 hour, monotonic advances 16m.
        let issued2 = table.issue_at(req("h1", "/home/me/ntp.txt"), t0).unwrap();
        let ntp_back_1m = TicketNow::from_parts(
            wall0 - Duration::from_secs(3600),
            mono0 + Duration::from_secs(60),
        );
        assert!(
            table.lookup_at(&issued2.id, ntp_back_1m).is_some(),
            "ticket stays valid while monotonic time is within TTL"
        );
        let ntp_back_16m = TicketNow::from_parts(
            wall0 - Duration::from_secs(3600),
            mono0 + Duration::from_secs(17 * 60),
        );
        assert!(
            table.lookup_at(&issued2.id, ntp_back_16m).is_none(),
            "monotonic clock still expires the ticket even if wall clock jumped backward"
        );
    }

    #[test]
    fn overlong_paths_and_fields_are_rejected() {
        let table = TicketTable::new();
        let too_long_path = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(too_long_path.len() > MAX_PATH_BYTES);
        let err = table.issue(req("h1", &too_long_path)).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(extract_file_name(&too_long_path).is_err());

        // Exactly MAX_PATH_BYTES is allowed.
        let exact_path = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
        assert_eq!(exact_path.len(), MAX_PATH_BYTES);
        assert!(table.issue(req("h1", &exact_path)).is_ok());

        let long_holder = "h".repeat(MAX_FIELD_BYTES + 1);
        assert_eq!(
            table.issue(req(&long_holder, "/a.txt")).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
    }

    #[test]
    fn holder_bound_evicts_lru_and_revocation_clears_only_that_holder() {
        let table = TicketTable::with_config(TicketConfig {
            max_per_holder: 3,
            max_total: 10,
            ..TicketConfig::default()
        });
        let t0 = TicketNow::now();
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
        let t0 = TicketNow::now();
        table.issue_at(req("h1", "/a.txt"), t0).unwrap();
        table.issue_at(req("h2", "/b.txt"), t0).unwrap();
        let err = table.issue_at(req("h3", "/c.txt"), t0).unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
    }

    #[test]
    fn path_segment_round_trips_and_extract_validates_names() {
        for name in [
            "hello.txt",
            "Holiday 2026.mp4",
            "café #1?.png",
            "a+b%c.jpg",
            "100%.txt",
            "a%41.txt",
            "trailing.",
        ] {
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
