//! Slot `alloc-region`: the process's allocator. One stretch of address space reserved at the
//! first allocation, handed out by moving a mark and never returned to the kernel.
//!
//! Why not glibc's. A run is a few tens of milliseconds at 1M events, and what glibc does for a
//! new thread or a large request is `mmap`, `munmap` and `mprotect`: each takes the process's
//! address-space lock for writing. A page fault on fresh memory holds that lock for reading
//! while the kernel zeroes the page, 2 MB of it with huge pages, and a waiting writer makes
//! every later reader queue behind it. So thread starts, heap growth and first touches took
//! turns: eight workers needed 3 ms to come up and build their empty lists (the timeline of
//! `SS_TIMING=2`), and without the `GLIBC_TUNABLES` the image set, the whole run took twice as
//! long. Here the only `mmap` is the reservation, before any thread exists; after it an
//! allocation moves the thread's own mark or pops the thread's own list.
//!
//! - Each thread takes 64 MB of the reservation at a time (`CHUNK`) and gives out blocks from
//!   it one after another, so what a thread allocates lies together and fills its huge pages:
//!   a first write zeroes 2 MB, and blocks spread over the address space made that 45% more
//!   memory to zero at 1M events than glibc's arenas did.
//! - Up to 64 KB a block's size is a power of two, at least 16 bytes; freed, it goes on the
//!   freeing thread's list for that size and is the next block of that size the thread gives
//!   out. These come from the reservation's first gigabyte, which is kept on 4 KB pages: a
//!   thread's first few bytes then cost one small page, not 2 MB zeroed under the
//!   address-space lock while the other threads are starting (0.3 ms a thread, one after
//!   another, when they did).
//! - A larger block is whole 4 KB pages. Freed, it waits in one of a few slots for its size
//!   (nothing is written into it, so one that was never touched stays untouched) and is given
//!   out again to a request it holds. Over 16 MB it comes from the reservation directly, as
//!   whole huge pages.
//! - A block that grows (`realloc`) stays where it is when it is the last one its thread gave
//!   out; otherwise it moves.
//! - `alloc_zeroed` clears only a block that was used before: the reservation's pages are the
//!   kernel's zero pages until written, as `calloc`'s are.
//!
//! The reservation is `MAP_NORESERVE`, so it counts against nothing until used, and asks for
//! transparent huge pages. If it cannot be made, or is used up, a request goes to the system
//! allocator. Linux only; elsewhere this is the system allocator.

pub struct Region;

/// The stretch of the reservation that every block larger than 64 KB came from so far: its
/// first byte and one past the last byte given out. `None` when there is no reservation.
/// (Small blocks, the thread pool's among them, are in the gigabyte before it.)
pub fn large_used() -> Option<(usize, usize)> {
    #[cfg(target_os = "linux")]
    {
        linux::large_used()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use std::alloc::{GlobalAlloc, Layout, System};

    // SAFETY: the system allocator's contract, passed through.
    unsafe impl GlobalAlloc for super::Region {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::UnsafeCell;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::Relaxed, Ordering::SeqCst};

    use super::Region;

    /// Address space asked for, in turn, until one is granted.
    const RESERVES: [usize; 3] = [64 << 30, 16 << 30, 4 << 30];
    const HUGE: usize = 2 << 20;
    const PAGE: usize = 4096;
    /// The reservation's first bytes, kept on 4 KB pages, for small blocks; and what a thread
    /// takes of them at a time.
    const SMALL_ZONE: usize = 1 << 30;
    const SMALL_CHUNK: usize = 1 << 20;
    /// What a thread takes of the rest at a time.
    const CHUNK: usize = 64 << 20;
    /// The largest block given out of a thread's chunk; a larger one is the reservation's.
    const MOST: usize = CHUNK / 4;
    /// log2 of the smallest block, and of the largest that is a power of two and listed by
    /// the thread that frees it.
    const MIN: u32 = 4;
    const SMALL: u32 = 16;
    /// Freed large blocks kept per size; one more is not reused.
    const SLOTS: usize = 24;
    /// Sizes up to 2^47.
    const SIZES: usize = 48;

    const NEW: u8 = 0;
    const SETTING_UP: u8 = 1;
    const READY: u8 = 2;
    const SYSTEM: u8 = 3;
    static STATE: AtomicU8 = AtomicU8::new(NEW);
    /// The reservation: its first byte, the next free one past the small blocks' zone, one
    /// past its last; and the next free byte of that zone.
    static BASE: AtomicUsize = AtomicUsize::new(0);
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static END: AtomicUsize = AtomicUsize::new(0);
    static SMALL_NEXT: AtomicUsize = AtomicUsize::new(0);
    /// Freed large blocks: slot row `n` holds blocks of at least `1 << n` bytes; 0 is a free
    /// slot.
    static FREED: [[AtomicUsize; SLOTS]; SIZES] = [const { [const { AtomicUsize::new(0) }; SLOTS] }; SIZES];

    /// A thread's own: what is left of its chunk and of its chunk for small blocks, and its
    /// freed small blocks by size, each holding the address of the next in its first word.
    struct Own {
        at: usize,
        end: usize,
        /// `SS_COLLAPSE=all`: the chunk is on 2 MB pages up to here, never below `at`
        /// (`back_chunk`); `usize::MAX` when chunks are left to their first writes.
        backed: usize,
        small_at: usize,
        small_end: usize,
        freed: [usize; (SMALL - MIN + 1) as usize],
    }

    thread_local! {
        // No destructor and a constant start: usable from a thread's first allocation to its
        // last, and never itself a reason to allocate.
        static OWN: UnsafeCell<Own> = const { UnsafeCell::new(Own { at: 0, end: 0, backed: 0, small_at: 0, small_end: 0, freed: [0; (SMALL - MIN + 1) as usize] }) };
    }

    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
        fn getenv(name: *const u8) -> *const u8;
    }
    const PROT_READ_WRITE: i32 = 1 | 2;
    const MAP_PRIVATE_ANONYMOUS_NORESERVE: i32 = 0x02 | 0x20 | 0x4000;
    const MADV_HUGEPAGE: i32 = 14;
    const MADV_NOHUGEPAGE: i32 = 15;
    const MADV_POPULATE_WRITE: i32 = 23;
    /// `SS_POPULATE` is set and not 0 (`types::mem::populate_window`): a block that moves to
    /// a larger one has the pages its bytes go to backed in one call first. Cleared when the
    /// kernel does not know the call.
    static POPULATE: AtomicBool = AtomicBool::new(false);
    /// The smallest block that is worth the call.
    const POPULATE_FROM: usize = 64 << 10;
    const MADV_COLLAPSE: i32 = 25;
    /// `SS_COLLAPSE` (`types::mem`): 1 for `all`, 2 for `emulate`, else 0. With either, a
    /// thread's chunk is put onto 2 MB pages as its mark passes each 2 MB (`back`).
    static COLLAPSE: AtomicU8 = AtomicU8::new(0);
    /// Collapses the kernel refused in a row; at 8 it is not asked again.
    static REFUSALS: AtomicU8 = AtomicU8::new(0);

    pub(super) fn large_used() -> Option<(usize, usize)> {
        let base = BASE.load(Relaxed);
        (base != 0).then(|| (base + SMALL_ZONE, NEXT.load(Relaxed)))
    }

    /// Makes the reservation, once. Returns whether there is one.
    #[cold]
    #[inline(never)]
    fn set_up() -> bool {
        loop {
            match STATE.compare_exchange(NEW, SETTING_UP, SeqCst, SeqCst) {
                Ok(_) => break,
                Err(READY) => return true,
                Err(SYSTEM) => return false,
                // Another thread is at it (in practice the first allocation comes long before
                // a second thread).
                Err(_) => std::hint::spin_loop(),
            }
        }
        for bytes in RESERVES {
            // SAFETY: a fresh anonymous mapping, nothing of the process's is touched.
            let got = unsafe { mmap(std::ptr::null_mut(), bytes, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS_NORESERVE, -1, 0) };
            if got as isize == -1 || got.is_null() {
                continue;
            }
            // Whole huge pages only.
            let (first, last) = ((got as usize).next_multiple_of(HUGE), (got as usize + bytes) & !(HUGE - 1));
            // `SS_HUGE=advise|default|never`, by its first letter, read from the C library:
            // nothing may allocate here. `advise` (the pick) asks for huge pages; where the
            // machine's `defrag` is `madvise` (most distributions' default) a fault in an
            // advised range that finds no free 2 MB compacts memory itself, on the faulting
            // thread, before it returns. `default` leaves the range to the machine's setting
            // (huge pages only where it says `always`, and never that stall); `never` is
            // 4 KB pages.
            // SAFETY: a terminated name; the value, when there is one, has a first byte.
            let mode = unsafe {
                let value = getenv(c"SS_HUGE".as_ptr().cast());
                if value.is_null() { b'a' } else { *value }
            };
            // SAFETY: as above.
            let populate = unsafe {
                let value = getenv(c"SS_POPULATE".as_ptr().cast());
                !value.is_null() && *value != 0 && !(*value == b'0' && *value.add(1) == 0)
            };
            POPULATE.store(populate, SeqCst);
            // SAFETY: as above; the value is compared up to its terminator.
            let collapse = unsafe {
                let value = getenv(c"SS_COLLAPSE".as_ptr().cast());
                let is = |word: &core::ffi::CStr| !value.is_null() && core::ffi::CStr::from_ptr(value.cast()) == word;
                if is(c"all") {
                    1
                } else if is(c"emulate") {
                    2
                } else {
                    0
                }
            };
            COLLAPSE.store(collapse, SeqCst);
            // SAFETY: inside the mapping. A kernel without huge pages says no to both: 4 KB
            // pages everywhere then.
            unsafe {
                madvise(first as *mut u8, SMALL_ZONE, MADV_NOHUGEPAGE);
                match mode {
                    b'd' => {}
                    b'n' => drop(madvise((first + SMALL_ZONE) as *mut u8, last - first - SMALL_ZONE, MADV_NOHUGEPAGE)),
                    _ => drop(madvise((first + SMALL_ZONE) as *mut u8, last - first - SMALL_ZONE, MADV_HUGEPAGE)),
                }
            }
            BASE.store(first, SeqCst);
            SMALL_NEXT.store(first, SeqCst);
            NEXT.store(first + SMALL_ZONE, SeqCst);
            END.store(last, SeqCst);
            STATE.store(READY, SeqCst);
            return true;
        }
        STATE.store(SYSTEM, SeqCst);
        false
    }

    #[inline(always)]
    fn ready() -> bool {
        match STATE.load(Relaxed) {
            READY => true,
            SYSTEM => false,
            _ => set_up(),
        }
    }

    /// Whether `ptr` is a block of the reservation (and not the system allocator's).
    #[inline(always)]
    fn ours(ptr: *mut u8) -> bool {
        let (at, base) = (ptr as usize, BASE.load(Relaxed));
        base != 0 && at >= base && at < END.load(Relaxed)
    }

    /// `bytes` of the reservation, aligned to `align` (a power of two); null when it is used
    /// up.
    fn take(bytes: usize, align: usize) -> *mut u8 {
        let mut next = NEXT.load(Relaxed);
        loop {
            let start = (next + align - 1) & !(align - 1);
            let end = match start.checked_add(bytes) {
                Some(end) if end <= END.load(Relaxed) => end,
                _ => return std::ptr::null_mut(),
            };
            match NEXT.compare_exchange_weak(next, end, SeqCst, Relaxed) {
                Ok(_) => return start as *mut u8,
                Err(now) => next = now,
            }
        }
    }

    /// `bytes` from the thread's chunk, aligned to `align`, from a new chunk when this one
    /// has no room; null when the reservation is used up. `bytes <= MOST`, `align <= HUGE`.
    #[inline(always)]
    fn give(own: &mut Own, bytes: usize, align: usize) -> *mut u8 {
        let start = (own.at + align - 1) & !(align - 1);
        if start + bytes <= own.end {
            own.at = start + bytes;
            if own.at > own.backed {
                back_chunk(own);
            }
            return start as *mut u8;
        }
        fresh(own, bytes)
    }

    /// `SS_COLLAPSE=all`: the whole 2 MB steps of `from..from + len` (both on such a step)
    /// onto 2 MB pages: a byte written into each, then `MADV_COLLAPSE`, which makes a 2 MB
    /// page of a step that has one page (Linux 6.1; it does not ask the machine's
    /// huge-page setting). `emulate`: the range marked `MADV_HUGEPAGE` first, so that the
    /// write itself gets the page where the machine gives them, and no collapse.
    ///
    /// The bytes written are zeros, into memory no caller has yet (or has only just been
    /// given): what `alloc_zeroed` counts on stays true.
    fn back(from: usize, len: usize) {
        let mode = COLLAPSE.load(Relaxed);
        if mode == 0 || len == 0 {
            return;
        }
        // SAFETY: inside the reservation, above every block in use.
        unsafe {
            if mode == 2 {
                madvise(from as *mut u8, len, MADV_HUGEPAGE);
            }
            let mut step = from;
            while step < from + len {
                (step as *mut u8).write_volatile(0);
                step += HUGE;
            }
            if mode == 1 && REFUSALS.load(Relaxed) < 8 {
                if madvise(from as *mut u8, len, MADV_COLLAPSE) == 0 {
                    REFUSALS.store(0, Relaxed);
                } else {
                    REFUSALS.fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// The thread's chunk onto 2 MB pages up to the 2 MB step at or above its mark.
    #[cold]
    #[inline(never)]
    fn back_chunk(own: &mut Own) {
        let to = ((own.at + HUGE - 1) & !(HUGE - 1)).min(own.end);
        let from = own.backed;
        own.backed = to;
        back(from, to - from);
    }

    #[cold]
    #[inline(never)]
    fn fresh(own: &mut Own, bytes: usize) -> *mut u8 {
        // What is left of the old chunk was never touched: it costs nothing.
        let chunk = take(CHUNK, HUGE);
        if !chunk.is_null() {
            (own.at, own.end) = (chunk as usize + bytes, chunk as usize + CHUNK);
            own.backed = if COLLAPSE.load(Relaxed) != 0 { chunk as usize } else { usize::MAX };
            if own.at > own.backed {
                back_chunk(own);
            }
        }
        chunk
    }

    /// A small block of `bytes` (a power of two) from the thread's small chunk, from a new
    /// one when this one has no room, and from the thread's large chunk once the zone is
    /// used up.
    #[inline(always)]
    fn give_small(own: &mut Own, bytes: usize) -> *mut u8 {
        // Aligned to its size up to a page: at least the request's alignment.
        let align = bytes.min(PAGE);
        let start = (own.small_at + align - 1) & !(align - 1);
        if start + bytes <= own.small_end {
            own.small_at = start + bytes;
            return start as *mut u8;
        }
        fresh_small(own, bytes)
    }

    #[cold]
    #[inline(never)]
    fn fresh_small(own: &mut Own, bytes: usize) -> *mut u8 {
        let zone_end = BASE.load(Relaxed) + SMALL_ZONE;
        let mut next = SMALL_NEXT.load(Relaxed);
        loop {
            if next + SMALL_CHUNK > zone_end {
                return give(own, bytes, bytes.min(PAGE));
            }
            match SMALL_NEXT.compare_exchange_weak(next, next + SMALL_CHUNK, SeqCst, Relaxed) {
                Ok(_) => break,
                Err(now) => next = now,
            }
        }
        (own.small_at, own.small_end) = (next + bytes, next + SMALL_CHUNK);
        next as *mut u8
    }

    /// What a request becomes: a small block of `1 << n` bytes, or `bytes` (whole pages)
    /// aligned to `align`.
    enum Kind {
        Small(u32),
        Large(usize, usize),
    }

    #[inline(always)]
    fn kind(size: usize, align: usize) -> Kind {
        if size <= 1 << SMALL && align <= PAGE {
            let bytes = size.max(align).max(1 << MIN);
            return Kind::Small(usize::BITS - (bytes - 1).leading_zeros());
        }
        let bytes = size.saturating_add(PAGE - 1) & !(PAGE - 1);
        if bytes > MOST { Kind::Large(bytes.saturating_add(HUGE - 1) & !(HUGE - 1), align.max(HUGE)) } else { Kind::Large(bytes, align.max(PAGE)) }
    }

    /// A block for the request, and whether it was used before; null when the reservation
    /// is used up.
    #[inline(always)]
    unsafe fn block(layout: Layout) -> (*mut u8, bool) {
        match kind(layout.size(), layout.align()) {
            Kind::Small(size) => OWN.with(|own| {
                // SAFETY: the thread's own, and nothing here allocates.
                let own = unsafe { &mut *own.get() };
                let list = &mut own.freed[(size - MIN) as usize];
                if *list != 0 {
                    let block = *list as *mut u8;
                    // SAFETY: a freed block of this size holds the next one's address.
                    *list = unsafe { block.cast::<usize>().read() };
                    return (block, true);
                }
                (give_small(own, 1usize << size), false)
            }),
            Kind::Large(bytes, align) => {
                // A freed block that holds it: any in the row of the next power of two.
                let row = (usize::BITS - (bytes - 1).leading_zeros()) as usize;
                if row < SIZES && align <= if bytes > MOST { HUGE } else { PAGE } {
                    for slot in &FREED[row] {
                        let block = slot.load(Relaxed);
                        if block != 0 && slot.compare_exchange(block, 0, SeqCst, Relaxed).is_ok() {
                            return (block as *mut u8, true);
                        }
                    }
                }
                if bytes > MOST || align > HUGE {
                    return (take(bytes, align), false);
                }
                // SAFETY: as above.
                (OWN.with(|own| give(unsafe { &mut *own.get() }, bytes, align)), false)
            }
        }
    }

    // SAFETY: a block is handed to one caller at a time (a thread's own mark and lists, a
    // compare-and-swap on the reservation's mark or on a slot), holds the size asked for at
    // the alignment asked for, and is not touched again until that caller frees it.
    unsafe impl GlobalAlloc for Region {
        #[inline]
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if ready() {
                let (block, _) = unsafe { block(layout) };
                if !block.is_null() {
                    return block;
                }
            }
            unsafe { System.alloc(layout) }
        }

        #[inline]
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if ready() {
                let (block, used) = unsafe { block(layout) };
                if !block.is_null() {
                    if used {
                        // SAFETY: the block holds at least `layout.size()` bytes.
                        unsafe { block.write_bytes(0, layout.size()) };
                    }
                    return block;
                }
            }
            unsafe { System.alloc_zeroed(layout) }
        }

        #[inline]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if !ours(ptr) {
                return unsafe { System.dealloc(ptr, layout) };
            }
            match kind(layout.size(), layout.align()) {
                Kind::Small(size) => OWN.with(|own| {
                    // SAFETY: the thread's own; the block is at least 16 bytes and no longer
                    // the caller's.
                    let own = unsafe { &mut *own.get() };
                    let list = &mut own.freed[(size - MIN) as usize];
                    unsafe { ptr.cast::<usize>().write(*list) };
                    *list = ptr as usize;
                }),
                Kind::Large(bytes, _) => {
                    // The row of the largest power of two it holds.
                    let row = (usize::BITS - 1 - bytes.leading_zeros()) as usize;
                    if row < SIZES {
                        for slot in &FREED[row] {
                            if slot.load(Relaxed) == 0 && slot.compare_exchange(0, ptr as usize, SeqCst, Relaxed).is_ok() {
                                return;
                            }
                        }
                    }
                    // Every slot taken: the block is not reused. Its pages stay until the exit.
                }
            }
        }

        #[inline]
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if !ours(ptr) {
                return unsafe { System.realloc(ptr, layout, new_size) };
            }
            // A block that shrinks stays, with its room: freed under the smaller size, it is
            // given out again as a block of that size.
            if new_size <= layout.size() {
                return ptr;
            }
            match (kind(layout.size(), layout.align()), kind(new_size, layout.align())) {
                // The block already holds it.
                (Kind::Small(old), Kind::Small(new)) if new <= old => return ptr,
                (Kind::Large(old, _), Kind::Large(new, _)) if new <= old => return ptr,
                // The last block this thread gave out grows in place.
                (Kind::Large(old, _), Kind::Large(new, _)) if new <= MOST => {
                    let grown = OWN.with(|own| {
                        // SAFETY: the thread's own.
                        let own = unsafe { &mut *own.get() };
                        let grows = ptr as usize + old == own.at && ptr as usize + new <= own.end;
                        if grows {
                            own.at = ptr as usize + new;
                            // Now, while the block's new bytes hold nothing.
                            if own.at > own.backed {
                                back_chunk(own);
                            }
                        }
                        grows
                    });
                    if grown {
                        return ptr;
                    }
                }
                _ => {}
            }
            // SAFETY: the caller's contract: `new_size` with this alignment is a valid layout.
            let to = unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, layout.align())) };
            if !to.is_null() {
                // `SS_POPULATE`: the copy below writes the new block's first pages one after
                // another, each a fault of its own where it is fresh memory without huge
                // pages. A large block starts on a page (`kind`).
                if new_size > MOST && ours(to) && COLLAPSE.load(Relaxed) != 0 {
                    // The reservation's own block, whole 2 MB steps: those the copy fills.
                    back(to as usize, (layout.size() + HUGE - 1) & !(HUGE - 1));
                } else if layout.size() >= POPULATE_FROM && ours(to) && to as usize & (PAGE - 1) == 0 && POPULATE.load(Relaxed) {
                    // SAFETY: whole pages inside the new block, which holds `new_size` bytes.
                    if unsafe { madvise(to, layout.size() & !(PAGE - 1), MADV_POPULATE_WRITE) } != 0 {
                        POPULATE.store(false, Relaxed);
                    }
                }
                // SAFETY: two distinct blocks, each at least `layout.size()` bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(ptr, to, layout.size());
                    self.dealloc(ptr, layout);
                }
            }
            to
        }
    }
}
