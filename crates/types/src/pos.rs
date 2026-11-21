//! Slot `pipe-positional`: the records the positional parse writes (`ingest::pos`).

use crate::id::Id;

/// `PosEvent::flags`: the row passed every check of rules "Events" steps 3 and 4.
pub const POS_VALID: u16 = 1;
/// `PosEvent::flags`: `u` is not a user number but an index into the worker's `odd_users`:
/// a user ID the generator's spelling does not cover, or a number past the dense arrays.
pub const POS_ODD_USER: u16 = 2;

/// One event line whose `event_id` decodes to a number inside the dense arrays, in the list of
/// its number's partition. 16 bytes, kept as two words: the parse builds a record in two
/// registers and stores them, where five narrow fields were written to the stack and read
/// back as one 16-byte load that could not be forwarded from them, a stall on every row
/// (7.6% of a whole run on the x86 box).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PosEvent {
    /// `u | ts << 32`.
    low: u64,
    /// `ingest | slot << 32 | flags << 48`.
    high: u64,
}

impl PosEvent {
    /// No row: the largest `ingest`, so any row replaces it, and not valid.
    pub const EMPTY: PosEvent = PosEvent { low: 0, high: u32::MAX as u64 };

    #[inline(always)]
    pub fn new(u: u32, ts: u32, ingest: u32, slot: u16, flags: u16) -> PosEvent {
        PosEvent { low: u64::from(u) | u64::from(ts) << 32, high: u64::from(ingest) | u64::from(slot) << 32 | u64::from(flags) << 48 }
    }

    /// `person << 9 | alias`, or an index (`POS_ODD_USER`); anything when the row is invalid
    /// for an empty `user_id`.
    #[inline(always)]
    pub fn u(&self) -> u32 {
        self.low as u32
    }

    /// Milliseconds after `Ctx::origin`; meaningful only with `POS_VALID`.
    #[inline(always)]
    pub fn ts(&self) -> u32 {
        (self.low >> 32) as u32
    }

    /// `Ctx::ingest_rel`: dedup keeps the smallest.
    #[inline(always)]
    pub fn ingest(&self) -> u32 {
        self.high as u32
    }

    /// The event number's bits below the partition's.
    #[inline(always)]
    pub fn slot(&self) -> u16 {
        (self.high >> 32) as u16
    }

    #[inline(always)]
    pub fn flags(&self) -> u16 {
        (self.high >> 48) as u16
    }
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
/// its person's range. 24 bytes, as three words (see `PosEvent`).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PosClick {
    /// `click_id`, for the tie-break among clicks of one `ts`.
    pub click: Id,
    /// `u | ts << 32`.
    low: u64,
    campaign: u64,
}

impl PosClick {
    #[inline(always)]
    pub fn new(click: Id, u: u32, ts: u32, campaign: u32) -> PosClick {
        PosClick { click, low: u64::from(u) | u64::from(ts) << 32, campaign: u64::from(campaign) }
    }

    #[inline(always)]
    pub fn u(&self) -> u32 {
        self.low as u32
    }

    #[inline(always)]
    pub fn ts(&self) -> u32 {
        (self.low >> 32) as u32
    }

    #[inline(always)]
    pub fn campaign(&self) -> u32 {
        self.campaign as u32
    }
}

const _: () = assert!(size_of::<PosEvent>() == 16 && size_of::<PosPay>() == 12 && size_of::<PosClick>() == 24);
