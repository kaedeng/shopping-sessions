//! Many append-only lists carved out of one allocation: a worker's dedup partitions in the parse,
//! its person buckets in the dedup stage (slot `alloc-parallel`, `alternatives.md`).
//!
//! One allocation per worker instead of one `Vec` per list. glibc serves a block this large with
//! a single `mmap` and writes only its first page, so the kernel faults the rest in as the worker
//! writes its records: on the worker's own thread, holding the address-space lock only for
//! reading, beside the other workers. Thousands of small `Vec`s had glibc write a header into
//! every page up front and grow its heaps with `mprotect` and `mmap`, which take that lock for
//! writing, so with 8 workers allocating at once the page faults and the heap growth took turns:
//! on the x86 box `mprotect` averaged 1.4 ms a call against 31 µs on one thread (`x86.md`).
//! A list's length is 4 bytes here against a `Vec`'s 24, which also shrinks what a scatter
//! keeps in L1.
//!
//! A list that outgrows its `room` spills into a `Vec` of its own, so any list holds any number
//! of records; `pieces` returns the carved part, the spill, then what is still staged.
//!
//! Slot `scatter-wc` (`SS_SCATTER=wc|plain`, the pick on x86-64): a record first goes to a small
//! staging buffer of its list, and a full buffer moves to the list's end in whole cache lines
//! with streaming (non-temporal) stores. A scatter to 1,024 lists otherwise takes a cache miss
//! per new line of every list: the lists' ends (64 KB) do not fit a 32 KB L1D, and each new line
//! is first read from memory only to be overwritten. Staged, the stores land in a compact area
//! that stays cached, and a streaming store writes a full line without reading it, so it also
//! halves the memory traffic of the scatter (Schuhknecht, Khanchandani and Dittrich, PVLDB 2015).

use std::alloc::{self, Layout};
use std::ptr::NonNull;
use std::sync::OnceLock;

use crate::{env, mem};

/// A streaming store wants whole cache lines.
const LINE: usize = 64;

/// Bytes a list stages before they move to its end (`SS_WC_BYTES`). The x86 box measured the
/// parse at 256 bytes ahead of 128 and 64: a flush is a branch the predictor cannot learn, and
/// a larger stage takes it less often, while the stages of 1,024 lists still fit L2.
const STAGE_BYTES: usize = 256;

/// The staging of `Parts::new`: 0 for plain stores. Read once.
fn default_stage_bytes() -> usize {
    static BYTES: OnceLock<usize> = OnceLock::new();
    *BYTES.get_or_init(|| {
        // Streaming stores are an x86 remedy: elsewhere the plain loop stays the pick.
        let options: &[&str] = match (cfg!(feature = "scatter-wc"), cfg!(target_arch = "x86_64")) {
            (false, _) => &["plain"],
            (true, true) => &["wc", "plain"],
            (true, false) => &["plain", "wc"],
        };
        match options[env::pick("SS_SCATTER", options)] {
            "wc" => env::number("SS_WC_BYTES", STAGE_BYTES).max(1),
            _ => 0,
        }
    })
}

/// `slots` uninitialised `T`s on a cache-line boundary. Like a `Vec`, a large one is a single
/// `mmap` of which the allocator touches only the first page.
struct Block<T> {
    at: NonNull<T>,
    slots: usize,
}

// SAFETY: a `Block` owns its memory like a `Vec<T>` does.
unsafe impl<T: Send> Send for Block<T> {}
unsafe impl<T: Sync> Sync for Block<T> {}

impl<T> Block<T> {
    fn layout(slots: usize) -> Layout {
        Layout::array::<T>(slots).and_then(|layout| layout.align_to(LINE)).expect("Parts: the size overflows")
    }

    fn new(slots: usize) -> Block<T> {
        let layout = Self::layout(slots);
        if layout.size() == 0 {
            return Block { at: NonNull::dangling(), slots };
        }
        // SAFETY: the layout has a size.
        let at = unsafe { alloc::alloc(layout) }.cast::<T>();
        Block { at: NonNull::new(at).unwrap_or_else(|| alloc::handle_alloc_error(layout)), slots }
    }

    fn bytes(&self) -> usize {
        self.slots * size_of::<T>()
    }
}

impl<T> Drop for Block<T> {
    fn drop(&mut self) {
        let layout = Self::layout(self.slots);
        if layout.size() != 0 {
            // SAFETY: allocated in `new` with this layout.
            unsafe { alloc::dealloc(self.at.as_ptr().cast(), layout) };
        }
    }
}

/// `lists` append-only lists of `T`, each with `room` records carved out of one allocation.
pub struct Parts<T> {
    /// `lists * room` slots: list `i` owns slots `i * room ..`, of which the first `lens[i]`
    /// are written.
    region: Block<T>,
    room: usize,
    lens: Vec<u32>,
    spill: Vec<Vec<T>>,
    /// Slot `scatter-wc`: list `i` stages its newest `fill[i]` records in slots `i * stage ..`
    /// of `staged`. `stage` is 0 for plain stores, and then nothing is ever staged.
    staged: Block<T>,
    stage: usize,
    fill: Vec<u8>,
}

impl<T: Copy> Parts<T> {
    /// Allocates the region; no page of it is touched until a record is written there. Staged
    /// or plain as `SS_SCATTER` says (module docs).
    pub fn new(lists: usize, room: usize) -> Parts<T> {
        Self::with_stage_bytes(lists, room, default_stage_bytes())
    }

    /// Like `new` with the staging given: each list stages about `stage_bytes` bytes (whole
    /// cache lines of whole records, at least one line), or nothing when it is 0.
    pub fn with_stage_bytes(lists: usize, room: usize, stage_bytes: usize) -> Parts<T> {
        assert!(lists > 0 && room > 0 && room <= u32::MAX as usize, "Parts::new({lists}, {room})");
        let size = size_of::<T>();
        let stage = if stage_bytes == 0 || size == 0 || LINE % align_of::<T>() != 0 {
            0
        } else {
            // The fewest records that end on a line boundary, then as many of those groups as fit.
            let group = LINE / gcd(size, LINE);
            (group * (stage_bytes / (group * size)).max(1)).min(u8::MAX as usize / group * group)
        };
        // A flush moves a whole stage, so a staged list's room is a whole number of stages and
        // every list starts on a line boundary.
        let room = if stage == 0 { room } else { room.next_multiple_of(stage) };
        assert!(room <= u32::MAX as usize, "Parts::new({lists}, {room})");
        let slots = lists.checked_mul(room).expect("Parts: lists * room overflows");
        Parts {
            region: Block::new(slots),
            room,
            lens: vec![0; lists],
            spill: (0..lists).map(|_| Vec::new()).collect(),
            staged: Block::new(lists * stage),
            stage,
            fill: vec![0; lists],
        }
    }

    pub fn lists(&self) -> usize {
        self.lens.len()
    }

    /// Appends `rec` to list `list`.
    ///
    /// # Safety
    /// `list < self.lists()`.
    #[inline(always)]
    pub unsafe fn push_unchecked(&mut self, list: usize, rec: T) {
        // One branch per record on a value fixed for the run, so the predictor always has it;
        // the two loops are not worth a second copy of every caller.
        if self.stage != 0 {
            // SAFETY: the caller keeps `list` in range, and `fill[list] < stage`.
            unsafe {
                let fill = self.fill.get_unchecked_mut(list);
                let at = *fill as usize;
                self.staged.at.as_ptr().add(list * self.stage + at).write(rec);
                if at + 1 == self.stage {
                    *fill = 0;
                    self.flush(list);
                } else {
                    *fill = at as u8 + 1;
                }
            }
            return;
        }
        // SAFETY: the caller keeps `list` in range.
        let len = unsafe { self.lens.get_unchecked_mut(list) };
        if (*len as usize) < self.room {
            // SAFETY: `list * room + len < lists * room`, the region's capacity, and each slot is
            // written at most once before `pieces` reads it.
            unsafe { self.region.at.as_ptr().add(list * self.room + *len as usize).write(rec) };
            *len += 1;
        } else {
            self.spill(list, rec);
        }
    }

    /// Moves list `list`'s full stage to its end.
    ///
    /// # Safety
    /// `list < self.lists()`, staging is on, and the list's whole stage is written.
    #[inline(always)]
    unsafe fn flush(&mut self, list: usize) {
        // SAFETY: the caller keeps `list` in range; the carved length is a whole number of
        // stages, so `len + stage <= room` leaves room for all of this one.
        unsafe {
            let len = self.lens.get_unchecked_mut(list);
            let from = self.staged.at.as_ptr().add(list * self.stage);
            if *len as usize + self.stage <= self.room {
                let to = self.region.at.as_ptr().add(list * self.room + *len as usize);
                stream(from.cast(), to.cast(), self.stage * size_of::<T>());
                *len += self.stage as u32;
            } else {
                self.spill_stage(list);
            }
        }
    }

    /// Appends `rec` to list `list`; panics if there is no such list.
    pub fn push(&mut self, list: usize, rec: T) {
        assert!(list < self.lists(), "list {list} of {}", self.lists());
        // SAFETY: checked above.
        unsafe { self.push_unchecked(list, rec) }
    }

    #[cold]
    #[inline(never)]
    fn spill(&mut self, list: usize, rec: T) {
        self.spill[list].push(rec);
    }

    #[cold]
    #[inline(never)]
    fn spill_stage(&mut self, list: usize) {
        // SAFETY: called with the list's whole stage written.
        let stage = unsafe { std::slice::from_raw_parts(self.staged.at.as_ptr().add(list * self.stage), self.stage) };
        self.spill[list].extend_from_slice(stage);
    }

    /// Call after a run of `push`es and before another thread reads the lists: streaming stores
    /// are weakly ordered, and this makes them visible like any other store. Free when nothing
    /// was streamed.
    #[inline]
    pub fn fence(&self) {
        #[cfg(target_arch = "x86_64")]
        if self.stage != 0 {
            // SAFETY: SSE is part of x86-64.
            unsafe { core::arch::x86_64::_mm_sfence() };
        }
    }

    /// List `list` in order of appending: the carved part, the spill (usually empty), then the
    /// records still staged.
    pub fn pieces(&self, list: usize) -> [&[T]; 3] {
        let (len, fill) = (self.lens[list] as usize, self.fill[list] as usize);
        // SAFETY: the first `len` slots of the list's room and the first `fill` of its stage
        // were written by `push_unchecked`.
        let (carved, staged) = unsafe {
            (
                std::slice::from_raw_parts(self.region.at.as_ptr().add(list * self.room), len),
                std::slice::from_raw_parts(self.staged.at.as_ptr().add(list * self.stage), fill),
            )
        };
        [carved, &self.spill[list], staged]
    }

    /// Records in list `list`.
    pub fn len_of(&self, list: usize) -> usize {
        self.lens[list] as usize + self.spill[list].len() + self.fill[list] as usize
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

    /// Slot `free-early`: empties every list and hands the region's pages back to the kernel on
    /// the calling thread (`mem::release`). The lists can be filled again afterwards.
    pub fn release(&mut self) {
        self.lens.fill(0);
        self.fill.fill(0);
        self.spill.iter_mut().for_each(|spill| *spill = Vec::new());
        // SAFETY: every list is empty now, so no slot of the region is read before it is
        // written again.
        unsafe { mem::release(self.region.at.as_ptr().cast(), self.region.bytes()) };
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Copies `bytes` (whole cache lines) from one line boundary to another without reading the
/// destination's lines into the cache first.
///
/// # Safety
/// Both ranges are valid, apart, and start on a 64-byte boundary; `bytes` is a multiple of 64.
#[inline(always)]
unsafe fn stream(from: *const u8, to: *mut u8, bytes: usize) {
    debug_assert!(bytes % LINE == 0 && from as usize % LINE == 0 && to as usize % LINE == 0);
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    // SAFETY: the caller's contract; AVX2 is compiled in.
    unsafe {
        use core::arch::x86_64::{_mm256_load_si256, _mm256_stream_si256};
        let mut at = 0;
        while at < bytes {
            _mm256_stream_si256(to.add(at).cast(), _mm256_load_si256(from.add(at).cast()));
            _mm256_stream_si256(to.add(at + 32).cast(), _mm256_load_si256(from.add(at + 32).cast()));
            at += LINE;
        }
    }
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    // SAFETY: the caller's contract; SSE2 is part of x86-64.
    unsafe {
        use core::arch::x86_64::{_mm_load_si128, _mm_stream_si128};
        let mut at = 0;
        while at < bytes {
            _mm_stream_si128(to.add(at).cast(), _mm_load_si128(from.add(at).cast()));
            at += 16;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    // SAFETY: the caller's contract.
    unsafe {
        std::ptr::copy_nonoverlapping(from, to, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::Parts;

    /// Every staging a record size can meet: plain, one line, the default, more than a list
    /// ever holds.
    const STAGES: [usize; 5] = [0, 1, 64, 256, 4096];

    fn fill_and_check<T: Copy + PartialEq + std::fmt::Debug>(make: impl Fn(u64) -> T) {
        for stage_bytes in STAGES {
            for room in [1, 2, 5, 64] {
                let mut parts = Parts::with_stage_bytes(7, room, stage_bytes);
                let mut expected = vec![Vec::new(); 7];
                for n in 0..1000u64 {
                    let list = ((n * 2_654_435_761) >> 7) as usize % 7;
                    parts.push(list, make(n));
                    expected[list].push(make(n));
                }
                parts.fence();
                for (list, want) in expected.iter().enumerate() {
                    let got: Vec<T> = parts.pieces(list).concat();
                    assert_eq!(&got, want, "stage {stage_bytes}, room {room}, list {list}");
                    assert_eq!(parts.len_of(list), want.len());
                }
                assert_eq!(parts.len(), 1000);
                assert_eq!(parts.iter().count(), 1000);
                // A staged list's room is rounded up to whole stages, so it may not spill.
                if stage_bytes == 0 {
                    assert_eq!(parts.spilled() > 0, room < 1000 / 7);
                }
                parts.release();
                assert!(parts.is_empty() && parts.spilled() == 0);
                parts.push(3, make(5));
                assert_eq!(parts.pieces(3).concat(), [make(5)]);
            }
        }
    }

    #[test]
    fn lists_keep_order_and_spill() {
        // 8, 16, 24 and 32 bytes: the record sizes in use, and one that does not divide a line.
        fill_and_check(|n| n);
        fill_and_check(|n| [n, !n]);
        fill_and_check(|n| [n, !n, n ^ 0x5555]);
        fill_and_check(|n| [n, !n, n ^ 0x5555, n.rotate_left(17)]);
        // Smaller alignments and an odd size.
        fill_and_check(|n| n as u8);
        fill_and_check(|n| [n as u8, (n >> 8) as u8, 7]);
    }

    #[test]
    fn empty_lists() {
        for stage_bytes in STAGES {
            let parts: Parts<u32> = Parts::with_stage_bytes(3, 4, stage_bytes);
            assert!(parts.is_empty());
            assert!((0..3).all(|list| parts.pieces(list).iter().all(|piece| piece.is_empty())));
        }
    }
}
