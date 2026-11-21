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
//! of records; `pieces` returns the carved part, then the spill.

/// `lists` append-only lists of `T`, each with `room` records carved out of one allocation.
pub struct Parts<T> {
    /// Capacity `lists * room`, length 0: list `i` owns slots `i * room ..`, written through the
    /// raw pointer, so the `Vec` only owns the memory.
    region: Vec<T>,
    room: usize,
    lens: Vec<u32>,
    spill: Vec<Vec<T>>,
}

impl<T: Copy> Parts<T> {
    /// Allocates the region; no page of it is touched until a record is written there.
    pub fn new(lists: usize, room: usize) -> Parts<T> {
        assert!(lists > 0 && room > 0 && room <= u32::MAX as usize, "Parts::new({lists}, {room})");
        let slots = lists.checked_mul(room).expect("Parts: lists * room overflows");
        Parts { region: Vec::with_capacity(slots), room, lens: vec![0; lists], spill: (0..lists).map(|_| Vec::new()).collect() }
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
        // SAFETY: the caller keeps `list` in range.
        let len = unsafe { self.lens.get_unchecked_mut(list) };
        if (*len as usize) < self.room {
            // SAFETY: `list * room + len < lists * room`, the region's capacity, and each slot is
            // written at most once before `pieces` reads it.
            unsafe { self.region.as_mut_ptr().add(list * self.room + *len as usize).write(rec) };
            *len += 1;
        } else {
            self.spill(list, rec);
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

    /// List `list` in order of appending: the carved part, then the spill (usually empty).
    pub fn pieces(&self, list: usize) -> [&[T]; 2] {
        let len = self.lens[list] as usize;
        // SAFETY: the first `len` slots of the list's room were written by `push_unchecked`.
        let carved = unsafe { std::slice::from_raw_parts(self.region.as_ptr().add(list * self.room), len) };
        [carved, &self.spill[list]]
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
}

#[cfg(test)]
mod tests {
    use super::Parts;

    #[test]
    fn lists_keep_order_and_spill() {
        for room in [1, 2, 5, 64] {
            let mut parts = Parts::new(7, room);
            let mut expected = vec![Vec::new(); 7];
            for n in 0..1000u64 {
                let list = ((n * 2_654_435_761) >> 7) as usize % 7;
                parts.push(list, n);
                expected[list].push(n);
            }
            for (list, want) in expected.iter().enumerate() {
                let got: Vec<u64> = parts.pieces(list).concat();
                assert_eq!(&got, want, "room {room}, list {list}");
                assert_eq!(parts.len_of(list), want.len());
            }
            assert_eq!(parts.len(), 1000);
            assert_eq!(parts.iter().count(), 1000);
            assert_eq!(parts.spilled() > 0, room < 1000 / 7);
        }
    }

    #[test]
    fn empty_lists() {
        let parts: Parts<u32> = Parts::new(3, 4);
        assert!(parts.is_empty());
        assert!((0..3).all(|list| parts.pieces(list).iter().all(|piece| piece.is_empty())));
    }
}
