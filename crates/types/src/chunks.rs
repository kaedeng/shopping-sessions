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
/// Blocks `for_each` of pooled lists asks for ahead of the one it is at.
const POOL_AHEAD: usize = 8;

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
    /// Records `push_pair_at` appended beyond one a pair: a pair's second, and each pad.
    extra: usize,
    /// `pool`: the lists' blocks go one after another into one chunk at a time, the one given
    /// last first, each list keeping where its blocks are (`index`) instead of chunks of its
    /// own; `slot..slot_end` is what is left of that chunk.
    pooled: bool,
    index: Vec<Vec<usize>>,
    slot: usize,
    slot_end: usize,
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
            extra: 0,
            pooled: false,
            index: Vec::new(),
            slot: 0,
            slot_end: 0,
            _records: PhantomData,
        }
    }

    /// Has these lists, empty, write their blocks into a pool (`push_as::<true>`): every
    /// list's next block goes into the next free 192 bytes of one chunk, the chunk given last
    /// (`give`) before any other, and each list keeps the blocks' addresses in order, room for
    /// `records` records each made now. For lists written a block at a time from many places
    /// (a worker's person buckets): a chunk of a list of its own takes its 21 blocks over a
    /// hundred dedup tasks, so a chunk just read is warm in the caches for the first of them
    /// only; a pool's chunk is written in one go while it is still warm, with plain stores.
    pub fn pool(&mut self, records: usize) {
        assert!(self.is_empty() && !self.pooled, "Chunks::pool on lists in use");
        let blocks = records * Self::SIZE / STAGE + 2;
        self.pooled = true;
        self.index = (0..self.lists()).map(|_| Vec::with_capacity(blocks)).collect();
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
        // SAFETY: the caller's.
        unsafe { self.push_at_as::<false>(list, rec) }
    }

    /// `push_at` into lists that are `POOLED` (`pool`) or not.
    ///
    /// # Safety
    /// `list < self.lists()`, and `POOLED` is whether `pool` was called.
    #[inline(always)]
    pub unsafe fn push_at_as<const POOLED: bool>(&mut self, list: usize, rec: T) {
        debug_assert!(list < self.lists() && POOLED == self.pooled);
        // SAFETY: the caller keeps `list` in range; a list's staging holds `STAGE` bytes, and
        // `fill` is a multiple of the record's size below it once a full block is written out.
        unsafe {
            let mut fill = *self.fill.get_unchecked(list) as usize;
            if fill == STAGE {
                if POOLED { self.flush_pooled(list) } else { self.flush(list) }
                fill = 0;
            }
            self.stage.ptr.add(list * STAGE + fill).cast::<T>().write_unaligned(rec);
            *self.fill.get_unchecked_mut(list) = (fill + Self::SIZE) as u16;
        }
    }

    /// Appends `first` and `second` to list `list` so that both lie in one staging block,
    /// and so in one piece of `for_each`: where the block has room for one record only,
    /// `pad` goes there first. For a row that is two records, read as one.
    ///
    /// # Safety
    /// `list < self.lists()`.
    #[inline(always)]
    pub unsafe fn push_pair_at(&mut self, list: usize, pad: T, first: T, second: T) {
        debug_assert!(STAGE >= 2 * Self::SIZE);
        // SAFETY: the caller keeps `list` in range.
        unsafe {
            if *self.fill.get_unchecked(list) as usize == STAGE - Self::SIZE {
                self.push_at(list, pad);
                self.extra += 1;
            }
            // A full block is written out by the first of them: both are in the next.
            self.push_at(list, first);
            self.push_at(list, second);
            self.extra += 1;
        }
    }

    /// Records `push_pair_at` appended beyond one a pair: `len` less this is the pairs and
    /// the records pushed alone.
    pub fn extra(&self) -> usize {
        self.extra
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
        self.push_as::<false>(list, rec)
    }

    /// `push` into lists that are `POOLED` (`pool`) or not; panics if that is wrong.
    #[inline(always)]
    pub fn push_as<const POOLED: bool>(&mut self, list: usize, rec: T) {
        assert!(list < self.lists() && POOLED == self.pooled, "list {list} of {}, pooled {}", self.lists(), self.pooled);
        // SAFETY: checked above.
        unsafe { self.push_at_as::<POOLED>(list, rec) }
    }

    /// List `list`'s full staging block to its chunk, with streaming stores.
    ///
    /// # Safety
    /// `list < self.lists()`, and its staging holds `STAGE` bytes.
    #[inline(never)]
    unsafe fn flush(&mut self, list: usize) {
        debug_assert!(!self.pooled);
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

    /// `flush` of pooled lists (`pool`): the block to the pool's next 192 bytes with plain
    /// stores (the chunk was given just after it was read, so its lines are in this core's
    /// caches, which a streaming store would first have to empty of them), its address to the
    /// list's index.
    ///
    /// # Safety
    /// As `flush`.
    #[inline(never)]
    unsafe fn flush_pooled(&mut self, list: usize) {
        debug_assert!(self.pooled);
        if self.slot == self.slot_end {
            let chunk = self.take();
            (self.slot, self.slot_end) = (chunk, chunk + LINK);
        }
        let to = self.slot;
        // SAFETY: the caller's contract; the pool's chunk has room for a block below its link,
        // and no list has a block there yet.
        unsafe {
            core::ptr::copy_nonoverlapping(self.stage.ptr.add(list * STAGE), to as *mut u8, STAGE);
            self.index.get_unchecked_mut(list).push(to);
            *self.blocks.get_unchecked_mut(list) += 1;
            *self.fill.get_unchecked_mut(list) = 0;
        }
        self.slot = to + STAGE;
    }

    /// A chunk to write: the one given last, else the block's next.
    #[cold]
    #[inline(never)]
    fn take(&mut self) -> usize {
        if let Some(new) = self.spare.pop() {
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
        new
    }

    /// A new chunk for list `list`, linked after the one whose link is at `link` (`NONE` for
    /// the list's first).
    #[cold]
    #[inline(never)]
    fn chunk(&mut self, list: usize, link: usize) -> usize {
        let new = self.take();
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
        assert!(!self.pooled, "Chunks::chunks_of on pooled lists");
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
        assert!(!self.pooled, "Chunks::for_each_chunk on pooled lists");
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
        if self.pooled {
            return self.for_each_pooled(list, piece);
        }
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

    /// `for_each` of pooled lists: a block at a time, the blocks a few on asked for.
    #[inline(always)]
    fn for_each_pooled<'a>(&'a self, list: usize, mut piece: impl FnMut(&'a [T])) {
        let index = &self.index[list];
        for (at, &block) in index.iter().enumerate() {
            if let Some(&coming) = index.get(at + POOL_AHEAD) {
                for line in [0, 64, 128] {
                    crate::mem::prefetch((coming + line) as *const u8);
                }
            }
            // SAFETY: each address is of a full block written by `flush_pooled`.
            piece(unsafe { std::slice::from_raw_parts(block as *const T, STAGE / Self::SIZE) });
        }
        let staged = self.fill[list] as usize;
        if staged != 0 {
            // SAFETY: the staging holds `staged` bytes of the list's records.
            piece(unsafe { std::slice::from_raw_parts(self.stage.ptr.add(list * STAGE).cast::<T>(), staged / Self::SIZE) });
        }
    }

    /// Where the first `sixteenths` of the chunks the lists have taken from their first block
    /// end, on a 2 MB step: `(start, cut)`, the block's chunks from `start` to below `cut`.
    /// For `give` of only those, and `release_above` of the rest.
    pub fn kept(&self, sixteenths: usize) -> (usize, usize) {
        let first = &self.arenas[0];
        let start = first.ptr as usize;
        let used = if self.arenas.len() == 1 { self.next - start } else { first.layout.size() };
        (start, (start + (used * sixteenths.min(16) / 16).next_multiple_of(HUGE)).min(start + first.layout.size()))
    }

    /// Empties the lists and hands back to the kernel the chunks they took from `cut` (of
    /// `kept`) on: of the first block, and every further block. The chunks below `cut` are
    /// left as they are (`give` may have handed them to other lists), and no list takes one of
    /// the first block again.
    ///
    /// # Safety
    /// `cut` is of `kept`, and nothing reads a chunk of these lists from `cut` on again.
    pub unsafe fn release_above(&mut self, cut: usize) {
        self.fill.fill(0);
        self.extra = 0;
        self.cur.fill(NONE);
        self.head.fill(0);
        self.blocks.fill(0);
        self.spare.clear();
        let first = &self.arenas[0];
        let end = first.ptr as usize + first.layout.size();
        let used = if self.arenas.len() == 1 { self.next } else { end };
        debug_assert!((first.ptr as usize..=end).contains(&cut));
        // SAFETY: the caller's: nothing reads these chunks again.
        unsafe {
            if used > cut {
                crate::mem::release(cut as *const u8, used - cut);
            }
            for more in &self.arenas[1..] {
                crate::mem::release(more.ptr, more.layout.size());
            }
        }
        self.arenas.truncate(1);
        (self.next, self.end) = (end, end);
    }

    /// Chunks the lists took from the first block (not given ones).
    pub fn fresh(&self) -> usize {
        let first = &self.arenas[0];
        if self.arenas.len() == 1 { (self.next - first.ptr as usize) / CHUNK } else { first.layout.size() / CHUNK }
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
        self.extra = 0;
        self.cur.fill(NONE);
        self.head.fill(0);
        self.blocks.fill(0);
        // Chunks that were given are other lists' pages: not handed back here, and not kept.
        self.spare.clear();
        self.index.iter_mut().for_each(Vec::clear);
        (self.slot, self.slot_end) = (0, 0);
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

    /// Pooled lists hold what was appended, write the chunks given last first, a block of
    /// each once, and keep them through `release_above` of the lists they came from.
    #[test]
    fn pooled_lists_write_given_chunks_and_keep_them() {
        let mut from: Chunks<[u64; 2]> = Chunks::new(4, 8 << 20);
        for n in 0..200_000u64 {
            from.push((n % 4) as usize, [n, !n]);
        }
        from.fence();
        let (start, cut) = from.kept(8);
        assert!(cut > start && cut % super::HUGE == 0);
        let mut to: Chunks<u64> = Chunks::new(5, 1 << 20);
        to.pool(1000);
        let mut given = Vec::new();
        let mut expected = vec![Vec::new(); 5];
        for list in 0..4 {
            from.chunks_of(list, |chunk| {
                if (start..cut).contains(&chunk) {
                    given.push(chunk);
                    // SAFETY: `from` outlives `to`, and nothing reads its chunks below `cut` again.
                    unsafe { to.give(chunk) };
                }
            });
            for n in 0..15_000u64 {
                let rec = n * 7 + list as u64;
                to.push_as::<true>((n % 5) as usize, rec);
                expected[(n % 5) as usize].push(rec);
            }
        }
        // SAFETY: nothing reads `from`'s lists again.
        unsafe { from.release_above(cut) };
        let mut blocks = Vec::new();
        for (list, want) in expected.iter().enumerate() {
            let mut got = Vec::new();
            to.for_each(list, |piece| got.extend_from_slice(piece));
            assert_eq!(&got, want, "list {list}");
            assert_eq!(to.len_of(list), want.len());
            blocks.extend(to.index[list].iter().copied());
        }
        // 60,000 records are 2,500 blocks, 120 chunks: all given ones while there were.
        let chunks: std::collections::BTreeSet<usize> = blocks.iter().map(|block| block & !(super::CHUNK - 1)).collect();
        assert!(chunks.iter().all(|chunk| given.contains(chunk)) || to.fresh() > 0);
        blocks.sort_unstable();
        blocks.dedup();
        assert_eq!(blocks.len(), to.index.iter().map(Vec::len).sum::<usize>());
        // More than the given chunks hold: the rest from the block.
        for n in 0..(given.len() as u64 * 600) {
            to.push_as::<true>(0, n);
        }
        assert!(to.fresh() > 0);
        let mut count = 0;
        to.for_each(0, |piece| count += piece.len());
        assert_eq!(count, to.len_of(0));
    }

    /// A pair's two records are always in one piece, whatever was pushed before it.
    #[test]
    fn pairs_stay_in_one_piece() {
        for lead in 0..40u64 {
            let mut chunks: Chunks<[u64; 2]> = Chunks::new(3, 1 << 20);
            let mut rows = 0u64;
            for n in 0..5_000u64 {
                let list = (n % 3) as usize;
                // SAFETY: the list is below 3.
                unsafe {
                    if n < lead || n % 7 != 0 { chunks.push_at(list, [n, 0]) } else { chunks.push_pair_at(list, [u64::MAX, 0], [n, 1], [n, 2]) }
                }
                rows += 1;
            }
            chunks.fence();
            assert_eq!(chunks.len() - chunks.extra(), rows as usize);
            let mut seen = 0u64;
            for list in 0..3 {
                chunks.for_each(list, |piece| {
                    let mut at = 0;
                    while at < piece.len() {
                        match piece[at] {
                            [u64::MAX, 0] => at += 1,
                            [n, 1] => {
                                assert_eq!(piece[at + 1], [n, 2], "a pair cut in two");
                                (seen, at) = (seen + 1, at + 2);
                            }
                            [_, 0] => (seen, at) = (seen + 1, at + 1),
                            other => panic!("a second record alone: {other:?}"),
                        }
                    }
                });
            }
            assert_eq!(seen, rows);
        }
    }

    #[test]
    fn lists_keep_order_across_chunks_and_blocks() {
        check(|n| n);
        check(|n| Rec12(n, !n as u32));
        check(|n| [n, !n]);
        check(|n| [n, !n, n * 7]);
    }
}
