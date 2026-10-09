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
static ALLOCATOR: block_cache::BlockCache = block_cache::BlockCache;

/// wasm32's system allocator is one dlmalloc behind one global spin lock, so threads that allocate
/// in a hot loop serialize on it (past about 8 threads the proof got slower, not faster). This
/// keeps freed blocks of up to 4 KiB in sharded free lists and hands them back without that lock.
///
/// A shard is picked from the stack address, not thread-local storage: wasm-bindgen allocates a
/// new thread's stack and TLS block with this allocator before the thread's TLS exists. Blocks are
/// rounded up to size classes (at most 25% over) and come from a heap of their own: cached blocks
/// scattered through the main heap would keep its free space from coalescing for the large
/// buffers (+0.3 to +1 GiB of wasm memory when they shared it). Both heaps count in the live and
/// peak figures.
#[cfg(target_arch = "wasm32")]
mod block_cache {
    use std::alloc::{GlobalAlloc, Layout};
    use std::cell::UnsafeCell;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use dlmalloc::Dlmalloc;

    use super::{CountingAllocator, LIVE, grew};

    const SHARDS: usize = 64;
    /// Shards tried when the first is locked by another thread.
    const PROBES: usize = 4;
    const MAX_SIZE: usize = 4 << 10;
    /// Freed bytes one shard may hold; past it blocks go back to the small heap.
    const SHARD_BYTES: usize = 1 << 20;
    /// 16, 32, 48, 64, then four classes per power of two up to `MAX_SIZE`.
    const N_CLASSES: usize = 4 + (MAX_SIZE.trailing_zeros() as usize - 6) * 4;
    /// Alignments up to 8 (dlmalloc's own) and up to 64 (`u32x16`); larger ones bypass the cache.
    const FAMILY_ALIGN: [usize; 2] = [8, 64];
    const PAGE: usize = 64 << 10;
    /// The small heap grows by this much at a time, to keep its segments few and large.
    const SEGMENT: usize = 4 << 20;

    /// Memory for the small heap: `memory.grow`, as dlmalloc's wasm backend, but in large steps.
    struct Segments;

    unsafe impl dlmalloc::Allocator for Segments {
        fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
            let pages = size.max(SEGMENT).div_ceil(PAGE);
            let previous = core::arch::wasm32::memory_grow::<0>(pages);
            if previous == usize::MAX {
                return (ptr::null_mut(), 0, 0);
            }
            let base = previous * PAGE;
            let mut size = pages * PAGE;
            // A segment ending at the top of the address space would make a pointer wrap to 0.
            if base.wrapping_add(size) == 0 {
                size -= 16;
            }
            (base as *mut u8, size, 0)
        }
        fn remap(&self, _: *mut u8, _: usize, _: usize, _: bool) -> *mut u8 {
            ptr::null_mut()
        }
        fn free_part(&self, _: *mut u8, _: usize, _: usize) -> bool {
            false
        }
        fn free(&self, _: *mut u8, _: usize) -> bool {
            false
        }
        fn can_release_part(&self, _: u32) -> bool {
            false
        }
        fn allocates_zeros(&self) -> bool {
            true
        }
        fn page_size(&self) -> usize {
            PAGE
        }
    }

    struct SmallHeap {
        locked: AtomicBool,
        heap: UnsafeCell<Dlmalloc<Segments>>,
    }

    unsafe impl Sync for SmallHeap {}

    static SMALL: SmallHeap = SmallHeap {
        locked: AtomicBool::new(false),
        heap: UnsafeCell::new(Dlmalloc::new_with_allocator(Segments)),
    };

    fn with_small_heap<R>(f: impl FnOnce(&mut Dlmalloc<Segments>) -> R) -> R {
        while SMALL
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        let result = f(unsafe { &mut *SMALL.heap.get() });
        SMALL.locked.store(false, Ordering::Release);
        result
    }

    unsafe fn small_alloc(family: usize, class_size: usize, zeroed: bool) -> *mut u8 {
        let pointer = with_small_heap(|heap| unsafe {
            if zeroed {
                heap.calloc(class_size, FAMILY_ALIGN[family])
            } else {
                heap.malloc(class_size, FAMILY_ALIGN[family])
            }
        });
        if !pointer.is_null() {
            grew(class_size);
        }
        pointer
    }

    unsafe fn small_free(pointer: *mut u8, family: usize, class_size: usize) {
        with_small_heap(|heap| unsafe { heap.free(pointer, class_size, FAMILY_ALIGN[family]) });
        LIVE.fetch_sub(class_size, std::sync::atomic::Ordering::Relaxed);
    }

    struct Shard {
        locked: AtomicBool,
        lists: UnsafeCell<Lists>,
    }

    struct Lists {
        heads: [[*mut u8; N_CLASSES]; 2],
        bytes: usize,
    }

    unsafe impl Sync for Shard {}

    static CACHE: [Shard; SHARDS] = [const {
        Shard {
            locked: AtomicBool::new(false),
            lists: UnsafeCell::new(Lists {
                heads: [[ptr::null_mut(); N_CLASSES]; 2],
                bytes: 0,
            }),
        }
    }; SHARDS];

    /// The class index and size for `size`, if `layout` is cacheable.
    fn class(layout: Layout) -> Option<(usize, usize, usize)> {
        let family = match layout.align() {
            0..=8 => 0,
            9..=64 => 1,
            _ => return None,
        };
        let size = layout.size().max(1);
        if size > MAX_SIZE {
            return None;
        }
        if size <= 64 {
            let k = size.div_ceil(16);
            return Some((family, k - 1, k * 16));
        }
        // 2^e < size <= 2^(e + 1), e >= 6: four classes of 2^(e - 2) each.
        let e = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize;
        let step = 1 << (e - 2);
        let k = size.div_ceil(step);
        Some((family, 4 + (e - 6) * 4 + (k - 5), k * step))
    }

    /// The shard of the calling thread's stack (thread stacks are at least 1 MiB apart).
    fn home() -> usize {
        let marker = 0u8;
        let address = std::hint::black_box(&marker) as *const u8 as usize;
        (((address >> 20) as u32).wrapping_mul(0x9E37_79B9) >> 26) as usize % SHARDS
    }

    /// Runs `f` on the first shard from home that is not locked, or returns None.
    fn with_shard<R>(f: impl FnOnce(&mut Lists) -> R) -> Option<R> {
        let home = home();
        for probe in 0..PROBES {
            let shard = &CACHE[(home + probe) % SHARDS];
            if shard
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let result = f(unsafe { &mut *shard.lists.get() });
                shard.locked.store(false, Ordering::Release);
                return Some(result);
            }
        }
        None
    }

    pub struct BlockCache;

    impl BlockCache {
        unsafe fn take(family: usize, class: usize, class_size: usize) -> *mut u8 {
            with_shard(|lists| {
                let head = lists.heads[family][class];
                if !head.is_null() {
                    lists.heads[family][class] = unsafe { *(head as *mut *mut u8) };
                    lists.bytes -= class_size;
                }
                head
            })
            .unwrap_or(ptr::null_mut())
        }

        unsafe fn put(pointer: *mut u8, family: usize, class: usize, class_size: usize) -> bool {
            with_shard(|lists| {
                if lists.bytes + class_size > SHARD_BYTES {
                    return false;
                }
                unsafe { *(pointer as *mut *mut u8) = lists.heads[family][class] };
                lists.heads[family][class] = pointer;
                lists.bytes += class_size;
                true
            })
            .unwrap_or(false)
        }
    }

    unsafe impl GlobalAlloc for BlockCache {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let Some((family, class, class_size)) = class(layout) else {
                return unsafe { CountingAllocator.alloc(layout) };
            };
            let cached = unsafe { Self::take(family, class, class_size) };
            if !cached.is_null() {
                return cached;
            }
            unsafe { small_alloc(family, class_size, false) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let Some((family, class, class_size)) = class(layout) else {
                return unsafe { CountingAllocator.alloc_zeroed(layout) };
            };
            let cached = unsafe { Self::take(family, class, class_size) };
            if !cached.is_null() {
                unsafe { ptr::write_bytes(cached, 0, layout.size()) };
                return cached;
            }
            unsafe { small_alloc(family, class_size, true) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            let Some((family, class, class_size)) = class(layout) else {
                return unsafe { CountingAllocator.dealloc(pointer, layout) };
            };
            if unsafe { !Self::put(pointer, family, class, class_size) } {
                unsafe { small_free(pointer, family, class_size) }
            }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            match (class(layout), class(new_layout)) {
                (None, None) => unsafe { CountingAllocator.realloc(pointer, layout, new_size) },
                (Some((_, old, _)), Some((_, new, _))) if old == new => pointer,
                _ => unsafe {
                    let new_pointer = self.alloc(new_layout);
                    if !new_pointer.is_null() {
                        ptr::copy_nonoverlapping(pointer, new_pointer, layout.size().min(new_size));
                        self.dealloc(pointer, layout);
                    }
                    new_pointer
                },
            }
        }
    }
}

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
