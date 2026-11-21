//! Slot `rec-32`: the fixed-width records that move between the stages.
//!
//! Every `ts` is a `u32` of milliseconds after `Ctx::origin` (window start - 7 days), for
//! events and clicks alike, so the two compare directly.

use crate::id::Id;

/// `EventRec::flags`: every check of rules "Events" steps 3 and 4 passed (user, ts in the
/// window, type, product, price for a purchase, not late). Unset, the record is a tombstone:
/// it still takes part in dedup, and only `id` and `ingest` mean anything.
pub const VALID: u32 = 1;

/// `pay` = `price_cents` (low 24 bits, 1..=10,000,000) | category index << 24.
pub const PAY_CATEGORY_SHIFT: u32 = 24;
pub const PAY_PRICE_MASK: u32 = (1 << PAY_CATEGORY_SHIFT) - 1;
pub const MAX_PRICE: u32 = 10_000_000;

/// One parsed event line, as scattered into the dedup partitions. 32 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct EventRec {
    /// `event_id`, never empty.
    pub id: Id,
    /// `user_id` as read; the person is looked up after dedup.
    pub user: Id,
    pub ts: u32,
    /// `ingest_ts` in seconds, as `Ctx::ingest_rel` clamps it. Dedup keeps the smallest.
    pub ingest: u32,
    /// 0 unless a valid purchase.
    pub pay: u32,
    pub flags: u32,
}

/// One valid click (every click that reaches a record passed validation). 24 bytes.
/// `user` holds the `user_id` after the parse and the person after the link lookup.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct ClickRec {
    pub user: Id,
    pub click: Id,
    pub ts: u32,
    /// Index into `Ctx::campaigns`.
    pub campaign: u32,
}

/// One surviving event in its person bucket. 16 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PersonEvent {
    pub person: Id,
    pub ts: u32,
    /// 0 unless a purchase.
    pub pay: u32,
}

const _: () = assert!(size_of::<EventRec>() == 32 && size_of::<ClickRec>() == 24 && size_of::<PersonEvent>() == 16);
