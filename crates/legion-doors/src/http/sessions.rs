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
}

/// Every live session and the token that opened it.
#[derive(Default)]
pub(crate) struct Sessions {
    bound: Mutex<HashMap<String, Binding>>,
}

impl Sessions {
    /// Whether `who` may use the session `id` on this endpoint: it is one this server opened, for
    /// the same token, and it has not gone idle. Using it counts as activity.
    pub(crate) fn admit(&self, id: &str, who: &Authenticated, compact: bool) -> bool {
        let mut bound = self.lock();
        match bound.get_mut(id) {
            Some(binding)
                if binding.owner == *who
                    && binding.compact == compact
                    && binding.last_used.elapsed() < IDLE =>
            {
                binding.last_used = Instant::now();
                true
            }
            _ => false,
        }
    }

    /// Makes room for one more session of `who`'s: the sessions to end so the token stays within
    /// [`PER_TOKEN`], or `None` when the server already holds [`TOTAL`] and nothing can be ended.
    /// The sessions returned are forgotten already; the caller closes them.
    pub(crate) fn make_room(&self, who: &Authenticated) -> Option<Vec<SessionRef>> {
        let mut bound = self.lock();
        let mut own: Vec<(&String, Instant)> = bound
            .iter()
            .filter(|(_, binding)| binding.owner == *who)
            .map(|(id, binding)| (id, binding.last_used))
            .collect();
        own.sort_by_key(|(_, last_used)| *last_used);
        let surplus = (own.len() + 1).saturating_sub(PER_TOKEN);
        let evicted: Vec<String> = own
            .into_iter()
            .take(surplus)
            .map(|(id, _)| id.clone())
            .collect();
        if bound.len() - evicted.len() >= TOTAL {
            return None;
        }
        Some(
            evicted
                .into_iter()
                .filter_map(|id| {
                    bound.remove(&id).map(|binding| SessionRef {
                        id,
                        compact: binding.compact,
                    })
                })
                .collect(),
        )
    }

    /// Records that `who` opened the session `id`.
    pub(crate) fn bind(&self, id: String, who: Authenticated, compact: bool) {
        self.lock().insert(
            id,
            Binding {
                owner: who,
                compact,
                last_used: Instant::now(),
            },
        );
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
                bound.remove(&id).map(|binding| SessionRef {
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
        if matches!(polled, Poll::Ready(None)) {
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

    #[test]
    fn a_session_is_its_tokens_alone() {
        let sessions = Sessions::default();
        let phone = who("phone", "phone");
        sessions.bind("s1".to_owned(), phone.clone(), false);
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
            assert_eq!(sessions.make_room(&phone), Some(Vec::new()));
            sessions.bind(format!("s{n}"), phone.clone(), false);
        }
        assert!(sessions.admit("s0", &phone, false));
        // s0 was just used, so another is the least recently used.
        let ended = sessions.make_room(&phone).unwrap();
        assert_eq!(ended.len(), 1);
        assert_ne!(ended[0].id, "s0");
        assert!(!sessions.admit(&ended[0].id, &phone, false));
        assert!(sessions.admit("s0", &phone, false));
    }

    #[test]
    fn the_server_holds_only_so_many() {
        let sessions = Sessions::default();
        for n in 0..TOTAL {
            sessions.bind(format!("s{n}"), who(&format!("t{n}"), "p"), false);
        }
        assert_eq!(sessions.make_room(&who("late", "p")), None);
        // A token at its own limit still makes room by ending its own oldest.
        let crowded = Sessions::default();
        let phone = who("phone", "phone");
        for n in 0..TOTAL {
            let owner = if n < PER_TOKEN {
                phone.clone()
            } else {
                who(&format!("t{n}"), "p")
            };
            crowded.bind(format!("s{n}"), owner, false);
        }
        assert_eq!(crowded.make_room(&phone).map(|ended| ended.len()), Some(1));
    }
}
