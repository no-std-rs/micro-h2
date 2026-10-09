//! Single-threaded benchmark accounting. The host's existing allocations are
//! outside the workload budget; all allocations made during the workload count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub struct Meter;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

fn reserve(bytes: usize) -> bool {
    LIVE.fetch_update(Relaxed, Relaxed, |live| {
        live.checked_add(bytes)
            .filter(|next| *next <= LIMIT.load(Relaxed))
    })
    .is_ok()
}

fn allocated() {
    CALLS.fetch_add(1, Relaxed);
    PEAK.fetch_max(LIVE.load(Relaxed), Relaxed);
}

// SAFETY: Every allocation/deallocation is forwarded unchanged to System.
// Accounting uses only atomics and never allocates or unwinds. Returning null
// when the workload budget is exhausted follows GlobalAlloc's failure contract.
// Unsafe code exists only in this std benchmark, not in the library.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return null_mut();
        }
        // SAFETY: The caller supplied a valid allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
        } else {
            allocated();
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return null_mut();
        }
        // SAFETY: The caller supplied a valid allocation layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
        } else {
            allocated();
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: Pointer and layout are forwarded from GlobalAlloc's caller.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let growth = new_size.saturating_sub(layout.size());
        if !reserve(growth) {
            return null_mut();
        }
        // SAFETY: Pointer, old layout, and new size are forwarded unchanged.
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if replacement.is_null() {
            LIVE.fetch_sub(growth, Relaxed);
        } else {
            LIVE.fetch_sub(layout.size().saturating_sub(new_size), Relaxed);
            allocated();
        }
        replacement
    }
}

pub struct Budget {
    baseline: usize,
    calls: usize,
}

pub struct Usage {
    pub peak: usize,
    pub allocations: usize,
}

impl Budget {
    pub fn start(bytes: usize) -> Self {
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        LIMIT.store(baseline.checked_add(bytes).unwrap(), Relaxed);
        Self {
            baseline,
            calls: CALLS.load(Relaxed),
        }
    }

    pub fn finish(self) -> Usage {
        let usage = Usage {
            peak: PEAK.load(Relaxed) - self.baseline,
            allocations: CALLS.load(Relaxed) - self.calls,
        };
        assert_eq!(LIVE.load(Relaxed), self.baseline, "workload leaked memory");
        usage
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        LIMIT.store(usize::MAX, Relaxed);
    }
}

/// Check the measuring instrument before trusting its workload results.
pub fn verify_meter() {
    // Call the instrument directly: std::alloc's wrappers permit the compiler
    // to elide allocations or assume they succeed, invalidating rejection tests.
    let meter = Meter;
    let budget = Budget::start(64);
    let layout = Layout::from_size_align(32, 8).unwrap();
    // SAFETY: These allocations use valid nonzero layouts. Each successful
    // allocation is checked before access and freed with its current layout.
    // A failed realloc preserves the original allocation.
    unsafe {
        let first = meter.alloc(layout);
        let second = meter.alloc_zeroed(layout);
        assert!(!first.is_null() && !second.is_null());
        assert_eq!(*second, 0);
        assert!(
            meter
                .alloc(Layout::from_size_align(1, 1).unwrap())
                .is_null()
        );
        assert!(meter.realloc(first, layout, 33).is_null());
        meter.dealloc(second, layout);
        let grown = meter.realloc(first, layout, 48);
        assert!(!grown.is_null());
        let shrunk = meter.realloc(grown, Layout::from_size_align(48, 8).unwrap(), 16);
        assert!(!shrunk.is_null());
        meter.dealloc(shrunk, Layout::from_size_align(16, 8).unwrap());
    }
    let usage = budget.finish();
    assert_eq!(usage.peak, 64);
    assert_eq!(usage.allocations, 4);
}
