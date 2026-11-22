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

    /// The record's two words, as `words` gives them.
    #[inline(always)]
    pub fn from_words(low: u64, high: u64) -> PosEvent {
        PosEvent { low, high }
    }

    /// `(u | ts << 32, ingest | slot << 32 | flags << 48)`.
    #[inline(always)]
    pub fn words(&self) -> (u64, u64) {
        (self.low, self.high)
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

/// One valid click of a numbered user that has no slot in the click table: its `click_id` is
/// not a number of the table's (spelled another way, one line in 200,000, or past its end),
/// or its campaign is past a `PosClick`'s bits. The click as the classic lane takes it, with
/// the user's number: it joins its person's clicks (`src/positional.rs`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExtraClick {
    /// `person << 9 | alias`.
    pub u: u32,
    pub click: Id,
    pub ts: u32,
    pub campaign: u32,
}

/// Bits of a click's key (`click_key_at`); the campaign's index is above them.
pub const CLICK_KEY_BITS: u32 = 52;
/// Campaigns a `PosClick` can name.
pub const CLICK_CAMPAIGNS: usize = 1 << (64 - CLICK_KEY_BITS);

/// One valid click whose `user_id` decodes to a number inside the dense arrays and whose
/// `click_id` is a number of the click table: the table's slot at that number. 16 bytes, as
/// two words (see `PosEvent`). A slot no click was stored in is all zeros, which no click is:
/// a key counts at least one digit.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct PosClick {
    /// `u | ts << 32`.
    low: u64,
    /// `key | campaign << CLICK_KEY_BITS`: the `click_id`'s key, for the tie-break among
    /// clicks of one `ts`.
    rest: u64,
}

/// The key of a `click_id` spelled as the generator spells one: `k`, then 1 to 8 of `0-9a-z`
/// without a leading zero. The digits six bits each, the first highest, shorter IDs filled
/// with zeros below, then the number of digits in the low four bits: two keys compare as
/// their IDs' bytes do. `None` for any other spelling.
///
/// # Safety
/// `len` bytes readable at `p`.
#[inline]
pub unsafe fn click_key_at(p: *const u8, len: usize) -> Option<u64> {
    if !(2..=9).contains(&len) || unsafe { *p } != b'k' {
        return None;
    }
    let digits = len - 1;
    let mut sextets = 0u64;
    for at in 0..digits {
        let value = match unsafe { *p.add(1 + at) } {
            byte @ b'0'..=b'9' => byte - b'0',
            byte @ b'a'..=b'z' => byte - b'a' + 10,
            _ => return None,
        };
        sextets |= u64::from(value) << (6 * (7 - at));
    }
    if digits > 1 && sextets >> 42 == 0 {
        return None;
    }
    Some(sextets << 4 | digits as u64)
}

/// The `click_id` a key stands for, packed (`types::id`).
pub fn click_key_id(key: u64) -> Id {
    let (digits, sextets) = ((key & 15) as usize, key >> 4);
    let mut text = vec![b'k'];
    text.extend((0..digits).map(|at| b"0123456789abcdefghijklmnopqrstuvwxyz"[(sextets >> (6 * (7 - at)) & 63) as usize]));
    crate::id::pack(&text)
}

impl PosClick {
    /// `campaign` is below `CLICK_CAMPAIGNS`.
    #[inline(always)]
    pub fn new(key: u64, u: u32, ts: u32, campaign: u32) -> PosClick {
        PosClick { low: u64::from(u) | u64::from(ts) << 32, rest: key | u64::from(campaign) << CLICK_KEY_BITS }
    }

    /// Whether a click was stored here: a slot of the click table nobody wrote is zeros.
    #[inline(always)]
    pub fn is_some(&self) -> bool {
        self.rest & 15 != 0
    }

    /// `(u | ts << 32, key | campaign << CLICK_KEY_BITS)`.
    #[inline(always)]
    pub fn words(&self) -> (u64, u64) {
        (self.low, self.rest)
    }

    /// A click from its two words: `u | ts << 32`, `key | campaign << CLICK_KEY_BITS`.
    #[inline(always)]
    pub fn from_words(low: u64, rest: u64) -> PosClick {
        PosClick { low, rest }
    }

    /// The `click_id`'s key (`click_key_at`).
    #[inline(always)]
    pub fn key(&self) -> u64 {
        self.rest & ((1 << CLICK_KEY_BITS) - 1)
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
        (self.rest >> CLICK_KEY_BITS) as u32
    }
}

const _: () = assert!(size_of::<PosEvent>() == 16 && size_of::<PosPay>() == 12 && size_of::<PosClick>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_keys_order_as_the_ids_bytes() {
        let ids: [&[u8]; 14] = [b"k0", b"k1", b"k10", b"k100", b"k1z", b"k2", b"k9", b"ka", b"ka0", b"kz", b"kzzzzzzzz", b"k10000000", b"kb1ni90tn", b"kc1ni90tn"];
        let key = |id: &[u8]| unsafe { click_key_at(id.as_ptr(), id.len()) }.unwrap();
        for a in ids {
            assert_eq!(click_key_id(key(a)), crate::id::pack(a), "{:?}", std::str::from_utf8(a));
            assert!(key(a) < 1 << CLICK_KEY_BITS);
            for b in ids {
                assert_eq!(key(a).cmp(&key(b)), a.cmp(b), "{:?} and {:?}", std::str::from_utf8(a), std::str::from_utf8(b));
            }
        }
        for bad in [&b""[..], b"k", b"K1", b"k01", b"k00", b"kA", b"k1 ", b"u1", b"k123456789", b"k1\r", b"k\xffz", b"k1_"] {
            assert_eq!(unsafe { click_key_at(bad.as_ptr(), bad.len()) }, None, "{:?}", std::str::from_utf8(bad));
        }
    }
}
