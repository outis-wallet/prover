//! Heap accounting for memory measurements: a counting wrapper around the system allocator
//! (dlmalloc on wasm32) and the wasm memory size, which only grows.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK_LIVE: AtomicUsize = AtomicUsize::new(0);

pub struct CountingAllocator;

fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_LIVE.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grew(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            grew(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_pointer
    }
}

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const MIB: usize = 1 << 20;

/// `wasm <memory size> live <live heap> peak <peak live heap>`, in MiB.
pub fn report() -> String {
    #[cfg(target_arch = "wasm32")]
    let wasm_bytes = core::arch::wasm32::memory_size::<0>() * 65536;
    #[cfg(not(target_arch = "wasm32"))]
    let wasm_bytes = 0;
    format!(
        "wasm {} live {} peak {}",
        wasm_bytes / MIB,
        LIVE.load(Ordering::Relaxed) / MIB,
        PEAK_LIVE.load(Ordering::Relaxed) / MIB
    )
}
