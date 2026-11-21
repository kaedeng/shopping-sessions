//! Slot `sess-purchase-side`: how an event's `pay` rides through the sort. The keys are sorted
//! alone, 8 bytes wide; purchases (about 3% of rows) go to a side list sorted the same way and
//! are merged back a session at a time.
//!
//! Contract, kept by every alternative (`sess-wide` sorts `(key, pay)` records instead):
//! - `clear(rows)` forgets the last bucket and makes room for `rows` events;
//! - `extend(events)` adds events as `(key, pay)`, `pay` as in `PersonEvent`;
//! - `sort(key_bits)` orders the rows by key (slot `sort-*`) and puts a sentinel key
//!   `u64::MAX` after the last;
//! - `key(i)`, `i <= rows`: the `i`-th key in order, the sentinel at `rows`;
//! - `purchases(range, f)`: `range` is one session's rows in sorted order, every row of its
//!   last key included, ranges ascending and disjoint; calls `f(price, category)` for each
//!   purchase among them and returns their summed price.

use std::ops::Range;

use types::record::{PAY_CATEGORY_SHIFT, PAY_PRICE_MASK};

#[derive(Default)]
pub struct Rows {
    keys: Vec<u64>,
    /// `(key, pay)` of the purchases, then a sentinel key `u64::MAX`.
    side: Vec<(u64, u32)>,
    /// The first purchase not yet handed out.
    next: usize,
    scratch: Vec<u64>,
}

impl Rows {
    pub fn clear(&mut self, rows: usize) {
        self.keys.clear();
        self.keys.reserve(rows + 1);
        self.side.clear();
        self.next = 0;
    }

    /// Appends events as `(key, pay)`; called once per piece, so `Vec::extend` keeps its length
    /// in a register for a slice's trusted length.
    #[inline(always)]
    pub fn extend(&mut self, rows: impl Iterator<Item = (u64, u32)>) {
        let Rows { keys, side, .. } = self;
        keys.extend(rows.map(|(key, pay)| {
            if pay != 0 {
                side.push((key, pay));
            }
            key
        }));
    }

    pub fn sort(&mut self, key_bits: u32) {
        crate::sort_std::sort(&mut self.keys, key_bits, &mut self.scratch);
        self.keys.push(u64::MAX);
        // Equal keys are equal timestamps of one person, so one session: their order is free.
        self.side.sort_unstable_by_key(|&(key, _)| key);
        self.side.push((u64::MAX, 0));
    }

    #[inline(always)]
    pub fn key(&self, i: usize) -> u64 {
        debug_assert!(i < self.keys.len());
        // SAFETY: `i <= rows`, and `sort` left `rows + 1` keys.
        unsafe { *self.keys.get_unchecked(i) }
    }

    #[inline(always)]
    pub fn purchases(&mut self, range: Range<usize>, mut f: impl FnMut(u32, u8)) -> u64 {
        let last = self.key(range.end - 1);
        let mut sum = 0;
        loop {
            // SAFETY: the sentinel key is above every `last`, so the walk stops inside `side`.
            let (key, pay) = unsafe { *self.side.get_unchecked(self.next) };
            if key > last {
                return sum;
            }
            let price = pay & PAY_PRICE_MASK;
            f(price, (pay >> PAY_CATEGORY_SHIFT) as u8);
            sum += u64::from(price);
            self.next += 1;
        }
    }
}
