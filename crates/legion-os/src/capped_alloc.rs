//! Process-wide global allocator wrapper with an opt-in, exact live-heap cap.
//!
//! In normal Legion operation (`cap == 0`), every allocation pays only a single relaxed atomic
//! load before delegating straight to [`std::alloc::System`] — no read-modify-write atomic
//! instructions and no cross-core cache-line contention.
//!
//! When the hidden `__thumb-worker` subcommand starts, its first act is calling
//! [`set_memory_cap`]. From that point on, every allocation and growth updates the live byte
//! counter and immediately terminates the worker with [`CAP_EXCEEDED_EXIT_CODE`] if the live heap
//! would exceed the cap — giving every platform (including macOS) an exact, zero-sampling-gap
//! memory bound that the parent process distinguishes unambiguously from external signals.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Distinct process exit code emitted by [`CappedAlloc`] when an allocation would exceed the
/// configured heap cap.
///
/// Chosen outside common shell/signal codes so the parent unambiguously recognizes "this file
/// exceeded its allowed memory limit" rather than an external kill or crash.
pub const CAP_EXCEEDED_EXIT_CODE: i32 = 107;

static LIMIT_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Global allocator delegating to [`std::alloc::System`] with an opt-in exact live-byte cap.
pub struct CappedAlloc;

impl CappedAlloc {
    /// Creates a new `CappedAlloc` instance suitable for `#[global_allocator]`.
    pub const fn new() -> Self {
        Self
    }
}

impl Default for CappedAlloc {
    fn default() -> Self {
        Self::new()
    }
}

/// Arms the process-wide heap cap at `bytes` (`0` leaves the allocator unlimited).
///
/// Intended to be called once at worker startup before any untrusted input is read.
pub fn set_memory_cap(bytes: usize) {
    LIVE_BYTES.store(0, Ordering::Relaxed);
    PEAK_BYTES.store(0, Ordering::Relaxed);
    LIMIT_BYTES.store(bytes, Ordering::Release);
}

/// Returns the currently configured heap cap in bytes (`0` when unlimited).
pub fn memory_cap() -> usize {
    LIMIT_BYTES.load(Ordering::Acquire)
}

/// Returns the current tracked live heap bytes (tracked only while a cap is active).
pub fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// Returns the peak tracked live heap bytes observed since [`set_memory_cap`] was called.
pub fn peak_bytes() -> usize {
    PEAK_BYTES.load(Ordering::Relaxed)
}

#[inline]
fn exit_cap_exceeded() -> ! {
    #[cfg(unix)]
    unsafe {
        libc::_exit(CAP_EXCEEDED_EXIT_CODE);
    }
    #[cfg(windows)]
    unsafe {
        windows::Win32::System::Threading::ExitProcess(CAP_EXCEEDED_EXIT_CODE as u32);
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::process::exit(CAP_EXCEEDED_EXIT_CODE);
    }
}

#[inline]
fn try_reserve(add: usize, limit: usize) -> bool {
    let mut cur = LIVE_BYTES.load(Ordering::Relaxed);
    loop {
        let Some(next) = cur.checked_add(add) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match LIVE_BYTES.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                PEAK_BYTES.fetch_max(next, Ordering::Relaxed);
                return true;
            }
            Err(observed) => cur = observed,
        }
    }
}

#[inline]
fn release(sub: usize) {
    // Saturating subtraction prevents any small pre-cap startup allocation freed after
    // `set_memory_cap` from underflowing `LIVE_BYTES`.
    let mut cur = LIVE_BYTES.load(Ordering::Relaxed);
    loop {
        let next = cur.saturating_sub(sub);
        match LIVE_BYTES.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => cur = observed,
        }
    }
}

// SAFETY: Delegates all raw memory operations directly to `std::alloc::System`, preserving its
// alignment, layout, and null/error contracts, and only tracks live sizes via lock-free atomics.
unsafe impl GlobalAlloc for CappedAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit == 0 {
            return unsafe { System.alloc(layout) };
        }
        let size = layout.size();
        if !try_reserve(size, limit) {
            exit_cap_exceeded();
        }
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            release(size);
            exit_cap_exceeded();
        }
        ptr
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit == 0 {
            return unsafe { System.alloc_zeroed(layout) };
        }
        let size = layout.size();
        if !try_reserve(size, limit) {
            exit_cap_exceeded();
        }
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            release(size);
            exit_cap_exceeded();
        }
        ptr
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if LIMIT_BYTES.load(Ordering::Relaxed) != 0 {
            release(layout.size());
        }
        unsafe { System.dealloc(ptr, layout) };
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit == 0 {
            return unsafe { System.realloc(ptr, layout, new_size) };
        }
        let old_size = layout.size();
        if new_size > old_size {
            let delta = new_size - old_size;
            if !try_reserve(delta, limit) {
                exit_cap_exceeded();
            }
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if new_ptr.is_null() {
                release(delta);
                exit_cap_exceeded();
            }
            new_ptr
        } else {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if new_ptr.is_null() {
                exit_cap_exceeded();
            }
            release(old_size - new_size);
            new_ptr
        }
    }
}
