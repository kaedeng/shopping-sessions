//! Many append-only lists carved out of one allocation: a worker's dedup partitions in the parse,
//! its person buckets in the dedup stage (slots `alloc-parallel` and `scatter-wc`,
//! `alternatives.md`).
//!
//! One allocation per worker instead of one `Vec` per list. A block this large is one `mmap`,
//! and only its first page is touched up front, so the kernel faults the rest in as the worker
//! writes its records: on the worker's own thread, holding the address-space lock only for
//! reading, beside the other workers. Thousands of small `Vec`s had glibc write a header into
//! every page up front and grow its heaps with `mprotect` and `mmap`, which take that lock for
//! writing, so with 8 workers allocating at once the page faults and the heap growth took turns:
//! on the x86 box `mprotect` averaged 1.4 ms a call against 31 µs on one thread (`x86.md`).
//! A list's length is 4 bytes here against a `Vec`'s 24, which also shrinks what a scatter
//! keeps in L1.
//!
//! Two ways to write, picked per `Parts` and monomorphized per loop through `Mode`:
//! - `Plain`: each record is stored straight into its list's room. Into 1,024 lists that is a
//!   store to a cache line that is usually not in L1, and on x86 (32 KB L1D) the scatter is
//!   about a third of the parse.
//! - `Combine` (slot `scatter-wc`): each list stages its records in a buffer of whole cache
//!   lines; when the buffer is full it goes to the list's room with streaming stores, which
//!   write the line without reading it first and keep it out of the caches (software
//!   write-combining, Schuhknecht et al., PVLDB 2015). The staging buffers are `lists *
//!   lines * 64` bytes. The records still staged are part of the list: `pieces` returns them.
//!   A writer calls `fence` before other threads read what it wrote (once per task).
//!
//! A list that outgrows its `room` spills into a `Vec` of its own, so any list holds any number
//! of records; `pieces` returns the written part, the staged part, then the spill.

use std::alloc::{Layout, alloc, dealloc};
use std::marker::PhantomData;
use std::ptr::NonNull;

/// How records reach a list's room: chosen once per loop, never per record.
pub trait Mode {
    const COMBINE: bool;
}

/// Each record stored straight into its list.
pub struct Plain;

/// Records staged per list and written a buffer at a time with streaming stores.
pub struct Combine;

impl Mode for Plain {
    const COMBINE: bool = false;
}

impl Mode for Combine {
    const COMBINE: bool = true;
}

const LINE: usize = 64;

/// The write mode for every `Parts` of this run: `SS_SCATTER=combine|plain` (`combine` on
/// x86-64, where the scatter misses L1, `plain` elsewhere) and `SS_WC_LINES`, the cache lines a
/// staging buffer holds at least (default 4: on x86, 8 workers, the parse measured 7.7 GB/s
/// plain, 9.4–9.7 at 1 line, 9.5–10.1 at 2, 10.4–10.6 at 4, 10.3–10.4 at 8). `None` means
/// plain; read once.
pub fn mode_from_env() -> Option<usize> {
    mode_for("SS_SCATTER")
}

/// The write mode for the person buckets: `SS_SCATTER_BUCKETS=combine|plain`, defaulting to
/// `SS_SCATTER`'s. Streamed buckets are not in cache when the sessions stage reads them.
pub fn bucket_mode_from_env() -> Option<usize> {
    if std::env::var_os("SS_SCATTER_BUCKETS").is_some() { mode_for("SS_SCATTER_BUCKETS") } else { mode_from_env() }
}

fn mode_for(var: &str) -> Option<usize> {
    #[cfg(not(feature = "scatter-wc"))]
    let _ = var;
    #[cfg(feature = "scatter-wc")]
    {
        let options: &[&str] = if cfg!(target_arch = "x86_64") { &["combine", "plain"] } else { &["plain", "combine"] };
        if options[crate::env::pick(var, options)] == "combine" {
            let lines = crate::env::number("SS_WC_LINES", 4);
            assert!(lines.is_power_of_two() && lines <= 16, "SS_WC_LINES={lines}: 1, 2, 4, 8 or 16");
            return Some(lines);
        }
    }
    None
}

/// An aligned block of `T`s; uninitialized, owned, freed on drop.
struct Block<T> {
    ptr: NonNull<T>,
    layout: Layout,
}

impl<T> Block<T> {
    fn new(count: usize) -> Block<T> {
        let bytes = count.checked_mul(size_of::<T>()).expect("Parts: size overflows").max(1);
        let layout = Layout::from_size_align(bytes, LINE.max(align_of::<T>())).expect("Parts: layout");
        // SAFETY: `layout` has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc(layout) }.cast::<T>()).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Block { ptr, layout }
    }
}

impl<T> Drop for Block<T> {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr().cast(), self.layout) }
    }
}

// SAFETY: a `Block` is plain owned memory, as a `Vec<T>` is.
unsafe impl<T: Send> Send for Block<T> {}
unsafe impl<T: Sync> Sync for Block<T> {}

/// `lists` append-only lists of `T`, each with `room` records carved out of one allocation.
pub struct Parts<T> {
    /// `lists * room` slots: list `i` owns slots `i * room ..`.
    region: Block<T>,
    room: usize,
    /// Records in each list's room, staged ones included; never more than `room`.
    lens: Vec<u32>,
    spill: Vec<Vec<T>>,
    /// Records per staging buffer, a power of two; 0 when plain.
    batch: usize,
    /// `lists * batch` slots: list `i` stages in slots `i * batch ..`.
    stage: Block<T>,
    _records: PhantomData<T>,
}

impl<T: Copy> Parts<T> {
    /// Allocates the region (and, to combine, the staging buffers of `lines` cache lines each,
    /// `mode_from_env`); no page of the region is touched until a record lands there. `room` is
    /// rounded up to a whole number of staging buffers.
    pub fn new(lists: usize, room: usize, combine: Option<usize>) -> Parts<T> {
        assert!(lists > 0 && room > 0 && size_of::<T>() > 0, "Parts::new({lists}, {room})");
        // Records per buffer: the fewest, a power of two, that fill whole lines and at least the
        // lines asked for (4 lines: 8 of 32 bytes, 16 of 16, 16 of 24 in 6 lines).
        let batch = combine.map_or(0, |lines| {
            let bytes = |n: usize| n * size_of::<T>();
            let fill = (0..20).map(|shift| 1usize << shift).find(|&n| bytes(n) % LINE == 0 && bytes(n) >= lines * LINE);
            fill.expect("Parts: no power-of-two count of records fills whole cache lines")
        });
        let room = if batch > 0 { room.div_ceil(batch) * batch } else { room };
        assert!(room <= u32::MAX as usize, "Parts: room {room} over u32");
        let slots = lists.checked_mul(room).expect("Parts: lists * room overflows");
        Parts {
            region: Block::new(slots),
            room,
            lens: vec![0; lists],
            spill: (0..lists).map(|_| Vec::new()).collect(),
            batch,
            stage: Block::new(lists * batch),
            _records: PhantomData,
        }
    }

    pub fn lists(&self) -> usize {
        self.lens.len()
    }

    /// Whether this `Parts` stages its records: call the loops with `Combine` if so, else `Plain`.
    pub fn combining(&self) -> bool {
        self.batch > 0
    }

    /// Appends `rec` to list `list`, the way `M` says; `M::COMBINE` must equal `combining()`.
    ///
    /// # Safety
    /// `list < self.lists()`.
    #[inline(always)]
    pub unsafe fn push_as<M: Mode>(&mut self, list: usize, rec: T) {
        debug_assert!(list < self.lists() && M::COMBINE == self.combining());
        // SAFETY: the caller keeps `list` in range.
        let at = unsafe { *self.lens.get_unchecked(list) } as usize;
        if at >= self.room {
            self.spill(list, rec);
            return;
        }
        // SAFETY: as above; `at < room <= u32::MAX`.
        unsafe { *self.lens.get_unchecked_mut(list) = at as u32 + 1 };
        if !M::COMBINE {
            // SAFETY: `list * room + at < lists * room`, the region's size; each slot is written
            // once before `pieces` reads it.
            unsafe { self.region.ptr.as_ptr().add(list * self.room + at).write(rec) };
            return;
        }
        let fill = at & (self.batch - 1);
        // SAFETY: `list * batch + fill < lists * batch`, the staging buffers' size.
        let buffer = unsafe { self.stage.ptr.as_ptr().add(list * self.batch) };
        unsafe { buffer.add(fill).write(rec) };
        if fill == self.batch - 1 {
            // The buffer is full: `at + 1 - batch` is a multiple of `batch`, and `room` is too, so
            // the destination is inside the list's room and aligned to the buffer's size.
            let to = unsafe { self.region.ptr.as_ptr().add(list * self.room + at + 1 - self.batch) };
            unsafe { stream(buffer.cast(), to.cast(), self.batch * size_of::<T>()) };
        }
    }

    /// Appends `count` records the way `M` says: record `at` is `record(at)`, its list first.
    /// `push_as` in a loop of its own: the lists' fields stay in registers, and the one call,
    /// for a list that is full, is outside the loop that runs for every record. (Called per
    /// record from a decode loop, `push_as` reloaded the lengths' base, the room and the
    /// staging buffers from the stack each time: 24 instructions a record against 15.)
    ///
    /// # Safety
    /// Every list `record` returns is below `self.lists()`, and `M::COMBINE` equals
    /// `combining()`.
    #[inline(always)]
    pub unsafe fn extend_as<M: Mode>(&mut self, count: usize, mut record: impl FnMut(usize) -> (usize, T)) {
        debug_assert!(M::COMBINE == self.combining());
        let (room, batch) = (self.room, self.batch);
        let (mask, shift) = (batch.wrapping_sub(1), if M::COMBINE { batch.trailing_zeros() } else { 0 });
        let mut at = 0;
        loop {
            let (lens, region, stage) = (self.lens.as_mut_ptr(), self.region.ptr.as_ptr(), self.stage.ptr.as_ptr());
            // SAFETY: as `push_as`, list by list.
            let (list, rec) = unsafe {
                loop {
                    if at == count {
                        return;
                    }
                    let (list, rec) = record(at);
                    debug_assert!(list < self.lens.len());
                    let len = *lens.add(list) as usize;
                    if len >= room {
                        break (list, rec);
                    }
                    at += 1;
                    *lens.add(list) = len as u32 + 1;
                    if !M::COMBINE {
                        region.add(list * room + len).write(rec);
                        continue;
                    }
                    let fill = len & mask;
                    let buffer = stage.add(list << shift);
                    buffer.add(fill).write(rec);
                    if fill == mask {
                        stream(buffer.cast(), region.add(list * room + len - mask).cast(), batch * size_of::<T>());
                    }
                }
            };
            self.spill(list, rec);
            at += 1;
        }
    }

    /// Appends `rec` to list `list`, plain or staged as this `Parts` was built; panics if there
    /// is no such list. Not for hot loops: it decides the mode per call.
    pub fn push(&mut self, list: usize, rec: T) {
        assert!(list < self.lists(), "list {list} of {}", self.lists());
        // SAFETY: checked above; the mode matches `combining()`.
        unsafe {
            if self.combining() { self.push_as::<Combine>(list, rec) } else { self.push_as::<Plain>(list, rec) }
        }
    }

    /// Makes this thread's streamed lines visible to other threads: call it after the last
    /// `push_as::<Combine>` of a task, before another thread may read the lists.
    #[inline]
    pub fn fence(&self) {
        #[cfg(target_arch = "x86_64")]
        if self.combining() {
            // SAFETY: SSE is part of x86-64 (newer compilers call this intrinsic safe).
            #[allow(unused_unsafe)]
            unsafe {
                core::arch::x86_64::_mm_sfence()
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn spill(&mut self, list: usize, rec: T) {
        self.spill[list].push(rec);
    }

    /// List `list` in order of appending: the records written to its room, those still staged,
    /// then the spill. The staged and spilled pieces are usually short or empty.
    pub fn pieces(&self, list: usize) -> [&[T]; 3] {
        let len = self.lens[list] as usize;
        let staged = if self.batch > 0 { len & (self.batch - 1) } else { 0 };
        // SAFETY: the first `len - staged` slots of the list's room were written, by
        // `push_as::<Plain>` or by full buffers; the first `staged` slots of its buffer hold the
        // rest. A plain `Parts` has `staged == 0` and an empty, aligned buffer.
        unsafe {
            [
                std::slice::from_raw_parts(self.region.ptr.as_ptr().add(list * self.room), len - staged),
                std::slice::from_raw_parts(self.stage.ptr.as_ptr().add(list * self.batch), staged),
                &self.spill[list],
            ]
        }
    }

    /// Records in list `list`.
    pub fn len_of(&self, list: usize) -> usize {
        self.lens[list] as usize + self.spill[list].len()
    }

    /// Records in all lists.
    pub fn len(&self) -> usize {
        (0..self.lists()).map(|list| self.len_of(list)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lists that spilled: more than `room` records were appended to them.
    pub fn spilled(&self) -> usize {
        self.spill.iter().filter(|spill| !spill.is_empty()).count()
    }

    /// Every record, list by list.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.lists()).flat_map(move |list| self.pieces(list)).flatten()
    }

    /// Writes a byte into every page of the lists' room and of the staging buffers until
    /// `stop` says so, so that the kernel backs them now: for a thread with nothing else to do
    /// while the inputs stage ends. Left to the first records, those page faults (2 MB zeroed
    /// each with huge pages) made a worker's first chunk take two to three times as long as
    /// its later ones. The lists are empty; nothing reads these bytes before a record
    /// overwrites them.
    ///
    /// `lists`: only the first so many lists (the others are not expected to get a record:
    /// their pages are then backed when one does).
    pub fn touch(&mut self, lists: usize, stop: &impl Fn() -> bool) {
        const PAGE: usize = 4096;
        let lists = lists.min(self.lists());
        let (stage, region) = (lists * self.batch * size_of::<T>(), lists * self.room * size_of::<T>());
        for block in [(self.stage.ptr.as_ptr().cast::<u8>(), stage.min(self.stage.layout.size())), (self.region.ptr.as_ptr().cast::<u8>(), region.min(self.region.layout.size()))] {
            let (bytes, len) = block;
            let mut at = 0;
            while at < len {
                if stop() {
                    return;
                }
                // SAFETY: inside the block, which this `Parts` owns and holds no record yet.
                unsafe { bytes.add(at).write(0) };
                at += PAGE;
            }
        }
    }

    /// Slot `free-early`: empties every list and hands the region's pages back to the kernel on
    /// the calling thread (`mem::release`), instead of leaving them to the one thread that
    /// exits. The lists can be filled again afterwards.
    pub fn release(&mut self) {
        self.lens.fill(0);
        self.spill.iter_mut().for_each(|spill| *spill = Vec::new());
        // SAFETY: every list is empty now, so no slot of the region is read before it is
        // written again.
        unsafe { crate::mem::release(self.region.ptr.as_ptr().cast(), self.region.layout.size()) };
    }
}

/// Copies `bytes` (a multiple of 64) from `from` to `to`, both 64-byte aligned, with streaming
/// stores where the target has them.
///
/// # Safety
/// Both ranges are valid and do not overlap.
#[inline(always)]
unsafe fn stream(from: *const u8, to: *mut u8, bytes: usize) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use core::arch::x86_64::{__m256i, _mm256_load_si256, _mm256_stream_si256};
        let mut at = 0;
        while at < bytes {
            _mm256_stream_si256(to.add(at).cast::<__m256i>(), _mm256_load_si256(from.add(at).cast::<__m256i>()));
            at += 32;
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    unsafe {
        core::ptr::copy_nonoverlapping(from, to, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::Parts;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Rec24([u64; 3]);

    fn check<T: Copy + PartialEq + std::fmt::Debug>(make: impl Fn(u64) -> T) {
        for combine in [None, Some(1), Some(2)] {
            for room in [1, 2, 5, 64] {
                let mut parts = Parts::new(7, room, combine);
                let mut expected = vec![Vec::new(); 7];
                for n in 0..1000u64 {
                    let list = ((n * 2_654_435_761) >> 7) as usize % 7;
                    parts.push(list, make(n));
                    expected[list].push(make(n));
                }
                parts.fence();
                for (list, want) in expected.iter().enumerate() {
                    let got: Vec<T> = parts.pieces(list).concat();
                    assert_eq!(&got, want, "combine {combine:?}, room {room}, list {list}");
                    assert_eq!(parts.len_of(list), want.len());
                }
                assert_eq!(parts.len(), 1000);
                assert_eq!(parts.iter().count(), 1000);
                assert!(parts.spilled() > 0);
            }
        }
    }

    #[test]
    fn lists_keep_order_and_spill() {
        check(|n| n);
        check(|n| (n, n * 3));
        check(|n| [n, n + 1, n + 2, n + 3]);
        check(|n| Rec24([n, !n, n * 7]));
    }

    #[test]
    fn extend_is_push() {
        for combine in [None, Some(1), Some(4)] {
            for room in [1, 3, 64] {
                let (mut pushed, mut extended) = (Parts::new(7, room, combine), Parts::new(7, room, combine));
                let record = |n: u64| (((n * 2_654_435_761) >> 7) as usize % 7, [n, !n]);
                for n in 0..1000u64 {
                    let (list, rec) = record(n);
                    pushed.push(list, rec);
                }
                for first in (0..1000u64).step_by(100) {
                    // SAFETY: the lists are below 7, and the mode is the one built with.
                    unsafe {
                        if extended.combining() {
                            extended.extend_as::<super::Combine>(100, |at| record(first + at as u64));
                        } else {
                            extended.extend_as::<super::Plain>(100, |at| record(first + at as u64));
                        }
                    }
                }
                extended.fence();
                for list in 0..7 {
                    assert_eq!(extended.pieces(list).concat(), pushed.pieces(list).concat(), "combine {combine:?}, room {room}, list {list}");
                }
            }
        }
    }

    #[test]
    fn release_empties_every_list() {
        for combine in [None, Some(1), Some(4)] {
            let mut parts = Parts::new(3, 4, combine);
            for n in 0..100u64 {
                parts.push((n % 3) as usize, [n; 2]);
            }
            parts.release();
            assert!(parts.is_empty() && parts.spilled() == 0, "combine {combine:?}");
            // The lists fill again from their first slot.
            for n in 0..7u64 {
                parts.push(2, [n; 2]);
            }
            parts.fence();
            assert_eq!(parts.pieces(2).concat(), (0..7).map(|n| [n; 2]).collect::<Vec<_>>(), "combine {combine:?}");
            assert_eq!(parts.len(), 7);
        }
    }

    #[test]
    fn buffer_sizes() {
        assert_eq!(Parts::<[u64; 4]>::new(1, 1, Some(1)).batch, 2);
        assert_eq!(Parts::<[u64; 4]>::new(1, 1, Some(4)).batch, 8);
        assert_eq!(Parts::<[u64; 2]>::new(1, 1, Some(4)).batch, 16);
        assert_eq!(Parts::<[u64; 3]>::new(1, 1, Some(1)).batch, 8);
        assert_eq!(Parts::<[u64; 3]>::new(1, 1, Some(4)).batch, 16);
        assert_eq!(Parts::<[u64; 3]>::new(1, 5, Some(4)).room, 16);
        assert_eq!(Parts::<[u64; 3]>::new(1, 5, None).batch, 0);
    }

    #[test]
    fn staged_records_are_listed_before_a_flush() {
        // 32-byte records, one line: buffers of 2. Three records leave one staged.
        let mut parts = Parts::new(2, 8, Some(1));
        for n in 0..3u64 {
            parts.push(1, [n; 4]);
        }
        assert_eq!(parts.pieces(1).map(<[_]>::len), [2, 1, 0]);
        assert_eq!(parts.pieces(1).concat(), vec![[0; 4], [1; 4], [2; 4]]);
    }

    #[test]
    fn empty_lists() {
        for combine in [None, Some(1)] {
            let parts: Parts<u32> = Parts::new(3, 4, combine);
            assert!(parts.is_empty());
            assert!((0..3).all(|list| parts.pieces(list).iter().all(|piece| piece.is_empty())));
        }
    }
}
