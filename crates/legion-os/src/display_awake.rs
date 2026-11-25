//! Keeps the display awake on Windows via a dedicated app-owned thread calling
//! `SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED)`.
//!
//! Because `SetThreadExecutionState` is per-thread state on Windows, every acquire and release must
//! run on the same thread; the OS also clears the thread's execution state automatically if the
//! process terminates for any reason.

use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

enum Msg {
    Set(bool),
    Shutdown,
}

/// Owns a dedicated worker thread that applies display-awake state changes on one consistent
/// thread and guarantees the hold is cleared on release or drop.
pub struct DisplayAwakeHold {
    tx: Option<Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
    active: bool,
}

impl DisplayAwakeHold {
    /// Starts a new hold backed by the platform display-awake primitive (Windows
    /// `SetThreadExecutionState`; no-op on other platforms).
    pub fn new() -> Self {
        Self::with_applier(apply_platform_display_awake)
    }

    /// Starts a new hold backed by a custom state applier, allowing deterministic unit testing of
    /// single-thread affinity, idempotency, and drop cleanup on every OS.
    pub fn with_applier<F>(mut apply: F) -> Self
    where
        F: FnMut(bool) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Msg>();
        let handle = thread::Builder::new()
            .name("legion-display-awake".to_owned())
            .spawn(move || {
                let mut held = false;
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Set(on) => {
                            if on != held {
                                apply(on);
                                held = on;
                            }
                        }
                        Msg::Shutdown => break,
                    }
                }
                if held {
                    apply(false);
                }
            })
            .ok();
        let tx = handle.as_ref().map(|_| tx);
        Self {
            tx,
            handle,
            active: false,
        }
    }

    /// Enables or clears the display-awake hold.
    pub fn set(&mut self, on: bool) {
        if self.active == on {
            return;
        }
        if let Some(tx) = &self.tx
            && tx.send(Msg::Set(on)).is_ok()
        {
            self.active = on;
        }
    }

    /// Reports whether the hold is currently active.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Default for DisplayAwakeHold {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DisplayAwakeHold {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Shutdown);
        }
        self.active = false;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn apply_platform_display_awake(on: bool) {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, SetThreadExecutionState,
    };

    let flags = if on {
        ES_CONTINUOUS | ES_DISPLAY_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    // SAFETY: `SetThreadExecutionState` takes only a bitmask for the calling thread with no
    // pointers, lifetimes, or aliasing invariants.
    let _ = unsafe { SetThreadExecutionState(flags) };
}

#[cfg(not(windows))]
fn apply_platform_display_awake(_on: bool) {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;

    use super::*;

    #[test]
    fn display_awake_hold_uses_one_thread_is_idempotent_and_clears_on_drop() {
        let events: Arc<Mutex<Vec<(ThreadId, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        {
            let mut hold = DisplayAwakeHold::with_applier(move |on| {
                recorded
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((thread::current().id(), on));
            });
            assert!(!hold.is_active());
            hold.set(true);
            hold.set(true); // idempotent
            assert!(hold.is_active());
            hold.set(false);
            hold.set(false); // idempotent
            assert!(!hold.is_active());
            hold.set(true);
            assert!(hold.is_active());
            // Drop while still active -> worker must clear with `false` before exiting.
        }

        let log = events.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            log.len(),
            4,
            "expected [true, false, true, false], got {log:?}"
        );
        assert_eq!(
            log.iter().map(|(_, on)| *on).collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
        let worker_tid = log[0].0;
        assert_ne!(worker_tid, thread::current().id());
        assert!(
            log.iter().all(|(tid, _)| *tid == worker_tid),
            "every SetThreadExecutionState transition must happen on the same owned thread"
        );
    }
}
