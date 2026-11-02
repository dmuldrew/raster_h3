//! Counting global allocator shared by the memory benchmark suite and the
//! `profile_hotspots` example. Each binary registers it with
//! `#[global_allocator] static GLOBAL: Counting = Counting;`.
//!
//! Counts are of requested bytes, so they are the same on every platform's
//! system allocator; they are not RSS.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn on_alloc(size: usize) {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn on_dealloc(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged and only updates counters, so `System`'s guarantees carry over.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's (non-zero-size) layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            on_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's (non-zero-size) layout.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            on_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        on_dealloc(layout.size());
        // SAFETY: `ptr` was allocated by this allocator (hence `System`) with `layout`.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`/`layout` describe a live `System` allocation and
        // `new_size` is valid per the caller's contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            on_dealloc(layout.size());
            on_alloc(new_size);
        }
        new
    }
}

/// Counter readings at one instant.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub allocs: u64,
    pub alloc_bytes: u64,
    pub live: usize,
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        allocs: ALLOCS.load(Ordering::Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        live: LIVE.load(Ordering::Relaxed),
    }
}

/// Currently live bytes.
pub fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Restart peak tracking from the current live bytes; returns them.
pub fn reset_peak() -> usize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

/// Highest live bytes since the last [`reset_peak`].
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Allocations, bytes allocated and peak extra live bytes over one region.
#[derive(Clone, Copy, Debug, Default)]
pub struct Region {
    pub allocs: u64,
    pub alloc_bytes: u64,
    /// Peak live bytes above the live bytes at the start of the region.
    pub peak_extra: usize,
    /// Live bytes at the end minus live bytes at the start (saturating).
    pub retained: usize,
}

/// Measure `f` as one region. Not reentrant: nested calls reset the peak.
pub fn region<T>(f: impl FnOnce() -> T) -> (T, Region) {
    let start = snapshot();
    let base = reset_peak();
    let out = f();
    let end = snapshot();
    let region = Region {
        allocs: end.allocs - start.allocs,
        alloc_bytes: end.alloc_bytes - start.alloc_bytes,
        peak_extra: peak().saturating_sub(base),
        retained: end.live.saturating_sub(start.live),
    };
    (out, region)
}
