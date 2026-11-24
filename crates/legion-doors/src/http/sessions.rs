//! Which token opened each MCP session, so a session is only ever used by the token that opened
//! it.
//!
//! The SDK keeps sessions by id alone, and anyone who learns an id could otherwise send on it,
//! read the replies to it, end it or cancel its calls. So the door records, for every session it
//! sees opened, the token that opened it, and turns away any request for a session that is not
//! that token's exactly as it turns away one for a session that does not exist. A session ends
//! when its token is removed, replaced or expires, and when it has been idle too long, and a token
//! may hold only so many at once.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body::{Body, Frame, SizeHint};

use legion_core::tokens::Tokens;

use crate::Authenticated;

/// The most sessions one token holds at once. Opening one more ends the token's least recently
/// used session, so a client that forgets to end its sessions keeps working.
pub(crate) const PER_TOKEN: usize = 16;

/// The most sessions the server holds at once. Past this, opening a session is refused until one
/// ends or goes idle.
pub(crate) const TOTAL: usize = 256;

/// How long a session may go unused before it is ended: the SDK's own idle limit.
pub(crate) const IDLE: Duration = Duration::from_secs(5 * 60);

/// A session the server knows of, by the MCP endpoint it was opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRef {
    pub(crate) id: String,
    /// Whether it was opened on the compact endpoint, whose sessions are kept apart.
    pub(crate) compact: bool,
}

struct Binding {
    owner: Authenticated,
    compact: bool,
    last_used: Instant,
    /// A place held for a session being opened, which has no id yet.
    pending: bool,
}

/// Every live session and the token that opened it.
#[derive(Default)]
pub(crate) struct Sessions {
    bound: Mutex<HashMap<String, Binding>>,
    /// Numbers the places held for sessions being opened.
    reservations: AtomicU64,
}

/// A place held for a session while it is being opened, so a burst of `initialize` requests
/// cannot open more than the limits allow between checking for room and taking it. Dropped
/// without [`Reservation::bind`], when opening failed, it gives the place back.
pub(crate) struct Reservation<'a> {
    sessions: &'a Sessions,
    key: String,
    owner: Option<Authenticated>,
    compact: bool,
}

impl Reservation<'_> {
    /// Records the session opened in the place held, under the id the SDK gave it.
    pub(crate) fn bind(mut self, id: String) {
        let mut bound = self.sessions.lock();
        bound.remove(&self.key);
        if let Some(owner) = self.owner.take() {
            bound.insert(
                id,
                Binding {
                    owner,
                    compact: self.compact,
                    last_used: Instant::now(),
                    pending: false,
                },
            );
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.owner.is_some() {
            self.sessions.lock().remove(&self.key);
        }
    }
}

impl Sessions {
    /// Whether `who` may use the session `id` on this endpoint: it is one this server opened, for
    /// the same token, and it has not gone idle. Using it counts as activity.
    pub(crate) fn admit(&self, id: &str, who: &Authenticated, compact: bool) -> bool {
        let mut bound = self.lock();
        match bound.get_mut(id) {
            Some(binding)
                if !binding.pending
                    && binding.owner == *who
                    && binding.compact == compact
                    && binding.last_used.elapsed() < IDLE =>
            {
                binding.last_used = Instant::now();
                true
            }
            _ => false,
        }
    }

    /// Holds a place for one more session of `who`'s, and returns it with the sessions to end so
    /// the token stays within [`PER_TOKEN`] (its least recently used, already forgotten here; the
    /// caller closes them). `None` when there is no room: the server holds [`TOTAL`] already, or
    /// the token's places are all taken by sessions still being opened.
    ///
    /// Room is found and taken under one lock, so requests racing each other are counted.
    pub(crate) fn reserve(
        &self,
        who: &Authenticated,
        compact: bool,
    ) -> Option<(Reservation<'_>, Vec<SessionRef>)> {
        let mut bound = self.lock();
        let mut own: Vec<(&String, Instant, bool)> = bound
            .iter()
            .filter(|(_, binding)| binding.owner == *who)
            .map(|(id, binding)| (id, binding.last_used, binding.pending))
            .collect();
        let surplus = (own.len() + 1).saturating_sub(PER_TOKEN);
        own.retain(|(_, _, pending)| !pending);
        own.sort_by_key(|(_, last_used, _)| *last_used);
        if own.len() < surplus {
            return None;
        }
        let evicted: Vec<String> = own
            .into_iter()
            .take(surplus)
            .map(|(id, _, _)| id.clone())
            .collect();
        if bound.len() - evicted.len() >= TOTAL {
            return None;
        }
        let ended = evicted
            .into_iter()
            .filter_map(|id| {
                bound.remove(&id).map(|binding| SessionRef {
                    id,
                    compact: binding.compact,
                })
            })
            .collect();
        // No header value can hold a NUL, so no client can name this key as its session.
        let key = format!(
            "\0reserved-{}",
            self.reservations.fetch_add(1, Ordering::Relaxed)
        );
        bound.insert(
            key.clone(),
            Binding {
                owner: who.clone(),
                compact,
                last_used: Instant::now(),
                pending: true,
            },
        );
        drop(bound);
        Some((
            Reservation {
                sessions: self,
                key,
                owner: Some(who.clone()),
                compact,
            },
            ended,
        ))
    }

    /// Forgets a session its client ended.
    pub(crate) fn forget(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Forgets and returns every session to end now: idle ones, and those whose token is no
    /// longer in `tokens` as it was, or has expired. `tokens` is `None` when the file cannot be
    /// read, and then no token is good and every session ends.
    pub(crate) fn stale(&self, tokens: Option<&Tokens>) -> Vec<SessionRef> {
        let now = jiff::Timestamp::now();
        let mut bound = self.lock();
        let ended: Vec<String> = bound
            .iter()
            .filter(|(_, binding)| {
                let token_good = tokens
                    .and_then(|tokens| tokens.tokens.get(&binding.owner.token))
                    .is_some_and(|entry| {
                        entry.hash == binding.owner.hash
                            && entry.principal.as_str() == binding.owner.principal
                            && !entry.expired(now)
                    });
                !token_good || binding.last_used.elapsed() >= IDLE
            })
            .map(|(id, _)| id.clone())
            .collect();
        ended
            .into_iter()
            .filter_map(|id| {
                bound
                    .remove(&id)
                    .filter(|binding| !binding.pending)
                    .map(|binding| SessionRef {
                        id,
                        compact: binding.compact,
                    })
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Binding>> {
        // The map stays consistent whatever panicked while holding it.
        self.bound.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A response body that runs `on_abandon` when it is dropped before it ended: what happens when
/// the client goes away while its request is still being answered.
///
/// The SDK cancels a call when a stateless request's stream is dropped, but not when one inside a
/// session is; the door uses this to tell the session the request was cancelled, as the client
/// would have, so the call stops and gives back its slot.
pub(crate) struct Abandonable<B> {
    inner: B,
    on_abandon: Option<Box<dyn FnOnce() + Send>>,
}

impl<B> Abandonable<B> {
    pub(crate) fn new(inner: B, on_abandon: impl FnOnce() + Send + 'static) -> Abandonable<B> {
        Abandonable {
            inner,
            on_abandon: Some(Box::new(on_abandon)),
        }
    }
}

impl<B: Body + Unpin> Body for Abandonable<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        // A server stops polling a body that says it has ended, and drops it: that is not
        // abandoning it.
        if matches!(polled, Poll::Ready(None)) || self.inner.is_end_stream() {
            self.on_abandon = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for Abandonable<B> {
    fn drop(&mut self) {
        if let Some(on_abandon) = self.on_abandon.take() {
            on_abandon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(token: &str, principal: &str) -> Authenticated {
        Authenticated {
            principal: principal.to_owned(),
            token: token.to_owned(),
            hash: legion_core::tokens::hash(token),
            expires: None,
        }
    }

    fn open(sessions: &Sessions, id: &str, owner: &Authenticated) -> Vec<SessionRef> {
        let (place, ended) = sessions.reserve(owner, false).unwrap();
        place.bind(id.to_owned());
        ended
    }

    #[test]
    fn a_session_is_its_tokens_alone() {
        let sessions = Sessions::default();
        let phone = who("phone", "phone");
        open(&sessions, "s1", &phone);
        assert!(sessions.admit("s1", &phone, false));
        assert!(!sessions.admit("s1", &who("admin", "admin"), false));
        assert!(!sessions.admit("s1", &who("phone2", "phone"), false));
        assert!(!sessions.admit("s1", &phone, true));
        assert!(!sessions.admit("s2", &phone, false));
        sessions.forget("s1");
        assert!(!sessions.admit("s1", &phone, false));
    }

    #[test]
    fn a_token_holds_only_so_many() {
        let sessions = Sessions::default();
        let phone = who("phone", "phone");
        for n in 0..PER_TOKEN {
            assert_eq!(open(&sessions, &format!("s{n}"), &phone), Vec::new());
        }
        assert!(sessions.admit("s0", &phone, false));
        // s0 was just used, so another is the least recently used.
        let ended = open(&sessions, "late", &phone);
        assert_eq!(ended.len(), 1);
        assert_ne!(ended[0].id, "s0");
        assert!(!sessions.admit(&ended[0].id, &phone, false));
        assert!(sessions.admit("s0", &phone, false));
        assert!(sessions.admit("late", &phone, false));
    }

    #[test]
    fn places_being_opened_count_and_are_given_back() {
        let sessions = Sessions::default();
        let phone = who("phone", "phone");
        let held: Vec<_> = (0..PER_TOKEN)
            .map(|_| sessions.reserve(&phone, false).unwrap().0)
            .collect();
        // Every place is being opened, and none can be ended to make room.
        assert!(sessions.reserve(&phone, false).is_none());
        assert!(!sessions.admit("\0reserved-0", &phone, false));
        drop(held);
        assert!(sessions.reserve(&phone, false).is_some());
        assert!(sessions.lock().is_empty());
    }

    #[test]
    fn the_server_holds_only_so_many() {
        let sessions = Sessions::default();
        for n in 0..TOTAL {
            open(&sessions, &format!("s{n}"), &who(&format!("t{n}"), "p"));
        }
        assert!(sessions.reserve(&who("late", "p"), false).is_none());
        // A token at its own limit still makes room by ending its own oldest.
        let crowded = Sessions::default();
        let phone = who("phone", "phone");
        for n in 0..TOTAL {
            let owner = if n < PER_TOKEN {
                phone.clone()
            } else {
                who(&format!("t{n}"), "p")
            };
            open(&crowded, &format!("s{n}"), &owner);
        }
        assert_eq!(
            crowded.reserve(&phone, false).map(|(_, ended)| ended.len()),
            Some(1)
        );
    }
}
