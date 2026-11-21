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
