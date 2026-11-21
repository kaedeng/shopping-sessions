//! Linked IDs: the union-find that names each person, and the per-row person lookup that
//! scatters survivors into the person buckets (decisions: Links, Hash and tables, Clicks).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's):
//! - `LinkTable::person(&self, id: Id) -> Id`, here
//! - `build(pairs: &[[Id; 2]]) -> LinkTable`, slot `links-serial`
//! - `scatter_events(table: &LinkTable, kept: &[EventRec], bucket_bits: u32, out: &mut [Vec<PersonEvent>])`
//!   and `scatter_clicks(table: &LinkTable, clicks: &[ClickRec], bucket_bits: u32, out: &mut [Vec<ClickRec>])`,
//!   slot `lookup-prefetch`

use std::hint::select_unpredictable;

use types::Id;
use types::hash::hash;
use types::mem::prefetch;

#[cfg(feature = "links-serial")]
mod links_serial;
#[cfg(feature = "links-serial")]
pub use links_serial::build;

#[cfg(feature = "lookup-prefetch")]
mod lookup_prefetch;
#[cfg(feature = "lookup-prefetch")]
pub use lookup_prefetch::{scatter_clicks, scatter_events};

#[cfg(test)]
mod tests;

/// One cache line of `[id, person]` slots; key 0 (the empty ID) is a free slot.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line([[Id; 2]; 4]);

/// user ID -> person name, read-only once built. Holds linked IDs only. Linear probing whose
/// home is the first slot of a line picked by the top bits of `hash(id)`, so a line's used
/// slots are a prefix of it, and a line with a free slot and no match ends the search.
pub struct LinkTable {
    lines: Box<[Line]>,
    shift: u32,
}

impl LinkTable {
    /// Room for `keys` keys, at most 3/4 full (generated data fills it about 0.4).
    fn new(keys: usize) -> LinkTable {
        let lines = keys.div_ceil(3).next_power_of_two().max(4);
        // SAFETY: zero is a valid `Line`. Aligned above calloc's 16 bytes, so std zeroes it with a
        // memset: 6 ms at full size on the M4, on the links worker beside the parse.
        let lines = unsafe { Box::<[Line]>::new_zeroed_slice(lines).assume_init() };
        LinkTable { shift: 64 - lines.len().trailing_zeros(), lines }
    }

    #[inline(always)]
    fn home(&self, id: Id) -> usize {
        (hash(id) >> self.shift) as usize
    }

    /// The person an ID belongs to: its root, or the ID itself when no link names it.
    #[inline(always)]
    pub fn person(&self, id: Id) -> Id {
        let mask = self.lines.len() - 1;
        let mut at = self.home(id);
        loop {
            // SAFETY: `at` is at most `mask`, and the length is a power of two.
            let line = unsafe { self.lines.get_unchecked(at) };
            // All four slots and one branch, since 44% of generated rows miss at random. `done`
            // is one integer: x86 codegen splits an `|` of compares into a branch each.
            // The used slots are a prefix, so a free last slot means the line has a free slot.
            let (mut person, mut done) = (id, line.0[3][0]);
            for [key, value] in line.0 {
                person = select_unpredictable(key == id, value, person);
                done = done.min(key ^ id);
            }
            if done == 0 {
                return person;
            }
            at = (at + 1) & mask;
        }
    }

    /// Asks for the line `person(id)` reads first.
    #[inline(always)]
    pub fn prefetch(&self, id: Id) {
        prefetch(self.lines.as_ptr().wrapping_add(self.home(id)));
    }
}
