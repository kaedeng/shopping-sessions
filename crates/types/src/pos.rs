//! Slot `pipe-positional`: the records the positional parse writes (`ingest::pos`).

use crate::id::Id;

/// `PosEvent::flags`: the row passed every check of rules "Events" steps 3 and 4.
pub const POS_VALID: u16 = 1;
/// `PosEvent::flags`: `u` is not a user number but an index into the worker's `odd_users`:
/// a user ID the generator's spelling does not cover, or a number past the dense arrays.
pub const POS_ODD_USER: u16 = 2;

/// One event line whose `event_id` decodes to a number inside the dense arrays, in the list of
/// its number's partition. 16 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PosEvent {
    /// `person << 9 | alias`, or an index (`POS_ODD_USER`); anything when the row is invalid
    /// for an empty `user_id`.
    pub u: u32,
    /// Milliseconds after `Ctx::origin`; meaningful only with `POS_VALID`.
    pub ts: u32,
    /// `Ctx::ingest_rel`: dedup keeps the smallest.
    pub ingest: u32,
    /// The event number's bits below the partition's.
    pub slot: u16,
    pub flags: u16,
}

/// A valid purchase's `pay`, beside its `PosEvent`. 12 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PosPay {
    pub slot: u32,
    /// The row's `ingest`: copies with equal `ingest_ts` are identical (rules), so this names
    /// the kept copy.
    pub ingest: u32,
    pub pay: u32,
}

/// One valid click whose `user_id` decodes to a number inside the dense arrays, in the list of
/// its person's range. 24 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PosClick {
    /// `click_id`, for the tie-break among clicks of one `ts`.
    pub click: Id,
    pub u: u32,
    pub ts: u32,
    pub campaign: u32,
    pub spare: u32,
}

const _: () = assert!(size_of::<PosEvent>() == 16 && size_of::<PosPay>() == 12 && size_of::<PosClick>() == 24);
