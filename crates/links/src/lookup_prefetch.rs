//! Slot `lookup-prefetch`. Every alternative keeps:
//!
//! ```text
//! pub fn scatter_events(table: &LinkTable, kept: &[EventRec], bucket_bits: u32, out: &mut [Vec<PersonEvent>])
//! pub fn scatter_clicks(table: &LinkTable, clicks: &[ClickRec], bucket_bits: u32, out: &mut [Vec<ClickRec>])
//! ```
//!
//! Each gives every record its person and appends it to
//! `out[(types::hash::person_hash(person) >> (64 - bucket_bits)) as usize]`; `scatter_events`
//! skips tombstones (records without `types::record::VALID`), `scatter_clicks` replaces `user`
//! with the person. One loop per call against the read-only table, prefetching 64 rows ahead.

use types::Id;
use types::hash::person_hash;
use types::record::{ClickRec, EventRec, PersonEvent, VALID};

use crate::LinkTable;

/// `prototypes/lookupbench`: 16 ahead and no prefetch were both slower. On the line table 32, 64
/// and 128 measured the same.
const AHEAD: usize = 64;

pub fn scatter_events(table: &LinkTable, kept: &[EventRec], bucket_bits: u32, out: &mut [Vec<PersonEvent>]) {
    scatter(table, kept, bucket_bits, out, |r| (r.flags & VALID != 0).then_some(r.user), |r, person| {
        PersonEvent { person, ts: r.ts, pay: r.pay }
    });
}

pub fn scatter_clicks(table: &LinkTable, clicks: &[ClickRec], bucket_bits: u32, out: &mut [Vec<ClickRec>]) {
    scatter(table, clicks, bucket_bits, out, |r| Some(r.user), |r, person| ClickRec { user: person, ..*r });
}

/// `user` is `None` for a row to skip.
#[inline(always)]
fn scatter<R, O>(
    table: &LinkTable,
    rows: &[R],
    bucket_bits: u32,
    out: &mut [Vec<O>],
    user: impl Fn(&R) -> Option<Id>,
    make: impl Fn(&R, Id) -> O,
) {
    assert!((1..=32).contains(&bucket_bits) && out.len() == 1 << bucket_bits);
    let shift = 64 - bucket_bits;
    // Each call is one partition, so start the first rows' loads before the loop needs them.
    for id in rows.iter().take(AHEAD).filter_map(&user) {
        table.prefetch(id);
    }
    for (i, r) in rows.iter().enumerate() {
        if let Some(id) = rows.get(i + AHEAD).and_then(&user) {
            table.prefetch(id);
        }
        if let Some(id) = user(r) {
            let person = table.person(id);
            // SAFETY: the shift leaves `bucket_bits` bits, and `out` has `1 << bucket_bits` buckets.
            unsafe { out.get_unchecked_mut((person_hash(person) >> shift) as usize) }.push(make(r, person));
        }
    }
}
