//! Fair, bounded-wait, cancellable resource pools for `fs.thumb`.
//!
//! Images and videos use separate pools so a batch of slow `ffmpeg` extractions cannot stall image
//! thumbnails:
//! - The **image pool** accounts for estimated in-flight decode bytes (`u64`) up to
//!   `min(512 MiB, total_machine_ram / 8)` (floored at the 256 MiB per-image cap so a single
//!   admitted image can always run) as well as total concurrent image decodes
//!   (`MAX_IN_FLIGHT_IMAGES`), bounding real peak memory across concurrent decodes.
//! - The **video pool** caps simultaneous `ffmpeg` subprocesses (`MAX_IN_FLIGHT_VIDEOS`).
//!
//! Both pools schedule waiting callers round-robin by principal so one caller sending many
//! 32-path requests cannot starve other principals, and both enforce a bounded wait timeout and
//! cooperative cancellation.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_core::CancellationToken;
use tokio::sync::Notify;

/// Maximum time a waiting head-of-queue request may be bypassed by smaller requests before the
/// scheduler holds new admissions so in-flight capacity drains for it.
const DEFAULT_STARVATION_AGE: Duration = Duration::from_millis(250);
/// Maximum number of smaller requests allowed to jump ahead of a waiting head-of-queue request
/// before the scheduler holds new admissions for it.
const DEFAULT_MAX_BYPASSES: u32 = 4;

/// Computes the image decode in-flight memory budget for this machine:
/// `min(max_budget, total_ram_bytes / 8)`, clamped below to `per_image_cap` so a single admitted
/// image can always run. When `total_ram_bytes` is 0 (unavailable), falls back to `max_budget`.
pub(crate) fn compute_image_in_flight_budget(
    total_ram_bytes: u64,
    per_image_cap: u64,
    max_budget: u64,
) -> u64 {
    if total_ram_bytes == 0 {
        return max_budget.max(per_image_cap);
    }
    let eighth = total_ram_bytes / 8;
    eighth.clamp(per_image_cap, max_budget.max(per_image_cap))
}

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
    cost: u64,
    enqueued_at: Instant,
    bypasses: u32,
    granted: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

struct PoolState {
    used_units: u64,
    used_slots: usize,
    active_by_principal: HashMap<String, usize>,
    queues: HashMap<String, VecDeque<Waiter>>,
    rr_order: VecDeque<String>,
}

/// Fair round-robin resource pool bounding both abstract units (estimated decode bytes for images,
/// slots for videos) and raw concurrent slot count.
pub(crate) struct FairPool {
    max_units: u64,
    max_slots: usize,
    starvation_age: Duration,
    max_bypasses: u32,
    next_id: AtomicU64,
    state: Mutex<PoolState>,
}

/// RAII guard holding capacity in a [`FairPool`]; dropping releases the capacity and wakes the
/// next fair waiter.
pub(crate) struct PoolPermit {
    pool: Arc<FairPool>,
    principal: String,
    cost: u64,
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
    cost: u64,
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
    pub(crate) fn new(max_units: u64, max_slots: usize) -> Arc<FairPool> {
        Self::with_aging(
            max_units,
            max_slots,
            DEFAULT_STARVATION_AGE,
            DEFAULT_MAX_BYPASSES,
        )
    }

    pub(crate) fn with_aging(
        max_units: u64,
        max_slots: usize,
        starvation_age: Duration,
        max_bypasses: u32,
    ) -> Arc<FairPool> {
        Arc::new(FairPool {
            max_units: max_units.max(1),
            max_slots: max_slots.max(1),
            starvation_age,
            max_bypasses: max_bypasses.max(1),
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
        cost: u64,
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
                    enqueued_at: Instant::now(),
                    bypasses: 0,
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

    fn release(&self, principal: &str, cost: u64) {
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

    fn cancel_or_release(&self, principal: &str, id: u64, cost: u64, granted: &AtomicBool) {
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
            let now = Instant::now();

            // Anti-starvation: find the oldest head-of-queue waiter that has been bypassed too
            // many times or has waited past `starvation_age`. If it fits now, admit it first; if
            // it cannot fit yet while other work is in flight, hold new admissions so in-flight
            // capacity drains instead of letting continuous small requests starve it forever.
            let mut oldest_starving: Option<(usize, Instant, bool)> = None;
            for (pos, principal) in state.rr_order.iter().enumerate() {
                if let Some(front) = state.queues.get(principal).and_then(VecDeque::front) {
                    let starving = front.bypasses >= self.max_bypasses
                        || now.saturating_duration_since(front.enqueued_at) >= self.starvation_age;
                    if starving {
                        let fits = state.used_units.saturating_add(front.cost) <= self.max_units
                            && state.used_slots.saturating_add(1) <= self.max_slots;
                        let better = match oldest_starving {
                            None => true,
                            Some((_, prev_t, prev_fits)) => {
                                (fits && !prev_fits)
                                    || (fits == prev_fits && front.enqueued_at < prev_t)
                            }
                        };
                        if better {
                            oldest_starving = Some((pos, front.enqueued_at, fits));
                        }
                    }
                }
            }

            let mut chosen_pos: Option<usize> = None;
            if let Some((starve_pos, _, fits)) = oldest_starving {
                if fits {
                    chosen_pos = Some(starve_pos);
                } else if state.used_units > 0 || state.used_slots > 0 {
                    // Hold admissions so active permits drain and the starving waiter can run.
                    break;
                }
            }

            let multiple_principals = num_principals > 1;
            let last_slot_left = state.used_slots.saturating_add(1) == self.max_slots;

            if chosen_pos.is_none() && multiple_principals && last_slot_left {
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

            // Charge a bypass to every earlier principal's head waiter that was skipped over so
            // large waiters age into anti-starvation priority promptly.
            for skipped_pos in 0..pos {
                if let Some(skipped_principal) = state.rr_order.get(skipped_pos)
                    && let Some(front) = state
                        .queues
                        .get_mut(skipped_principal)
                        .and_then(VecDeque::front_mut)
                {
                    front.bypasses = front.bypasses.saturating_add(1);
                }
            }

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

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn in_flight_budget_scales_with_machine_memory() {
        let cap = 256 * MIB;
        let max = 512 * MIB;
        // 64 GiB -> 512 MiB
        assert_eq!(
            compute_image_in_flight_budget(64 * 1024 * MIB, cap, max),
            max
        );
        // 8 GiB -> 1 GiB/8 capped at 512 MiB
        assert_eq!(
            compute_image_in_flight_budget(8 * 1024 * MIB, cap, max),
            max
        );
        // 4 GiB -> 512 MiB
        assert_eq!(
            compute_image_in_flight_budget(4 * 1024 * MIB, cap, max),
            max
        );
        // 3 GiB -> 384 MiB
        assert_eq!(
            compute_image_in_flight_budget(3 * 1024 * MIB, cap, max),
            384 * MIB
        );
        // 2 GiB -> 256 MiB
        assert_eq!(
            compute_image_in_flight_budget(2 * 1024 * MIB, cap, max),
            cap
        );
        // 1 GiB -> floored at per-image cap (256 MiB) so an admitted image never deadlocks
        assert_eq!(compute_image_in_flight_budget(1024 * MIB, cap, max), cap);
        // Unknown (0) -> max_budget
        assert_eq!(compute_image_in_flight_budget(0, cap, max), max);
    }

    #[tokio::test]
    async fn byte_budget_caps_in_flight_memory_and_times_out_cleanly() {
        let pool = FairPool::new(512 * MIB, 8);
        let cancel = CancellationToken::new();

        let p1 = pool
            .acquire("alice", 256 * MIB, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        let p2 = pool
            .acquire("alice", 256 * MIB, Duration::from_secs(1), &cancel)
            .await
            .unwrap();

        let err = pool
            .acquire("bob", 64 * MIB, Duration::from_millis(30), &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(err, AcquireError::TimedOut);

        drop(p1);
        let p3 = pool
            .acquire("bob", 200 * MIB, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        drop(p2);
        drop(p3);
    }

    #[tokio::test]
    async fn scheduler_never_exceeds_in_flight_byte_budget() {
        let budget = 300 * MIB;
        let pool = FairPool::new(budget, 8);
        let cancel = CancellationToken::new();
        let live = Arc::new(AtomicU64::new(0));
        let peak = Arc::new(AtomicU64::new(0));

        let costs = [
            40 * MIB,
            150 * MIB,
            220 * MIB,
            90 * MIB,
            180 * MIB,
            60 * MIB,
        ];
        let principals = ["alice", "bob", "carol"];

        let mut tasks = Vec::new();
        for i in 0..24 {
            let p = Arc::clone(&pool);
            let c = cancel.clone();
            let live_t = Arc::clone(&live);
            let peak_t = Arc::clone(&peak);
            let cost = costs[i % costs.len()];
            let who = principals[i % principals.len()];
            tasks.push(tokio::spawn(async move {
                let _permit = p
                    .acquire(who, cost, Duration::from_secs(5), &c)
                    .await
                    .unwrap();
                let now = live_t.fetch_add(cost, Ordering::SeqCst) + cost;
                peak_t.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(4)).await;
                live_t.fetch_sub(cost, Ordering::SeqCst);
            }));
        }

        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(live.load(Ordering::SeqCst), 0);
        let max_seen = peak.load(Ordering::SeqCst);
        assert!(
            max_seen <= budget,
            "scheduler admitted {max_seen} bytes > budget {budget}"
        );
        assert!(max_seen >= 220 * MIB, "expected concurrent admissions");
    }

    #[tokio::test]
    async fn round_robin_fairness_prevents_one_principal_from_starving_others() {
        let pool = FairPool::new(2, 2);
        let cancel = CancellationToken::new();

        let hold1 = pool
            .acquire("alice", 1, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        let hold2 = pool
            .acquire("alice", 1, Duration::from_secs(1), &cancel)
            .await
            .unwrap();

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
        let _reacquired = pool
            .acquire("carol", 1, Duration::from_secs(1), &hold_cancel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn aged_large_request_is_not_starved_by_continuous_small_requests() {
        let pool = FairPool::with_aging(300 * MIB, 8, Duration::from_secs(60), 2);
        let cancel = CancellationToken::new();

        // Seed 100 MiB in flight so a 250 MiB request cannot fit yet.
        let initial_small = pool
            .acquire("alice", 100 * MIB, Duration::from_secs(1), &cancel)
            .await
            .unwrap();

        let large_granted = Arc::new(AtomicBool::new(false));
        let large_pool = Arc::clone(&pool);
        let large_cancel = cancel.clone();
        let large_flag = Arc::clone(&large_granted);
        let large_task = tokio::spawn(async move {
            let _permit = large_pool
                .acquire("bob", 250 * MIB, Duration::from_secs(2), &large_cancel)
                .await
                .unwrap();
            large_flag.store(true, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(15)).await;

        // First two small requests bypass bob (hitting max_bypasses = 2).
        let s1 = pool
            .acquire("alice", 50 * MIB, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        let s2 = pool
            .acquire("alice", 50 * MIB, Duration::from_secs(1), &cancel)
            .await
            .unwrap();

        // Third small request must be held back so bob is not starved!
        let small3_done = Arc::new(AtomicBool::new(false));
        let s3_pool = Arc::clone(&pool);
        let s3_cancel = cancel.clone();
        let s3_flag = Arc::clone(&small3_done);
        let s3_task = tokio::spawn(async move {
            let _permit = s3_pool
                .acquire("carol", 50 * MIB, Duration::from_secs(2), &s3_cancel)
                .await
                .unwrap();
            s3_flag.store(true, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !small3_done.load(Ordering::SeqCst),
            "small request must not keep jumping ahead once large waiter hits anti-starvation threshold"
        );
        assert!(!large_granted.load(Ordering::SeqCst));

        // Releasing active small permits lets bob's large request run first, then carol.
        drop(initial_small);
        drop(s1);
        drop(s2);
        large_task.await.unwrap();
        assert!(large_granted.load(Ordering::SeqCst));
        s3_task.await.unwrap();
        assert!(small3_done.load(Ordering::SeqCst));
    }
}
