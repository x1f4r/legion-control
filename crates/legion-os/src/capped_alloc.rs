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

#[cold]
#[inline(never)]
fn exit_cap_exceeded() -> ! {
    #[cfg(unix)]
    unsafe {
        libc::_exit(CAP_EXCEEDED_EXIT_CODE);
    }
    #[cfg(windows)]
    unsafe {
        // Use `TerminateProcess(GetCurrentProcess(), ...)` rather than `ExitProcess` so no
        // thread-local destructors or `DllMain` hooks run on a thread that is mid-allocation.
        let _ = windows::Win32::System::Threading::TerminateProcess(
            windows::Win32::System::Threading::GetCurrentProcess(),
            CAP_EXCEEDED_EXIT_CODE as u32,
        );
        std::process::abort();
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

#[cold]
#[inline(never)]
unsafe fn alloc_capped(layout: Layout, limit: usize) -> *mut u8 {
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

#[cold]
#[inline(never)]
unsafe fn alloc_zeroed_capped(layout: Layout, limit: usize) -> *mut u8 {
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

#[cold]
#[inline(never)]
unsafe fn dealloc_capped(ptr: *mut u8, layout: Layout) {
    release(layout.size());
    unsafe { System.dealloc(ptr, layout) };
}

#[cold]
#[inline(never)]
unsafe fn realloc_capped(ptr: *mut u8, layout: Layout, new_size: usize, limit: usize) -> *mut u8 {
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

// SAFETY: Delegates all raw memory operations directly to `std::alloc::System`, preserving its
// alignment, layout, and null/error contracts, and only tracks live sizes via lock-free atomics.
// Only the single-load unarmed fast path is inlined so LTO does not bloat every allocation site.
unsafe impl GlobalAlloc for CappedAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit == 0 {
            unsafe { System.alloc(layout) }
        } else {
            unsafe { alloc_capped(layout, limit) }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit == 0 {
            unsafe { System.alloc_zeroed(layout) }
        } else {
            unsafe { alloc_zeroed_capped(layout, limit) }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if LIMIT_BYTES.load(Ordering::Relaxed) == 0 {
            unsafe { System.dealloc(ptr, layout) };
        } else {
            unsafe { dealloc_capped(ptr, layout) };
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit == 0 {
            unsafe { System.realloc(ptr, layout, new_size) }
        } else {
            unsafe { realloc_capped(ptr, layout, new_size, limit) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_alloc_zeroed_realloc_and_over_aligned_layouts_without_drift() {
        // Test CappedAlloc directly through the GlobalAlloc trait without arming the test
        // harness's own global allocator.
        let alloc = CappedAlloc::new();
        set_memory_cap(4 * 1024 * 1024);

        // 1. alloc + dealloc
        let l1 = Layout::from_size_align(1024, 8).unwrap();
        let p1 = unsafe { alloc.alloc(l1) };
        assert!(!p1.is_null());
        assert_eq!(live_bytes(), 1024);
        unsafe { alloc.dealloc(p1, l1) };
        assert_eq!(live_bytes(), 0);

        // 2. alloc_zeroed + over-aligned (align = 4096)
        let l2 = Layout::from_size_align(4096, 4096).unwrap();
        let p2 = unsafe { alloc.alloc_zeroed(l2) };
        assert!(!p2.is_null());
        assert_eq!((p2 as usize) % 4096, 0);
        assert_eq!(live_bytes(), 4096);
        let slice = unsafe { std::slice::from_raw_parts(p2, 4096) };
        assert!(slice.iter().all(|&b| b == 0));

        // 3. realloc grow + shrink (preserving over-alignment = 64)
        let l3 = Layout::from_size_align(256, 64).unwrap();
        let p3 = unsafe { alloc.alloc(l3) };
        assert!(!p3.is_null());
        assert_eq!((p3 as usize) % 64, 0);
        assert_eq!(live_bytes(), 4096 + 256);

        let p3_big = unsafe { alloc.realloc(p3, l3, 8192) };
        assert!(!p3_big.is_null());
        assert_eq!((p3_big as usize) % 64, 0);
        assert_eq!(live_bytes(), 4096 + 8192);
        let l3_big = Layout::from_size_align(8192, 64).unwrap();

        let p3_small = unsafe { alloc.realloc(p3_big, l3_big, 512) };
        assert!(!p3_small.is_null());
        assert_eq!(live_bytes(), 4096 + 512);
        let l3_small = Layout::from_size_align(512, 64).unwrap();

        unsafe {
            alloc.dealloc(p3_small, l3_small);
            alloc.dealloc(p2, l2);
        }
        assert_eq!(live_bytes(), 0);

        // 4. 10,000 sequential alloc/realloc/dealloc cycles must leave zero counter drift.
        for i in 1..=10_000usize {
            let sz = (i % 4096) + 16;
            let align = if i % 8 == 0 { 64 } else { 8 };
            let layout = Layout::from_size_align(sz, align).unwrap();
            let ptr = unsafe {
                if i % 3 == 0 {
                    alloc.alloc_zeroed(layout)
                } else {
                    alloc.alloc(layout)
                }
            };
            assert!(!ptr.is_null());
            let grown = sz + 128;
            let ptr2 = unsafe { alloc.realloc(ptr, layout, grown) };
            assert!(!ptr2.is_null());
            let layout2 = Layout::from_size_align(grown, align).unwrap();
            let shrunk = (sz / 2).max(8);
            let ptr3 = unsafe { alloc.realloc(ptr2, layout2, shrunk) };
            assert!(!ptr3.is_null());
            let layout3 = Layout::from_size_align(shrunk, align).unwrap();
            unsafe { alloc.dealloc(ptr3, layout3) };
        }
        assert_eq!(live_bytes(), 0);
        assert!(peak_bytes() >= 4096 + 8192);

        // Disarm back to 0 so sibling unit tests in the same process run unlimited.
        set_memory_cap(0);
    }
}
