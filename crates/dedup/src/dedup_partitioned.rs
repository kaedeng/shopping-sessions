//! Slot `dedup-partitioned`. Every alternative keeps:
//!
//! ```text
//! pub fn partition<'s, 'p>(
//!     pieces: impl Iterator<Item = &'p [EventRec]> + Clone,
//!     part_bits: u32,
//!     scratch: &'s mut Scratch,
//! ) -> &'s [EventRec]
//! ```
//!
//! `pieces` are the workers' vectors of one partition, so every copy of an `event_id` is in
//! them. Returns one record per distinct `id`: the copy with the smallest `ingest` (copies that
//! tie are identical). Tombstones take part like any record; the caller skips them.
//! `part_bits` says which hash bits already chose the partition.
//!
//! A table of `u32` slots, each 0 or 1 + an index into the kept list, linear probing from the
//! hash bits just below the partition's, each row's slot prefetched a few rows ahead.

use types::hash::hash;
use types::mem::prefetch;
use types::record::EventRec;

use crate::Scratch;

/// Slots per row, rounded up to a power of two, so the table ends a quarter full or less. Fewer
/// collisions mean fewer reads of the kept list and fewer mispredicts (implementation.md has the sweep).
const ROOM: usize = 4;
/// Rows ahead whose slot is prefetched.
const AHEAD: usize = 16;

pub fn partition<'s, 'p>(
    pieces: impl Iterator<Item = &'p [EventRec]> + Clone,
    part_bits: u32,
    scratch: &'s mut Scratch,
) -> &'s [EventRec] {
    let rows: usize = pieces.clone().map(<[EventRec]>::len).sum();
    let bits = (ROOM * rows).next_power_of_two().max(1024).trailing_zeros();
    assert!(part_bits + bits <= 64 && rows < u32::MAX as usize);
    let (shift, mask) = (64 - part_bits - bits, (1usize << bits) - 1);
    let slot_of = |id| (hash(id) >> shift) as usize & mask;
    let Scratch { table, kept } = scratch;
    table.clear();
    table.resize(mask + 1, 0);
    kept.clear();
    kept.reserve(rows);
    for piece in pieces {
        for (i, r) in piece.iter().enumerate() {
            if let Some(ahead) = piece.get(i + AHEAD) {
                prefetch(table.as_ptr().wrapping_add(slot_of(ahead.id)));
            }
            let mut at = slot_of(r.id);
            loop {
                // SAFETY: `at` is at most `mask`, and the table has `mask + 1` slots.
                let slot = unsafe { table.get_unchecked_mut(at) };
                if *slot == 0 {
                    kept.push(*r);
                    *slot = kept.len() as u32;
                    break;
                }
                // SAFETY: a slot is set to the kept length right after a push, never above it.
                let first = unsafe { kept.get_unchecked_mut(*slot as usize - 1) };
                if first.id == r.id {
                    if r.ingest < first.ingest {
                        *first = *r;
                    }
                    break;
                }
                at = (at + 1) & mask;
            }
        }
    }
    kept
}
