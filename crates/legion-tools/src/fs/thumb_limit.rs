//! Fair, bounded-wait, cancellable resource pools for `fs.thumb`.
//!
//! Images and videos use separate pools so a batch of slow `ffmpeg` extractions cannot stall image
//! thumbnails:
//! - The **image pool** accounts for in-flight megapixels (`MAX_IN_FLIGHT_IMAGE_MP`) as well as
//!   total concurrent image decodes (`MAX_IN_FLIGHT_IMAGES`), bounding real peak memory across
//!   concurrent progressive JPEG decodes.
//! - The **video pool** caps simultaneous `ffmpeg` subprocesses (`MAX_IN_FLIGHT_VIDEOS`).
//!
//! Both pools schedule waiting callers round-robin by principal so one caller sending many
//! 32-path requests cannot starve other principals, and both enforce a bounded wait timeout and
//! cooperative cancellation.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_core::CancellationToken;
use tokio::sync::Notify;

/// Why waiting for a pool permit did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcquireError {
    /// The caller's cancellation token fired while waiting.
    Cancelled,
    /// Waiting for capacity exceeded the bounded wait timeout (caller should retry shortly).
    TimedOut,
}

struct Waiter {
    id: u64,
    cost: usize,
    granted: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

struct PoolState {
    used_units: usize,
    used_slots: usize,
    active_by_principal: HashMap<String, usize>,
    queues: HashMap<String, VecDeque<Waiter>>,
    rr_order: VecDeque<String>,
}

/// Fair round-robin resource pool bounding both abstract units (for example megapixels) and raw
/// concurrent slot count.
pub(crate) struct FairPool {
    max_units: usize,
    max_slots: usize,
    next_id: AtomicU64,
    state: Mutex<PoolState>,
}

/// RAII guard holding capacity in a [`FairPool`]; dropping releases the capacity and wakes the
/// next fair waiter.
pub(crate) struct PoolPermit {
    pool: Arc<FairPool>,
    principal: String,
    cost: usize,
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        self.pool.release(&self.principal, self.cost);
    }
}

/// Drop guard ensuring a cancelled or dropped waiter future never leaks a queued entry or an
/// already-granted permit.
struct WaitGuard {
    pool: Arc<FairPool>,
    principal: String,
    id: u64,
    cost: usize,
    granted: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.pool
            .cancel_or_release(&self.principal, self.id, self.cost, &self.granted);
    }
}

impl FairPool {
    /// Creates a new pool allowing up to `max_units` total units and `max_slots` simultaneous
    /// holders.
    pub(crate) fn new(max_units: usize, max_slots: usize) -> Arc<FairPool> {
        Arc::new(FairPool {
            max_units: max_units.max(1),
            max_slots: max_slots.max(1),
            next_id: AtomicU64::new(1),
            state: Mutex::new(PoolState {
                used_units: 0,
                used_slots: 0,
                active_by_principal: HashMap::new(),
                queues: HashMap::new(),
                rr_order: VecDeque::new(),
            }),
        })
    }

    /// Acquires `cost` units (clamped to `1..=max_units`) for `principal`, waiting up to
    /// `wait_timeout` while watching `cancel`.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        principal: &str,
        cost: usize,
        wait_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<PoolPermit, AcquireError> {
        if cancel.is_cancelled() {
            return Err(AcquireError::Cancelled);
        }
        let cost = cost.clamp(1, self.max_units);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let granted = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());

        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.rr_order.is_empty()
                && state.used_units.saturating_add(cost) <= self.max_units
                && state.used_slots.saturating_add(1) <= self.max_slots
            {
                state.used_units = state.used_units.saturating_add(cost);
                state.used_slots = state.used_slots.saturating_add(1);
                *state
                    .active_by_principal
                    .entry(principal.to_owned())
                    .or_insert(0) += 1;
                return Ok(PoolPermit {
                    pool: Arc::clone(self),
                    principal: principal.to_owned(),
                    cost,
                });
            }

            let was_empty = state.queues.get(principal).is_none_or(VecDeque::is_empty);
            if was_empty {
                state.rr_order.push_back(principal.to_owned());
            }
            state
                .queues
                .entry(principal.to_owned())
                .or_default()
                .push_back(Waiter {
                    id,
                    cost,
                    granted: Arc::clone(&granted),
                    notify: Arc::clone(&notify),
                });
            self.dispatch_locked(&mut state);
        }

        if granted.load(Ordering::Acquire) {
            return Ok(PoolPermit {
                pool: Arc::clone(self),
                principal: principal.to_owned(),
                cost,
            });
        }

        let mut guard = WaitGuard {
            pool: Arc::clone(self),
            principal: principal.to_owned(),
            id,
            cost,
            granted: Arc::clone(&granted),
            armed: true,
        };

        let sleep = tokio::time::sleep(wait_timeout);
        tokio::pin!(sleep);

        loop {
            let notified = notify.notified();
            if granted.load(Ordering::Acquire) {
                guard.armed = false;
                return Ok(PoolPermit {
                    pool: Arc::clone(self),
                    principal: principal.to_owned(),
                    cost,
                });
            }
            tokio::select! {
                biased;
                () = notified => {
                    if granted.load(Ordering::Acquire) {
                        guard.armed = false;
                        return Ok(PoolPermit {
                            pool: Arc::clone(self),
                            principal: principal.to_owned(),
                            cost,
                        });
                    }
                }
                () = cancel.cancelled() => {
                    return Err(AcquireError::Cancelled);
                }
                () = &mut sleep => {
                    return Err(AcquireError::TimedOut);
                }
            }
        }
    }

    fn release(&self, principal: &str, cost: usize) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.used_units = state.used_units.saturating_sub(cost);
        state.used_slots = state.used_slots.saturating_sub(1);
        if let Some(count) = state.active_by_principal.get_mut(principal) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.active_by_principal.remove(principal);
            }
        }
        self.dispatch_locked(&mut state);
    }

    fn cancel_or_release(&self, principal: &str, id: u64, cost: usize, granted: &AtomicBool) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if granted.load(Ordering::Acquire) {
            state.used_units = state.used_units.saturating_sub(cost);
            state.used_slots = state.used_slots.saturating_sub(1);
            if let Some(count) = state.active_by_principal.get_mut(principal) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.active_by_principal.remove(principal);
                }
            }
            self.dispatch_locked(&mut state);
            return;
        }
        if let Some(queue) = state.queues.get_mut(principal) {
            if let Some(pos) = queue.iter().position(|w| w.id == id) {
                queue.remove(pos);
            }
            if queue.is_empty() {
                state.queues.remove(principal);
                state.rr_order.retain(|p| p != principal);
            }
        }
        self.dispatch_locked(&mut state);
    }

    fn dispatch_locked(&self, state: &mut PoolState) {
        loop {
            let num_principals = state.rr_order.len();
            if num_principals == 0 {
                break;
            }
            // Prefer a waiting principal with zero currently active slots if another principal
            // already holds slots and only one slot remains, preventing monopoly starvation.
            let mut chosen_pos: Option<usize> = None;
            let multiple_principals = num_principals > 1;
            let last_slot_left = state.used_slots.saturating_add(1) == self.max_slots;

            // First pass: check idle waiting principals first when multiple principals compete
            // for the last slot; otherwise follow strict round-robin order.
            if multiple_principals && last_slot_left {
                for (pos, principal) in state.rr_order.iter().enumerate() {
                    if state
                        .active_by_principal
                        .get(principal)
                        .copied()
                        .unwrap_or(0)
                        > 0
                    {
                        continue;
                    }
                    if let Some(front) = state.queues.get(principal).and_then(VecDeque::front)
                        && state.used_units.saturating_add(front.cost) <= self.max_units
                        && state.used_slots.saturating_add(1) <= self.max_slots
                    {
                        chosen_pos = Some(pos);
                        break;
                    }
                }
            }

            if chosen_pos.is_none() {
                for (pos, principal) in state.rr_order.iter().enumerate() {
                    if let Some(front) = state.queues.get(principal).and_then(VecDeque::front)
                        && state.used_units.saturating_add(front.cost) <= self.max_units
                        && state.used_slots.saturating_add(1) <= self.max_slots
                    {
                        chosen_pos = Some(pos);
                        break;
                    }
                }
            }

            let Some(pos) = chosen_pos else {
                break;
            };
            let Some(principal) = state.rr_order.remove(pos) else {
                break;
            };
            let Some(waiter) = state
                .queues
                .get_mut(&principal)
                .and_then(VecDeque::pop_front)
            else {
                state.queues.remove(&principal);
                continue;
            };
            let still_waiting = state.queues.get(&principal).is_some_and(|q| !q.is_empty());
            if still_waiting {
                state.rr_order.push_back(principal.clone());
            } else {
                state.queues.remove(&principal);
            }

            state.used_units = state.used_units.saturating_add(waiter.cost);
            state.used_slots = state.used_slots.saturating_add(1);
            *state.active_by_principal.entry(principal).or_insert(0) += 1;
            waiter.granted.store(true, Ordering::Release);
            waiter.notify.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pixel_budget_caps_in_flight_megapixels_and_times_out_cleanly() {
        let pool = FairPool::new(50, 8);
        let cancel = CancellationToken::new();

        // Two 25 MP decodes consume all 50 MP in flight.
        let p1 = pool
            .acquire("alice", 25, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        let p2 = pool
            .acquire("alice", 25, Duration::from_secs(1), &cancel)
            .await
            .unwrap();

        // A third 10 MP decode must time out when the pool stays full.
        let err = pool
            .acquire("bob", 10, Duration::from_millis(30), &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(err, AcquireError::TimedOut);

        // Releasing one 25 MP permit immediately allows a 20 MP decode.
        drop(p1);
        let p3 = pool
            .acquire("bob", 20, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        drop(p2);
        drop(p3);
    }

    #[tokio::test]
    async fn round_robin_fairness_prevents_one_principal_from_starving_others() {
        let pool = FairPool::new(2, 2);
        let cancel = CancellationToken::new();

        // Principal A holds both video slots.
        let hold1 = pool
            .acquire("alice", 1, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        let hold2 = pool
            .acquire("alice", 1, Duration::from_secs(1), &cancel)
            .await
            .unwrap();

        // Principal A queues 4 more requests, then Principal B queues 1.
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let mut alice_tasks = Vec::new();
        for _ in 0..4 {
            let p = Arc::clone(&pool);
            let c = cancel.clone();
            let o = Arc::clone(&order);
            alice_tasks.push(tokio::spawn(async move {
                let _permit = p
                    .acquire("alice", 1, Duration::from_secs(2), &c)
                    .await
                    .unwrap();
                o.lock().unwrap().push("alice");
            }));
        }
        // Let Alice's tasks enqueue first.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(15)).await;

        let bob_pool = Arc::clone(&pool);
        let bob_cancel = cancel.clone();
        let bob_order = Arc::clone(&order);
        let bob_task = tokio::spawn(async move {
            let _permit = bob_pool
                .acquire("bob", 1, Duration::from_secs(2), &bob_cancel)
                .await
                .unwrap();
            bob_order.lock().unwrap().push("bob");
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(15)).await;

        // Release ONE slot while Alice still holds the other: Bob (with 0 active slots) must be
        // scheduled ahead of Alice's 4 queued requests!
        drop(hold1);
        bob_task.await.unwrap();
        drop(hold2);
        for t in alice_tasks {
            t.await.unwrap();
        }
        let seen = order.lock().unwrap().clone();
        assert_eq!(
            seen.first().copied(),
            Some("bob"),
            "expected bob first: {seen:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_removes_waiter_without_leaking_capacity() {
        let pool = FairPool::new(1, 1);
        let hold_cancel = CancellationToken::new();
        let hold = pool
            .acquire("alice", 1, Duration::from_secs(1), &hold_cancel)
            .await
            .unwrap();

        let waiter_cancel = CancellationToken::new();
        let wait_pool = Arc::clone(&pool);
        let wc = waiter_cancel.clone();
        let handle = tokio::spawn(async move {
            wait_pool
                .acquire("bob", 1, Duration::from_secs(5), &wc)
                .await
                .err()
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        waiter_cancel.cancel();
        assert_eq!(handle.await.unwrap(), Some(AcquireError::Cancelled));

        drop(hold);
        // Full capacity is still available after cancelling the waiter.
        let _reacquired = pool
            .acquire("carol", 1, Duration::from_secs(1), &hold_cancel)
            .await
            .unwrap();
    }
}
