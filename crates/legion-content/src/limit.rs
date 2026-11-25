//! Per-ticket, per-holder, and global concurrency limits for active content streams.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Default maximum concurrent content streams per ticket.
pub const DEFAULT_MAX_STREAMS_PER_TICKET: usize = 8;

/// Default maximum concurrent content streams per credential holder.
pub const DEFAULT_MAX_STREAMS_PER_HOLDER: usize = 16;

/// Default maximum concurrent content streams across the whole server.
pub const DEFAULT_MAX_STREAMS_GLOBAL: usize = 64;

/// Default `Retry-After` delay (seconds) advertised on `429 Too Many Requests`.
pub const DEFAULT_RETRY_AFTER_SECS: u64 = 5;

/// Bounds enforced by a [`ContentLimiter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConcurrencyLimits {
    /// Maximum active streams on a single ticket at once.
    pub max_per_ticket: usize,
    /// Maximum active streams across all tickets belonging to one holder at once.
    pub max_per_holder: usize,
    /// Maximum active streams across all tickets and holders combined.
    pub max_global: usize,
    /// Value emitted in `Retry-After` when a cap is exceeded.
    pub retry_after_secs: u64,
}

impl Default for ConcurrencyLimits {
    fn default() -> Self {
        Self {
            max_per_ticket: DEFAULT_MAX_STREAMS_PER_TICKET,
            max_per_holder: DEFAULT_MAX_STREAMS_PER_HOLDER,
            max_global: DEFAULT_MAX_STREAMS_GLOBAL,
            retry_after_secs: DEFAULT_RETRY_AFTER_SECS,
        }
    }
}

/// Which cap prevented a new content stream from starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitScope {
    /// The per-ticket cap was reached.
    Ticket,
    /// The per-holder cap was reached.
    Holder,
    /// The global server-wide cap was reached.
    Global,
}

/// Error returned when acquiring a [`StreamPermit`] would exceed configured bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Which cap was hit.
    pub scope: LimitScope,
    /// Recommended `Retry-After` seconds.
    pub retry_after_secs: u64,
}

#[derive(Default)]
struct State {
    global: usize,
    by_holder: HashMap<String, usize>,
    by_ticket: HashMap<[u8; 32], usize>,
}

struct Inner {
    limits: ConcurrencyLimits,
    state: Mutex<State>,
}

/// Cloneable concurrency limiter shared across content requests.
#[derive(Clone)]
pub struct ContentLimiter {
    inner: Arc<Inner>,
}

impl Default for ContentLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentLimiter {
    /// Creates a limiter with [`ConcurrencyLimits::default`].
    pub fn new() -> Self {
        Self::with_limits(ConcurrencyLimits::default())
    }

    /// Creates a limiter with explicit caps.
    pub fn with_limits(limits: ConcurrencyLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Attempts to reserve one active stream slot for `(ticket_hash, holder)`.
    ///
    /// Returns an RAII [`StreamPermit`] that releases all three counters automatically on drop.
    pub fn try_acquire(
        &self,
        ticket_hash: [u8; 32],
        holder: &str,
    ) -> Result<StreamPermit, LimitExceeded> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let limits = self.inner.limits;
        if state.global >= limits.max_global {
            return Err(LimitExceeded {
                scope: LimitScope::Global,
                retry_after_secs: limits.retry_after_secs,
            });
        }
        let holder_count = state.by_holder.get(holder).copied().unwrap_or(0);
        if holder_count >= limits.max_per_holder {
            return Err(LimitExceeded {
                scope: LimitScope::Holder,
                retry_after_secs: limits.retry_after_secs,
            });
        }
        let ticket_count = state.by_ticket.get(&ticket_hash).copied().unwrap_or(0);
        if ticket_count >= limits.max_per_ticket {
            return Err(LimitExceeded {
                scope: LimitScope::Ticket,
                retry_after_secs: limits.retry_after_secs,
            });
        }

        state.global += 1;
        *state.by_holder.entry(holder.to_owned()).or_insert(0) += 1;
        *state.by_ticket.entry(ticket_hash).or_insert(0) += 1;
        Ok(StreamPermit {
            inner: Arc::clone(&self.inner),
            ticket_hash,
            holder: holder.to_owned(),
        })
    }

    /// Current active global stream count.
    pub fn active_global(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .global
    }

    /// Current active stream count for `holder`.
    pub fn active_for_holder(&self, holder: &str) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_holder
            .get(holder)
            .copied()
            .unwrap_or(0)
    }

    /// Current active stream count for `ticket_hash`.
    pub fn active_for_ticket(&self, ticket_hash: &[u8; 32]) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_ticket
            .get(ticket_hash)
            .copied()
            .unwrap_or(0)
    }
}

/// RAII permit holding one active stream slot in a [`ContentLimiter`].
pub struct StreamPermit {
    inner: Arc<Inner>,
    ticket_hash: [u8; 32],
    holder: String,
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.global = state.global.saturating_sub(1);
        if let Some(count) = state.by_holder.get_mut(&self.holder) {
            if *count <= 1 {
                state.by_holder.remove(&self.holder);
            } else {
                *count -= 1;
            }
        }
        if let Some(count) = state.by_ticket.get_mut(&self.ticket_hash) {
            if *count <= 1 {
                state.by_ticket.remove(&self.ticket_hash);
            } else {
                *count -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_ticket_holder_and_global_caps_and_releases_on_drop() {
        let limiter = ContentLimiter::with_limits(ConcurrencyLimits {
            max_per_ticket: 2,
            max_per_holder: 3,
            max_global: 4,
            retry_after_secs: 7,
        });
        let t1 = [1u8; 32];
        let t2 = [2u8; 32];
        let t3 = [3u8; 32];

        let p1 = limiter.try_acquire(t1, "h1").unwrap();
        let p2 = limiter.try_acquire(t1, "h1").unwrap();
        // Per-ticket cap reached.
        let err = limiter.try_acquire(t1, "h1").err().unwrap();
        assert_eq!(err.scope, LimitScope::Ticket);
        assert_eq!(err.retry_after_secs, 7);

        // Another ticket for h1 fits within holder cap (3).
        let p3 = limiter.try_acquire(t2, "h1").unwrap();
        let err = limiter.try_acquire(t2, "h1").err().unwrap();
        assert_eq!(err.scope, LimitScope::Holder);

        // Another holder takes the 4th global slot.
        let p4 = limiter.try_acquire(t3, "h2").unwrap();
        let err = limiter.try_acquire(t3, "h2").err().unwrap();
        assert_eq!(err.scope, LimitScope::Global);

        assert_eq!(limiter.active_global(), 4);
        assert_eq!(limiter.active_for_holder("h1"), 3);
        assert_eq!(limiter.active_for_ticket(&t1), 2);

        drop(p1);
        assert_eq!(limiter.active_global(), 3);
        assert_eq!(limiter.active_for_ticket(&t1), 1);
        let p5 = limiter.try_acquire(t1, "h1").unwrap();

        drop((p2, p3, p4, p5));
        assert_eq!(limiter.active_global(), 0);
        assert_eq!(limiter.active_for_holder("h1"), 0);
        assert_eq!(limiter.active_for_holder("h2"), 0);
    }
}
