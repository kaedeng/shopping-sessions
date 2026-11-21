//! Slot `ts-table`: `ts` and `ingest_ts` (rules "Timestamps"). A date-string table settles
//! `YYYY-MM-DDT`; SWAR checks and converts the rest. The form takes a branch on byte 10, then
//! on the length: on the M4 that beat computing F1/F2 and F3 both and picking one by 10%.

use types::Ctx;
use types::time::{DAY, SECOND, civil_from_days};

pub(crate) const ZEROS: u64 = 0x3030_3030_3030_3030;
const HIGH: u64 = 0xF0F0_F0F0_F0F0_F0F0;
const OVER: u64 = 0x1010_1010_1010_1010;
/// Bytes 11..19, `HH:MM:SS` minus `'0'`: adding these carries into 0x10 exactly when a digit is
/// over its largest value (hour tens 2, minute and second tens 5, units 9).
const TIME_ADD: u64 = 0x060A_0006_0A00_060D;
const TIME_COLONS: u64 = 0x0000_0A00_000A_0000;
const TIME_COLON_BYTES: u64 = 0x0000_FF00_00FF_0000;
/// Bytes 19..27 minus `'0'`: the zone, then `HH:MM` of an offset in bytes 1..5.
const ZONE_BYTES: u64 = 0x0000_FFFF_FFFF_FF00;
const ZONE_ADD: u64 = 0x0000_060A_0006_0D00;
const ZONE_COLON: u64 = 0x0000_0000_0A00_0000;
const ZONE_COLON_BYTE: u64 = 0x0000_0000_FF00_0000;
/// Days the table reaches past the window, so a late `ingest_ts` (up to 5 days) still hits it.
const INGEST_DAYS: i64 = 6;

#[inline(always)]
pub(crate) unsafe fn rd(p: *const u8) -> u64 {
    u64::from_le(unsafe { p.cast::<u64>().read_unaligned() })
}

/// All 8 bytes are ASCII digits (Lemire).
#[inline(always)]
pub(crate) fn is8(w: u64) -> bool {
    ((w & HIGH) | ((w.wrapping_add(0x0606_0606_0606_0606) & HIGH) >> 4)) == 0x3333_3333_3333_3333
}

/// The value of 8 ASCII digits, the first byte most significant (Lemire); garbage otherwise.
#[inline(always)]
pub(crate) fn parse8(w: u64) -> u64 {
    let w = (w & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
    let w = (w & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
    (w & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32
}

/// One date: bytes 0..8 and 3..11 of `YYYY-MM-DDT`, and its midnight.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Slot {
    head: u64,
    tail: u64,
    /// Milliseconds after `Ctx::origin`.
    ms: i64,
    /// Epoch seconds.
    s: i64,
}

pub(crate) struct Dates {
    /// A perfect hash of `tail`: every date of the table has its own slot.
    slots: Box<[Slot]>,
    mul: u64,
    shift: u32,
    origin: i64,
    /// 13 when every F3 value in either window has 13 digits, so the SWAR parse applies.
    f3_len: usize,
}

impl Dates {
    /// Every date a valid `ts` can be written with: the click window's first day - 1 (an offset
    /// moves the local date by up to a day) to the window's last day + 1, then `INGEST_DAYS`.
    pub fn new(ctx: &Ctx) -> Dates {
        let first = ctx.origin.div_euclid(DAY) - 1;
        let last = (ctx.window_end - 1).div_euclid(DAY) + 1 + INGEST_DAYS;
        let keys: Vec<Slot> = (first..=last)
            .filter_map(|day| {
                let (year, month, date) = civil_from_days(day);
                let text = format!("{year:04}-{month:02}-{date:02}T");
                let word = |at: usize| u64::from_le_bytes(text.as_bytes()[at..at + 8].try_into().unwrap());
                (0..=9999).contains(&year).then(|| Slot {
                    head: word(0),
                    tail: word(3),
                    ms: day * DAY - ctx.origin,
                    s: (day * DAY).div_euclid(SECOND),
                })
            })
            .collect();
        let digits = |ms: i64| if ms > 0 { ms.ilog10() as usize + 1 } else { 0 };
        let f3_len = if digits(ctx.origin) == 13 && digits(ctx.window_end - 1) == 13 { 13 } else { usize::MAX };
        for bits in usize::BITS - (4 * keys.len()).leading_zeros()..=24 {
            for seed in 1..=64u64 {
                let (mul, shift) = (seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, 64 - bits);
                let mut slots = vec![None; 1 << bits];
                if keys.iter().all(|key| slots[(key.tail.wrapping_mul(mul) >> shift) as usize].replace(*key).is_none()) {
                    // A free slot holds a date that hashes elsewhere, so no text can match it there.
                    let slots = slots.into_iter().map(|slot| slot.unwrap_or(keys[0])).collect();
                    return Dates { slots, mul, shift, origin: ctx.origin, f3_len };
                }
            }
        }
        unreachable!("a multiplier separating {} dates", keys.len())
    }

    /// Bytes 0..19, `YYYY-MM-DDTHH:MM:SS`: (valid with a date in the table, ms after origin, epoch seconds).
    ///
    /// # Safety
    /// 19 bytes readable at `p`.
    #[inline(always)]
    unsafe fn local(&self, p: *const u8) -> (bool, i64, i64) {
        let (head, tail) = unsafe { (rd(p), rd(p.add(3))) };
        // SAFETY: `shift` leaves exactly the bits that index `slots`.
        let slot = unsafe { *self.slots.get_unchecked((tail.wrapping_mul(self.mul) >> self.shift) as usize) };
        let t = unsafe { rd(p.add(11)) } ^ ZEROS;
        let bad = (t & HIGH) | (t.wrapping_add(TIME_ADD) & OVER) | ((t ^ TIME_COLONS) & TIME_COLON_BYTES);
        // Byte i becomes 10 * digit i + digit i + 1: the hour in byte 0, minute 3, second 6.
        let t = t.wrapping_mul(10).wrapping_add(t >> 8);
        let (hour, minute, second) = (t & 0xFF, t >> 24 & 0xFF, t >> 48 & 0xFF);
        let ok = (slot.head == head) & (slot.tail == tail) & (bad == 0) & (hour <= 23);
        let seconds = (hour * 3600 + minute * 60 + second) as i64;
        (ok, slot.ms + seconds * SECOND, slot.s + seconds)
    }

    /// F1, as `ingest_ts` always is: (valid with a date in the table, ms after origin, epoch seconds).
    ///
    /// # Safety
    /// 20 bytes readable at `p`.
    #[inline(always)]
    pub unsafe fn f1(&self, p: *const u8, len: usize) -> (bool, i64, i64) {
        let (ok, ms, s) = unsafe { self.local(p) };
        (ok & (len == 20) & (unsafe { *p.add(19) } == b'Z'), ms, s)
    }

    /// The offset in ms of `+HH:MM` or `-HH:MM` at `p`, or `None`.
    ///
    /// # Safety
    /// 8 bytes readable at `p`.
    #[inline(always)]
    unsafe fn offset(p: *const u8) -> Option<i64> {
        let z = unsafe { rd(p) } ^ ZEROS;
        let sign = z as u8;
        let z = z & ZONE_BYTES;
        let bad = (z & HIGH) | (z.wrapping_add(ZONE_ADD) & OVER) | ((z ^ ZONE_COLON) & ZONE_COLON_BYTE);
        let z = z.wrapping_mul(10).wrapping_add(z >> 8);
        let (hour, minute) = (z >> 8 & 0xFF, z >> 32 & 0xFF);
        let offset = (hour * 3600 + minute * 60) as i64 * SECOND;
        match sign ^ b'0' {
            b'+' if bad == 0 && hour <= 23 => Some(offset),
            b'-' if bad == 0 && hour <= 23 => Some(-offset),
            _ => None,
        }
    }

    /// Any form: (well-formed, ms after origin); the window check is the caller's.
    ///
    /// # Safety
    /// 27 bytes readable at `p`, and `len` of them the field.
    #[inline(always)]
    pub unsafe fn ts(&self, p: *const u8, len: usize) -> (bool, i64) {
        if unsafe { *p.add(10) } == b'T' {
            let (ok, ms, _) = unsafe { self.local(p) };
            if ok & (len == 20) & (unsafe { *p.add(19) } == b'Z') {
                return (true, ms);
            }
            if ok & (len == 25)
                && let Some(offset) = unsafe { Self::offset(p.add(19)) }
            {
                return (true, ms - offset);
            }
        } else {
            // F3 without leading zeros: bytes 0..8 and 5..13, the first 5 digits then the last 8.
            let (w0, w1) = unsafe { (rd(p), rd(p.add(5))) };
            if (len == self.f3_len) & (w0 as u8 != b'0') & is8(w0) & is8(w1) {
                return (true, (parse8(w0 << 24 | 0x30_3030) * 100_000_000 + parse8(w1)) as i64 - self.origin);
            }
        }
        unsafe { self.slow(p, len) }
    }

    /// F3 in general (leading zeros, any length); everything else that reaches here is bad.
    #[cold]
    #[inline(never)]
    unsafe fn slow(&self, p: *const u8, len: usize) -> (bool, i64) {
        let text = unsafe { std::slice::from_raw_parts(p, len) };
        let digits = &text[text.iter().position(|&b| b != b'0').unwrap_or(len)..];
        // 19 digits or more is at least 10^18 ms, far outside any window.
        if text.is_empty() || !text.iter().all(u8::is_ascii_digit) || digits.len() > 18 {
            return (false, 0);
        }
        (true, digits.iter().fold(0i64, |v, &d| v * 10 + i64::from(d - b'0')) - self.origin)
    }
}
