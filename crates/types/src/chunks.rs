//! Many append-only lists as chains of 4 KB chunks, handed out one after another from one
//! block per worker: the positional parse's lists (`ingest::pos`).
//!
//! `Parts` gives every list the same room, carved out of one allocation. The lists of the
//! positional parse fill unevenly (a worker's share of a partition's rows, a person range's
//! clicks), so the room was half again what the rows needed, and with 2 MB pages the kernel
//! zeroed all of it: 550 MB for 345 MB of event rows at 20M events, a tenth of the run's
//! cycles spent clearing pages. Here a list takes a chunk when it needs one, so the block
//! fills from its first byte with nothing between, and what is zeroed is what is written.
//!
//! Records are staged per list in 192 bytes (three cache lines; 16, 12 or 8 records of 12, 16
//! or 24 bytes) and written to the list's chunk with streaming stores, as `Parts` does when it
//! combines. A chunk holds 21 such blocks and, in its last 64 bytes, the address of the next.
//! A writer calls `fence` before other threads read what it wrote, and `settle` before its own
//! lists are read: that moves each list's staged records into its chunk.

use std::alloc::{Layout, alloc, dealloc};
use std::marker::PhantomData;

/// Bytes staged per list before they go to its chunk.
const STAGE: usize = 192;
const CHUNK: usize = 4096;
/// Bytes of records in a chunk; the next chunk's address follows them.
const LINK: usize = 4032;
/// The block is aligned to this, so that it fills whole huge pages from its first chunk.
const HUGE: usize = 2 << 20;
/// What a list's `cur` holds before its first chunk: the offset of a chunk's link, at no chunk.
const NONE: usize = LINK;

const _: () = assert!(LINK % STAGE == 0 && LINK + size_of::<usize>() <= CHUNK);

/// A block of chunks.
struct Arena {
    ptr: *mut u8,
    layout: Layout,
}

impl Arena {
    fn new(bytes: usize) -> Arena {
        let layout = Layout::from_size_align(bytes.next_multiple_of(CHUNK).max(CHUNK), HUGE).expect("Chunks: layout");
        // SAFETY: `layout` has a non-zero size.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Arena { ptr, layout }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// `lists` append-only lists of `T`, a record of 8, 12, 16, 24 or 32 bytes.
pub struct Chunks<T> {
    /// `lists * STAGE` bytes: list `i` stages in bytes `i * STAGE ..`.
    stage: Arena,
    /// Bytes staged per list, at most `STAGE`.
    fill: Vec<u16>,
    /// Where each list's next block goes: inside its last chunk, or at that chunk's link when
    /// it is full; `NONE` before the first.
    cur: Vec<usize>,
    /// Each list's first chunk, 0 for none.
    head: Vec<usize>,
    /// Blocks each list has in its chunks.
    blocks: Vec<u32>,
    /// The chunks: the next to hand out and one past the last of the current block; the
    /// blocks themselves, the first sized for the worker's share, more if that runs out.
    next: usize,
    end: usize,
    arenas: Vec<Arena>,
    /// Chunks of other lists that those are done with (`give`): taken before the block's.
    spare: Vec<usize>,
    _records: PhantomData<T>,
}

// SAFETY: plain owned memory, as a `Vec<T>` is; the addresses point into it.
unsafe impl<T: Send> Send for Chunks<T> {}
unsafe impl<T: Sync> Sync for Chunks<T> {}

impl<T: Copy> Chunks<T> {
    const SIZE: usize = {
        assert!(STAGE % size_of::<T>() == 0 && align_of::<T>() <= 8 && size_of::<T>() >= 8);
        size_of::<T>()
    };

    /// `lists` empty lists, with a block of `bytes` to take chunks from. No page of it is
    /// touched until a list's first block is full; more blocks are taken if it runs out.
    pub fn new(lists: usize, bytes: usize) -> Chunks<T> {
        assert!(lists > 0 && Self::SIZE > 0, "Chunks::new({lists}, {bytes})");
        let arena = Arena::new(bytes);
        Chunks {
            stage: Arena::new(lists * STAGE),
            fill: vec![0; lists],
            cur: vec![NONE; lists],
            head: vec![0; lists],
            blocks: vec![0; lists],
            next: arena.ptr as usize,
            end: arena.ptr as usize + arena.layout.size(),
            arenas: vec![arena],
            spare: Vec::new(),
            _records: PhantomData,
        }
    }

    pub fn lists(&self) -> usize {
        self.fill.len()
    }

    /// Bytes of the block the chunks come from first.
    pub fn capacity(&self) -> usize {
        self.arenas[0].layout.size()
    }

    /// Appends `rec` to list `list`.
    ///
    /// # Safety
    /// `list < self.lists()`.
    #[inline(always)]
    pub unsafe fn push_at(&mut self, list: usize, rec: T) {
        debug_assert!(list < self.lists());
        // SAFETY: the caller keeps `list` in range; a list's staging holds `STAGE` bytes, and
        // `fill` is a multiple of the record's size below it once a full block is written out.
        unsafe {
            let mut fill = *self.fill.get_unchecked(list) as usize;
            if fill == STAGE {
                self.flush(list);
                fill = 0;
            }
            self.stage.ptr.add(list * STAGE + fill).cast::<T>().write_unaligned(rec);
            *self.fill.get_unchecked_mut(list) = (fill + Self::SIZE) as u16;
        }
    }

    /// Appends `count` records: record `at` is `record(at)`, its list first. `push_at` in a
    /// loop of its own, the lists' fields in registers.
    ///
    /// # Safety
    /// Every list `record` returns is below `self.lists()`.
    #[inline(always)]
    pub unsafe fn extend(&mut self, count: usize, mut record: impl FnMut(usize) -> (usize, T)) {
        let (fills, stage) = (self.fill.as_mut_ptr(), self.stage.ptr);
        // SAFETY: as `push_at`, record by record; `flush` moves neither array.
        unsafe {
            for at in 0..count {
                let (list, rec) = record(at);
                debug_assert!(list < self.fill.len());
                let mut fill = *fills.add(list) as usize;
                if fill == STAGE {
                    self.flush(list);
                    fill = 0;
                }
                stage.add(list * STAGE + fill).cast::<T>().write_unaligned(rec);
                *fills.add(list) = (fill + Self::SIZE) as u16;
            }
        }
    }

    /// Appends `rec` to list `list`; panics if there is no such list.
    pub fn push(&mut self, list: usize, rec: T) {
        assert!(list < self.lists(), "list {list} of {}", self.lists());
        // SAFETY: checked above.
        unsafe { self.push_at(list, rec) }
    }

    /// List `list`'s full staging block to its chunk, with streaming stores.
    ///
    /// # Safety
    /// `list < self.lists()`, and its staging holds `STAGE` bytes.
    #[inline(never)]
    unsafe fn flush(&mut self, list: usize) {
        // SAFETY: the caller's contract; a chunk has room for a block wherever `cur` is not
        // at its link.
        unsafe {
            let mut to = *self.cur.get_unchecked(list);
            if to & (CHUNK - 1) == LINK {
                to = self.chunk(list, to);
            }
            stream(self.stage.ptr.add(list * STAGE), to as *mut u8);
            *self.cur.get_unchecked_mut(list) = to + STAGE;
            *self.blocks.get_unchecked_mut(list) += 1;
            *self.fill.get_unchecked_mut(list) = 0;
        }
    }

    /// A new chunk for list `list`, linked after the one whose link is at `link` (`NONE` for
    /// the list's first).
    #[cold]
    #[inline(never)]
    fn chunk(&mut self, list: usize, link: usize) -> usize {
        if let Some(new) = self.spare.pop() {
            if link == NONE {
                self.head[list] = new;
            } else {
                // SAFETY: `link` is the link of the list's last chunk.
                unsafe { (link as *mut usize).write(new) };
            }
            return new;
        }
        if self.next == self.end {
            // The worker took more of the rows than its block was sized for.
            let arena = Arena::new(16 << 20);
            (self.next, self.end) = (arena.ptr as usize, arena.ptr as usize + arena.layout.size());
            self.arenas.push(arena);
        }
        let new = self.next;
        self.next += CHUNK;
        // `SS_POPULATE`, `SS_COLLAPSE=all`: the chunks come one after another, so the block's
        // next stretch is backed in one call when the first chunk of it is taken
        // (`mem::back`).
        let window = crate::mem::back_window();
        if window != 0 && new & (window - 1) == 0 {
            // SAFETY: whole chunks of the block, from the one handed out now: none of them
            // holds a record yet.
            unsafe { crate::mem::back(new as *mut u8, window.min(self.end - new)) };
        }
        if link == NONE {
            self.head[list] = new;
        } else {
            // SAFETY: `link` is the link of the list's last chunk, inside a block.
            unsafe { (link as *mut usize).write(new) };
        }
        new
    }

    /// Hands these lists a chunk to write that some other list was read from and is done
    /// with: a list's next chunk is such a one before it is one of the block's.
    ///
    /// A chunk of the block is a page nothing has touched. Where the machine has no huge
    /// pages (the scorer), the kernel makes it one of its own 4 KB pages, zeroed, when it is
    /// first written, and takes it back, one more page under one lock, when the run ends: at
    /// 20M events the lists the dedup stage writes were 80,000 of a run's 390,000 pages. A
    /// chunk the stage has just read is a page the process already has.
    ///
    /// # Safety
    /// `chunk` is the address of a whole chunk (`chunks_of`, `for_each_chunk`) of lists that
    /// outlive these, that nothing reads again, and that is not given twice.
    pub unsafe fn give(&mut self, chunk: usize) {
        debug_assert!(chunk != 0 && chunk & (CHUNK - 1) == 0);
        self.spare.push(chunk);
    }

    /// Chunks given and not yet taken.
    pub fn spare(&self) -> usize {
        self.spare.len()
    }

    /// The address of every chunk of list `list`, for `give` once the list has been read.
    pub fn chunks_of(&self, list: usize, mut each: impl FnMut(usize)) {
        let head = self.head[list];
        if head != 0 {
            let last = (self.cur[list] - 1) & !(CHUNK - 1);
            let mut chunk = head;
            while chunk != last {
                // SAFETY: a chunk but the last is full and holds the next one's address.
                let next = unsafe { ((chunk + LINK) as *const usize).read() };
                each(chunk);
                chunk = next;
            }
            each(last);
        }
    }

    /// `for_each`, and with each piece the address of the chunk it is (0 for what is staged):
    /// once `piece` has returned for the piece after it, nothing of these lists reads that
    /// chunk again.
    #[inline(always)]
    pub fn for_each_chunk<'a>(&'a self, list: usize, mut piece: impl FnMut(&'a [T], usize)) {
        let head = self.head[list];
        // SAFETY: as `for_each`.
        unsafe {
            if head != 0 {
                let cur = self.cur[list];
                let last = (cur - 1) & !(CHUNK - 1);
                let mut chunk = head;
                while chunk != last {
                    let next = ((chunk + LINK) as *const usize).read();
                    crate::mem::prefetch(next as *const u8);
                    crate::mem::prefetch((next + 64) as *const u8);
                    crate::mem::prefetch((next + LINK) as *const u8);
                    piece(std::slice::from_raw_parts(chunk as *const T, LINK / Self::SIZE), chunk);
                    chunk = next;
                }
                piece(std::slice::from_raw_parts(last as *const T, (cur - last) / Self::SIZE), last);
            }
            let staged = self.fill[list] as usize;
            if staged != 0 {
                piece(std::slice::from_raw_parts(self.stage.ptr.add(list * STAGE).cast::<T>(), staged / Self::SIZE), 0);
            }
        }
    }

    /// Makes this thread's streamed lines visible to other threads: call it after a task's
    /// last record, before another thread may read the lists.
    #[inline]
    pub fn fence(&self) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE is part of x86-64 (newer compilers call this intrinsic safe).
        #[allow(unused_unsafe)]
        unsafe {
            core::arch::x86_64::_mm_sfence()
        }
    }

    /// List `list` in order of appending, a piece at a time: its chunks, then what is staged.
    #[inline(always)]
    pub fn for_each<'a>(&'a self, list: usize, mut piece: impl FnMut(&'a [T])) {
        let head = self.head[list];
        // SAFETY: a list's chunks hold records up to `cur`, each chunk but the last full and
        // linked to the next; chunks are aligned to 4 KB, the staging to 2 MB.
        unsafe {
            if head != 0 {
                let cur = self.cur[list];
                let last = (cur - 1) & !(CHUNK - 1);
                let mut chunk = head;
                while chunk != last {
                    // The next chunk is anywhere in the block: asked for now, its first lines
                    // and its link are there when this one is read.
                    let next = ((chunk + LINK) as *const usize).read();
                    crate::mem::prefetch(next as *const u8);
                    crate::mem::prefetch((next + 64) as *const u8);
                    crate::mem::prefetch((next + LINK) as *const u8);
                    piece(std::slice::from_raw_parts(chunk as *const T, LINK / Self::SIZE));
                    chunk = next;
                }
                piece(std::slice::from_raw_parts(last as *const T, (cur - last) / Self::SIZE));
            }
            let staged = self.fill[list] as usize;
            if staged != 0 {
                piece(std::slice::from_raw_parts(self.stage.ptr.add(list * STAGE).cast::<T>(), staged / Self::SIZE));
            }
        }
    }

    /// Records in list `list`.
    pub fn len_of(&self, list: usize) -> usize {
        self.blocks[list] as usize * (STAGE / Self::SIZE) + self.fill[list] as usize / Self::SIZE
    }

    /// Records in all lists.
    pub fn len(&self) -> usize {
        (0..self.lists()).map(|list| self.len_of(list)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes a byte into every page of the staging and of the first `bytes` of the block
    /// until `stop` says so, so that the kernel backs them now: for a thread with nothing else
    /// to do while the inputs stage ends (see `Parts::touch`). Nothing reads these bytes
    /// before a record overwrites them.
    pub fn touch(&mut self, bytes: usize, stop: &impl Fn() -> bool) {
        let block = &self.arenas[0];
        for (ptr, len) in [(self.stage.ptr, self.stage.layout.size()), (block.ptr, bytes.min(block.layout.size()))] {
            let mut at = 0;
            while at < len {
                if stop() {
                    return;
                }
                // SAFETY: inside a block this owns, which holds no record yet.
                unsafe { ptr.add(at).write(0) };
                at += CHUNK;
            }
        }
    }

    /// Slot `free-early`: empties every list and hands the chunks' pages back to the kernel on
    /// the calling thread (`mem::release`). The lists can be filled again afterwards.
    pub fn release(&mut self) {
        self.fill.fill(0);
        self.cur.fill(NONE);
        self.head.fill(0);
        self.blocks.fill(0);
        // Chunks that were given are other lists' pages: not handed back here, and not kept.
        self.spare.clear();
        // The first block's used part; any further block whole.
        let first = &self.arenas[0];
        let used = if self.arenas.len() == 1 { self.next - first.ptr as usize } else { first.layout.size() };
        // SAFETY: every list is empty now, so nothing reads a chunk before it is written again.
        unsafe { crate::mem::release(first.ptr, used) };
        self.arenas.truncate(1);
        let first = &self.arenas[0];
        (self.next, self.end) = (first.ptr as usize, first.ptr as usize + first.layout.size());
    }
}

/// Copies a staging block (`STAGE` bytes at `from`, aligned to 64) to `to`, also aligned to
/// 64, with streaming stores where the target has them.
///
/// # Safety
/// Both ranges are valid and do not overlap.
#[inline(always)]
unsafe fn stream(from: *const u8, to: *mut u8) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use core::arch::x86_64::{__m256i, _mm256_load_si256, _mm256_stream_si256};
        let (a, b, c) = (_mm256_load_si256(from.cast::<__m256i>()), _mm256_load_si256(from.add(32).cast::<__m256i>()), _mm256_load_si256(from.add(64).cast::<__m256i>()));
        let (d, e, f) = (_mm256_load_si256(from.add(96).cast::<__m256i>()), _mm256_load_si256(from.add(128).cast::<__m256i>()), _mm256_load_si256(from.add(160).cast::<__m256i>()));
        _mm256_stream_si256(to.cast::<__m256i>(), a);
        _mm256_stream_si256(to.add(32).cast::<__m256i>(), b);
        _mm256_stream_si256(to.add(64).cast::<__m256i>(), c);
        _mm256_stream_si256(to.add(96).cast::<__m256i>(), d);
        _mm256_stream_si256(to.add(128).cast::<__m256i>(), e);
        _mm256_stream_si256(to.add(160).cast::<__m256i>(), f);
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    unsafe {
        core::ptr::copy_nonoverlapping(from, to, STAGE)
    }
}

#[cfg(test)]
mod tests {
    use super::Chunks;

    #[derive(Clone, Copy, Debug, PartialEq)]
    #[repr(C)]
    struct Rec12(u64, u32);

    fn check<T: Copy + PartialEq + std::fmt::Debug>(make: impl Fn(u64) -> T) {
        for (lists, records, bytes) in [(1usize, 0u64, 0usize), (1, 1, 0), (7, 1000, 4096), (7, 100_000, 4096), (3, 50_000, 1 << 20), (700, 300_000, 64 << 10)] {
            let mut chunks: Chunks<T> = Chunks::new(lists, bytes);
            let mut expected = vec![Vec::new(); lists];
            let list_of = |n: u64| ((n.wrapping_mul(2_654_435_761)) >> 7) as usize % lists;
            // Half pushed one at a time, half through `extend`.
            for n in 0..records / 2 {
                chunks.push(list_of(n), make(n));
                expected[list_of(n)].push(make(n));
            }
            let from = records / 2;
            // SAFETY: the lists are below `lists`.
            unsafe { chunks.extend((records - from) as usize, |at| (list_of(from + at as u64), make(from + at as u64))) };
            for n in from..records {
                expected[list_of(n)].push(make(n));
            }
            chunks.fence();
            for (list, want) in expected.iter().enumerate() {
                let mut got = Vec::new();
                chunks.for_each(list, |piece| got.extend_from_slice(piece));
                assert_eq!(&got, want, "list {list} of {lists}, {records} records");
                assert_eq!(chunks.len_of(list), want.len());
            }
            assert_eq!(chunks.len(), records as usize);
            assert_eq!(chunks.is_empty(), records == 0);
            // Emptied, the lists fill again from the block's first chunk.
            chunks.release();
            assert!(chunks.is_empty());
            for n in 0..40u64 {
                chunks.push(lists - 1, make(n));
            }
            let mut got = Vec::new();
            chunks.for_each(lists - 1, |piece| got.extend_from_slice(piece));
            assert_eq!(got, (0..40).map(&make).collect::<Vec<_>>());
        }
    }

    /// Lists written into the chunks of lists that were read hold what was appended, take
    /// those chunks before any of their own block, and take none twice.
    #[test]
    fn given_chunks_are_written_before_the_blocks_own() {
        let mut from: Chunks<[u64; 3]> = Chunks::new(5, 1 << 20);
        for n in 0..20_000u64 {
            from.push((n % 5) as usize, [n, !n, n * 3]);
        }
        from.fence();
        let mut to: Chunks<u64> = Chunks::new(3, 1 << 20);
        let (mut given, mut read) = (Vec::new(), 0u64);
        for list in 0..5 {
            // Read with the chunks' addresses, and given once read.
            let mut pieces = Vec::new();
            from.for_each_chunk(list, |piece, chunk| {
                read += piece.len() as u64;
                pieces.push(chunk);
            });
            let mut chunks = Vec::new();
            from.chunks_of(list, |chunk| chunks.push(chunk));
            // The pieces are the chunks, then what is staged.
            assert_eq!(pieces.iter().copied().filter(|&chunk| chunk != 0).collect::<Vec<_>>(), chunks);
            for chunk in chunks {
                // SAFETY: `from` outlives `to`, and its list is not read again.
                unsafe { to.give(chunk) };
                given.push(chunk);
            }
        }
        assert_eq!(read, 20_000);
        let had = to.spare();
        assert_eq!(had, given.len());
        let mut expected = vec![Vec::new(); 3];
        for n in 0..30_000u64 {
            to.push((n % 3) as usize, n ^ 0x55);
            expected[(n % 3) as usize].push(n ^ 0x55);
        }
        to.fence();
        let mut used = Vec::new();
        for (list, want) in expected.iter().enumerate() {
            let mut got = Vec::new();
            to.for_each(list, |piece| got.extend_from_slice(piece));
            assert_eq!(&got, want, "list {list}");
            to.chunks_of(list, |chunk| used.push(chunk));
        }
        // 30,000 records of 8 bytes are 60 chunks: all of them given ones, none twice.
        assert!(!used.is_empty() && used.len() <= had);
        used.sort_unstable();
        used.dedup();
        assert_eq!(used.len() + to.spare(), had);
        assert!(used.iter().all(|chunk| given.contains(chunk)));
    }

    #[test]
    fn lists_keep_order_across_chunks_and_blocks() {
        check(|n| n);
        check(|n| Rec12(n, !n as u32));
        check(|n| [n, !n]);
        check(|n| [n, !n, n * 7]);
    }
}
