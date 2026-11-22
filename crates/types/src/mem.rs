/// Asks for the cache line at `p`; any address is fine, mapped or not.
#[inline(always)]
pub fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p as *const i8)
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

/// `len` zeroed `T`s on memory nothing has touched: the kernel's zero pages, each made
/// real when it is first written.
///
/// A `vec![0; len]` asks the allocator for zeroed memory too, but the process's allocator
/// (`src/region.rs`) may answer with a block that was freed earlier, and must then clear
/// all of it at once, on the calling thread: 6 ms for a 67 MB table at 20M events, before
/// the first worker could start. A request aligned above a huge page is never given such a
/// block.
pub struct Zeroed<T> {
    at: std::ptr::NonNull<T>,
    len: usize,
    layout: std::alloc::Layout,
}

// SAFETY: plain owned memory, as a `Vec<T>` is.
unsafe impl<T: Send> Send for Zeroed<T> {}
unsafe impl<T: Sync> Sync for Zeroed<T> {}

impl<T> Zeroed<T> {
    /// # Safety
    /// All zeros is a valid `T`.
    pub unsafe fn new(len: usize) -> Zeroed<T> {
        let layout = std::alloc::Layout::from_size_align((len * size_of::<T>()).max(1), 4 << 20).expect("Zeroed: layout");
        // SAFETY: the layout has a size.
        let at = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<T>();
        let Some(at) = std::ptr::NonNull::new(at) else { std::alloc::handle_alloc_error(layout) };
        Zeroed { at, len, layout }
    }
}

/// Bytes of a huge page.
const HUGE: usize = 2 << 20;

impl<T> Zeroed<T> {
    /// The indexes of one value in each 2 MB of the memory, those of piece `piece` of
    /// `pieces`.
    fn firsts(&self, piece: usize, pieces: usize) -> impl Iterator<Item = usize> + use<T> {
        let step = (HUGE / size_of::<T>().max(1)).max(1);
        let steps = self.len.div_ceil(step);
        (steps * piece / pieces..steps * (piece + 1) / pieces).map(move |at| at * step)
    }

    /// `touch` for memory no other thread has yet: the first `len` values onto whole 2 MB
    /// pages.
    pub fn touch_first(&mut self, len: usize) {
        // `SS_POPULATE`: every page of them backed in one call (`populate`), for a machine
        // without huge pages.
        let bytes = (len.min(self.len) * size_of::<T>()).next_multiple_of(4096).min(self.layout.size() & !4095);
        // `SS_COLLAPSE`: 2 MB pages asked for by name (`collapse`), after the writes below.
        let huge = collapse_on().then(|| bytes.next_multiple_of(HUGE).min(self.layout.size() & !(HUGE - 1)));
        if let Some(whole) = huge {
            // SAFETY: whole 2 MB steps of the memory, which starts on one.
            unsafe { advise_huge(self.at.as_ptr().cast(), whole) };
        } else if populate_window() != 0 {
            // SAFETY: whole pages of the memory, which starts on a page.
            if bytes != 0 && unsafe { populate(self.at.as_ptr().cast(), bytes) } {
                return;
            }
        }
        let step = (HUGE / size_of::<T>().max(1)).max(1);
        for at in (0..len.min(self.len)).step_by(step) {
            // SAFETY: inside the memory, which is zeros there: the write changes nothing.
            // Volatile, or the compiler drops a zero written to zeroed memory.
            unsafe { self.at.as_ptr().add(at).cast::<u8>().write_volatile(0) };
        }
        if let Some(whole) = huge {
            // SAFETY: as above; each step has a page written.
            if whole != 0 && !unsafe { collapse(self.at.as_ptr().cast(), whole) } && populate_window() != 0 {
                // SAFETY: whole pages of the memory.
                unsafe { populate(self.at.as_ptr().cast(), bytes) };
            }
        }
    }
}

/// Writes to piece `piece` of `pieces` of a table of atomics, changing nothing: one word in
/// every 2 MB is swapped for 0 if it is 0. At any time, by any thread.
///
/// What the kernel maps at an untouched address depends on the first access. A read gets
/// the shared zero page, for 2 MB at once where huge pages are on; the first write under
/// that mapping then breaks it into 4 KB pages, each faulted in by its own first write, and
/// every lookup after pays a page walk. A table that is probed before it is written (find
/// the key, or claim the slot) goes that way: at 20M events the users' directory and the
/// persons' arrays took 21,000 of a run's 34,000 page faults. A write to an untouched
/// address gets a whole 2 MB page, zeroed once.
///
/// The write is a compare-and-swap, which writes whether or not it succeeds. An atomic `or`
/// of 0 does not: the compiler knows it changes nothing and makes it a fenced load.
pub fn touch<T>(table: &Zeroed<T>, piece: usize, pieces: usize, word: impl Fn(&T) -> &std::sync::atomic::AtomicU64) {
    use std::sync::atomic::Ordering::Relaxed;
    // `SS_POPULATE`: every page of the piece backed in one call (`populate`), for a machine
    // without huge pages: there each page of the table is otherwise a fault of its own, at
    // the first key that lands on it.
    let bytes = table.len * size_of::<T>();
    let steps = bytes.div_ceil(HUGE);
    let (from, to) = (steps * piece / pieces * HUGE, (steps * (piece + 1) / pieces * HUGE).min(bytes & !4095));
    // `SS_COLLAPSE`: 2 MB pages asked for by name (`collapse`), after the writes below.
    let huge = collapse_on() && to > from;
    if huge {
        // SAFETY: whole pages inside the table, which starts on a 2 MB step.
        unsafe { advise_huge(table.at.as_ptr().cast::<u8>().add(from), to - from) };
    } else if populate_window() != 0 {
        // SAFETY: whole pages inside the table, which starts on a page.
        if to > from && unsafe { populate(table.at.as_ptr().cast::<u8>().add(from), to - from) } {
            return;
        }
    }
    for at in table.firsts(piece, pieces) {
        let _ = word(&table[at]).compare_exchange(0, std::hint::black_box(0), Relaxed, Relaxed);
    }
    if huge {
        // SAFETY: as above; every 2 MB step of the piece has a page written.
        if !unsafe { collapse(table.at.as_ptr().cast::<u8>().add(from), to - from) } && populate_window() != 0 {
            // SAFETY: as above.
            unsafe { populate(table.at.as_ptr().cast::<u8>().add(from), to - from) };
        }
    }
}

impl<T> std::ops::Deref for Zeroed<T> {
    type Target = [T];

    #[inline(always)]
    fn deref(&self) -> &[T] {
        // SAFETY: `len` values, valid from the first (zeros) on.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }
}

impl<T> std::ops::DerefMut for Zeroed<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, and owned.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

impl<T> Drop for Zeroed<T> {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout; the values need no drop of their own
        // that matters here (integers and atomics).
        unsafe { std::alloc::dealloc(self.at.as_ptr().cast(), self.layout) }
    }
}

/// Slot `free-early` (`SS_FREE=early|exit`): whether a buffer goes back to the kernel as soon as
/// it is dead (`release`) or with everything else at exit. Read once.
pub fn free_early() -> bool {
    static EARLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EARLY.get_or_init(|| {
        let options: &[&str] = if cfg!(feature = "free-early") { &["early", "exit"] } else { &["exit"] };
        options[crate::env::pick("SS_FREE", options)] == "early"
    })
}

/// Slot `free-early`: hands the whole pages inside `addr..addr + len` back to the kernel now
/// (`MADV_DONTNEED`), on the calling thread. Otherwise every page waits for the process's exit,
/// where one thread unmaps them all while the others are gone: on the x86 box the process ended
/// 125 ms after its last output at full size with 2 MB pages and 410 ms with 4 KB pages
/// (`x86.md`). `MADV_DONTNEED` takes the address-space lock shared, where `munmap` takes it
/// exclusive, so the workers release side by side. Linux only; a no-op elsewhere.
///
/// # Safety
/// Anonymous memory reads as zeros afterwards, so nothing may read `addr..addr + len` again for
/// its contents. A read-only file mapping is unaffected: a later read faults the page back in
/// from the page cache.
pub unsafe fn release(addr: *const u8, len: usize) {
    #[cfg(target_os = "linux")]
    {
        use core::ffi::{c_int, c_long, c_void};
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        unsafe extern "C" {
            fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
            fn sysconf(name: c_int) -> c_long;
        }
        const MADV_DONTNEED: c_int = 4;
        const SC_PAGESIZE: c_int = 30;
        static PAGE: AtomicUsize = AtomicUsize::new(0);
        let mut page = PAGE.load(Relaxed);
        if page == 0 {
            // The kernel rounds the length up to its own page size, so a guessed 4 KB could
            // reach past the range on a 16 or 64 KB kernel.
            // SAFETY: a query with no side effect.
            page = usize::try_from(unsafe { sysconf(SC_PAGESIZE) }).ok().filter(|page| page.is_power_of_two()).unwrap_or(1 << 16);
            PAGE.store(page, Relaxed);
        }
        let (from, to) = ((addr as usize).next_multiple_of(page), (addr as usize + len) & !(page - 1));
        if to > from {
            // SAFETY: whole pages inside the caller's range. A failure leaves the pages mapped,
            // which is what happens without this slot.
            unsafe { madvise(from as *mut c_void, to - from, MADV_DONTNEED) };
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (addr, len);
}

/// `SS_POPULATE=KB`: the bytes `populate` is asked for at a time by a list that takes its
/// memory page after page (`chunks::Chunks`), a power of two; 0, the default, for none.
/// Read once.
///
/// For a machine without huge pages (the scorer's transparent huge pages are `never`): there
/// every 4 KB page of fresh memory is a fault of its own, 416,000 of them in a run at 20M
/// events against 2,800 with 2 MB pages, each an exception and a turn at the address-space
/// lock that the eight workers take at once.
pub fn populate_window() -> usize {
    static WINDOW: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WINDOW.get_or_init(|| {
        let bytes = crate::env::number("SS_POPULATE", 0) << 10;
        if bytes == 0 { 0 } else { bytes.next_power_of_two().max(4096) }
    })
}

/// Has the kernel back `addr..addr + len` with zeroed pages now, in one call
/// (`MADV_POPULATE_WRITE`, Linux 5.14): the pages a thread is about to write one after
/// another, without a fault for each. A kernel that does not know the call says so once and
/// is not asked again. Linux only; a no-op elsewhere.
///
/// Returns whether the kernel did it.
///
/// # Safety
/// `addr..addr + len` is whole 4 KB pages of anonymous memory the caller owns. Pages that
/// hold data keep it.
pub unsafe fn populate(addr: *mut u8, len: usize) -> bool {
    #[cfg(target_os = "linux")]
    {
        use core::ffi::{c_int, c_void};
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        unsafe extern "C" {
            fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
        }
        const MADV_POPULATE_WRITE: c_int = 23;
        static REFUSED: AtomicBool = AtomicBool::new(false);
        if len != 0 && !REFUSED.load(Relaxed) {
            // SAFETY: the caller's range. A failure leaves the pages to their first writes.
            if unsafe { madvise(addr.cast(), len, MADV_POPULATE_WRITE) } == 0 {
                return true;
            }
            REFUSED.store(true, Relaxed);
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (addr, len);
        false
    }
}

/// `SS_COLLAPSE=off|on`: whether the tables that are looked up at random (the directory of
/// the users, the arrays of the persons) are put onto 2 MB pages by `collapse`. Read once.
///
/// For the scorer: its transparent huge pages are `never`, its kernel is 6.4 or later, and
/// a timing probe saw `MADV_COLLAPSE` make a 2 MB page there all the same. A lookup in a
/// 67 MB table on 4 KB pages is a page walk each time; on 2 MB pages the table is 34 entries
/// of the TLB.
pub fn collapse_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::env::pick("SS_COLLAPSE", &["off", "on"]) == 1)
}

/// Marks `addr..addr + len` as wanting huge pages (`MADV_HUGEPAGE`): where the machine gives
/// them on a first write, the writes that follow get them, and `collapse` has nothing left
/// to do.
///
/// # Safety
/// Whole pages of anonymous memory the caller owns.
unsafe fn advise_huge(addr: *mut u8, len: usize) {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: core::ffi::c_int) -> core::ffi::c_int;
        }
        const MADV_HUGEPAGE: core::ffi::c_int = 14;
        // SAFETY: the range of the caller; a refusal changes nothing.
        unsafe { madvise(addr.cast(), len, MADV_HUGEPAGE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (addr, len);
}

/// Has the kernel make every whole 2 MB step inside `addr..addr + len` one 2 MB page now
/// (`MADV_COLLAPSE`, Linux 6.1; it does not ask the huge-page setting of the machine). A
/// step needs at least one page written already. Each step is done under the address-space
/// lock held for writing, the rest of its 2 MB zeroed there: for a few tables before the
/// workers start, not for the lists. Returns whether every step became one page.
///
/// # Safety
/// Whole pages of anonymous memory the caller owns; their contents stay.
unsafe fn collapse(addr: *mut u8, len: usize) -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: core::ffi::c_int) -> core::ffi::c_int;
        }
        const MADV_COLLAPSE: core::ffi::c_int = 25;
        // SAFETY: the range of the caller; a failure leaves the pages as they are.
        unsafe { madvise(addr.cast(), len, MADV_COLLAPSE) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (addr, len);
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_keeps_the_partial_pages_and_zeroes_the_whole_ones() {
        // 1 MB so the range holds whole pages at any page size up to 64 KB.
        let mut bytes = vec![7u8; 1 << 20];
        let (first, last) = (bytes.as_ptr(), bytes.len() - 1);
        // SAFETY: the buffer is ours, and the test reads it knowing what `release` leaves.
        unsafe { super::release(first.wrapping_add(1), last - 1) };
        // The partial pages at both ends are never dropped.
        assert_eq!((bytes[0], bytes[1], bytes[last - 1], bytes[last]), (7, 7, 7, 7));
        // On Linux the whole pages inside read as zeros now; elsewhere nothing changed.
        let middle = bytes.len() / 2;
        assert_eq!(bytes[middle], if cfg!(target_os = "linux") { 0 } else { 7 });
        bytes[middle] = 9;
        assert_eq!(bytes[middle], 9);
    }
}
