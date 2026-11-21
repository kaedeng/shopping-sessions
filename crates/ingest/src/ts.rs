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

/// Eight timestamps at once (AVX-512), for the positional parse's batches (`pos`). Each lane
/// is a field's bytes as words the caller loaded (`rd`) and its date looked up (`date_s`); a
/// lane these do not accept is not bad, only left to `Dates::ts`.
#[cfg(all(
    feature = "pipe-positional",
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq"
))]
pub(crate) mod wide {
    use core::arch::x86_64::*;

    use super::{Dates, HIGH, OVER, TIME_ADD, TIME_COLON_BYTES, TIME_COLONS, ZEROS, ZONE_ADD, ZONE_BYTES, ZONE_COLON, ZONE_COLON_BYTE, rd};

    /// `Dates::date_s` of text that is not a date of the table.
    pub(crate) const NO_DATE: i64 = i64::MIN;

    #[inline(always)]
    fn all(value: u64) -> __m512i {
        // SAFETY: AVX-512F is compiled in (cfg above).
        #[allow(unused_unsafe)]
        unsafe {
            _mm512_set1_epi64(value as i64)
        }
    }

    /// `HH:MM:SS` or the `HH:MM` of an offset, as `local` and `offset` check and convert them:
    /// `t` is the 8 bytes less `'0'`. Returns the lanes with a digit out of place or range,
    /// and the bytes as `10 * digit + next digit`.
    #[inline(always)]
    fn pairs(t: __m512i, add: u64, colons: u64, colon_bytes: u64) -> (__m512i, __m512i) {
        // SAFETY: register arithmetic.
        #[allow(unused_unsafe)]
        unsafe {
            let bad = _mm512_or_si512(
                _mm512_or_si512(_mm512_and_si512(t, all(HIGH)), _mm512_and_si512(_mm512_add_epi64(t, all(add)), all(OVER))),
                _mm512_and_si512(_mm512_xor_si512(t, all(colons)), all(colon_bytes)),
            );
            (bad, _mm512_add_epi64(_mm512_add_epi64(_mm512_slli_epi64::<3>(t), _mm512_slli_epi64::<1>(t)), _mm512_srli_epi64::<8>(t)))
        }
    }

    impl Dates {
        /// The epoch second of the midnight of the date `YYYY-MM-DDT` at `p`, or `NO_DATE`:
        /// the table lookup of `local`, done row by row so the wide code needs no gather.
        ///
        /// # Safety
        /// 11 bytes readable at `p`.
        #[inline(always)]
        pub(crate) unsafe fn date_s(&self, p: *const u8) -> i64 {
            let (head, tail) = unsafe { (rd(p), rd(p.add(3))) };
            // SAFETY: `shift` leaves exactly the bits that index `slots`.
            let slot = unsafe { *self.slots.get_unchecked((tail.wrapping_mul(self.mul) >> self.shift) as usize) };
            if (slot.head == head) & (slot.tail == tail) { slot.s } else { NO_DATE }
        }

        /// A `YYYY-MM-DDTHH:MM:SS` whose bytes 8..16 are `g1`, bytes 12..20 `g2` and date
        /// `date_s`: the lanes `local` accepts, their epoch seconds, and byte 19.
        #[inline(always)]
        fn local_wide(&self, g1: __m512i, g2: __m512i, date_s: __m512i) -> (__mmask8, __m512i, __m512i) {
            // SAFETY: register arithmetic.
            #[allow(unused_unsafe)]
            unsafe {
                // The time is bytes 11..19: one byte of `g1`, seven of `g2`.
                let time = _mm512_or_si512(_mm512_and_si512(_mm512_srli_epi64::<24>(g1), all(0xFF)), _mm512_slli_epi64::<8>(g2));
                let (bad, t) = pairs(_mm512_xor_si512(time, all(ZEROS)), TIME_ADD, TIME_COLONS, TIME_COLON_BYTES);
                let byte = all(0xFF);
                let (hour, minute, second) =
                    (_mm512_and_si512(t, byte), _mm512_and_si512(_mm512_srli_epi64::<24>(t), byte), _mm512_and_si512(_mm512_srli_epi64::<48>(t), byte));
                let ok = _mm512_cmpneq_epi64_mask(date_s, all(NO_DATE as u64)) & _mm512_testn_epi64_mask(bad, bad) & _mm512_cmple_epu64_mask(hour, all(23));
                let seconds = _mm512_add_epi64(_mm512_add_epi64(_mm512_mul_epu32(hour, all(3600)), _mm512_mul_epu32(minute, all(60))), second);
                (ok, _mm512_add_epi64(date_s, seconds), _mm512_srli_epi64::<56>(g2))
            }
        }

        /// A slot's `ms` is its `s` in ms less the origin.
        #[inline(always)]
        fn ms_of(&self, s: __m512i) -> __m512i {
            // SAFETY: register arithmetic.
            #[allow(unused_unsafe)]
            unsafe {
                _mm512_sub_epi64(_mm512_mullo_epi64(s, all(1000)), all(self.origin as u64))
            }
        }

        /// `f1` for 8 fields of `len` bytes: the lanes it accepts, their ms after origin and
        /// their epoch seconds.
        #[inline(always)]
        pub(crate) fn f1_wide(&self, g1: __m512i, g2: __m512i, len: __m512i, date_s: __m512i) -> (__mmask8, __m512i, __m512i) {
            let (ok, s, zone) = self.local_wide(g1, g2, date_s);
            // SAFETY: register arithmetic.
            #[allow(unused_unsafe)]
            let ok = unsafe { ok & _mm512_cmpeq_epi64_mask(len, all(20)) & _mm512_cmpeq_epi64_mask(zone, all(u64::from(b'Z'))) };
            (ok, self.ms_of(s), s)
        }

        /// `ts` for 8 fields of `len` bytes, bytes 0..8 `g0`, 8..16 `g1`, 12..20 `g2` and
        /// 19..27 `g3`: F1, F2, and F3 as 13 digits without a leading zero. Returns the lanes
        /// that are one of these (each a value `ts` accepts) and their ms after origin.
        #[inline(always)]
        pub(crate) fn ts_wide(&self, g0: __m512i, g1: __m512i, g2: __m512i, g3: __m512i, len: __m512i, date_s: __m512i) -> (__mmask8, __m512i) {
            let (local_ok, s, zone) = self.local_wide(g1, g2, date_s);
            let local_ms = self.ms_of(s);
            // SAFETY: register arithmetic.
            #[allow(unused_unsafe)]
            unsafe {
                let f1 = local_ok & _mm512_cmpeq_epi64_mask(len, all(20)) & _mm512_cmpeq_epi64_mask(zone, all(u64::from(b'Z')));

                // The offset, as `offset` reads it: the sign, then `HH:MM` in bytes 1..6.
                let z = _mm512_xor_si512(g3, all(ZEROS));
                let sign = _mm512_and_si512(z, all(0xFF));
                let (bad, pair) = pairs(_mm512_and_si512(z, all(ZONE_BYTES)), ZONE_ADD, ZONE_COLON, ZONE_COLON_BYTE);
                let byte = all(0xFF);
                let (hour, minute) = (_mm512_and_si512(_mm512_srli_epi64::<8>(pair), byte), _mm512_and_si512(_mm512_srli_epi64::<32>(pair), byte));
                let offset = _mm512_mullo_epi64(_mm512_add_epi64(_mm512_mul_epu32(hour, all(3600)), _mm512_mul_epu32(minute, all(60))), all(1000));
                let plus = _mm512_cmpeq_epi64_mask(sign, all(u64::from(b'+' ^ b'0')));
                let minus = _mm512_cmpeq_epi64_mask(sign, all(u64::from(b'-' ^ b'0')));
                let f2 = local_ok & _mm512_cmpeq_epi64_mask(len, all(25)) & _mm512_testn_epi64_mask(bad, bad) & _mm512_cmple_epu64_mask(hour, all(23)) & (plus | minus);
                // A `+` offset is ahead of UTC: subtract it.
                let f2_ms = _mm512_mask_add_epi64(_mm512_sub_epi64(local_ms, offset), minus, local_ms, offset);

                // Bytes 0..8 and 5..13, as `ts` reads an F3.
                let (w0, w1) = (g0, _mm512_or_si512(_mm512_srli_epi64::<40>(g0), _mm512_slli_epi64::<24>(g1)));
                let (zero, nine) = (_mm512_set1_epi8(b'0' as i8), _mm512_set1_epi8(9));
                let (d0, d1) = (_mm512_sub_epi8(w0, zero), _mm512_sub_epi8(w1, zero));
                let digits = _mm512_cmple_epu8_mask(d0, nine) & _mm512_cmple_epu8_mask(d1, nine);
                let f3 = _mm512_cmpeq_epi64_mask(_mm512_movm_epi8(digits), all(u64::MAX))
                    & _mm512_cmpneq_epi64_mask(_mm512_and_si512(w0, all(0xFF)), all(u64::from(b'0')))
                    & _mm512_cmpeq_epi64_mask(len, all(self.f3_len as u64));
                // 8 digits, the first in the low byte: pairs, pairs of pairs, the two halves.
                let convert = |digits: __m512i| {
                    let pairs = _mm512_maddubs_epi16(digits, _mm512_set1_epi16(0x010A));
                    let quads = _mm512_madd_epi16(pairs, _mm512_set1_epi32(0x0001_0064));
                    _mm512_add_epi64(_mm512_mul_epu32(quads, all(10_000)), _mm512_srli_epi64::<32>(quads))
                };
                // The first five digits behind three zeros, then the last eight.
                let value = _mm512_add_epi64(_mm512_mul_epu32(convert(_mm512_slli_epi64::<24>(d0)), all(100_000_000)), convert(d1));
                let f3_ms = _mm512_sub_epi64(value, all(self.origin as u64));

                let ms = _mm512_mask_blend_epi64(f3, _mm512_mask_blend_epi64(f2, local_ms, f2_ms), f3_ms);
                (f1 | f2 | f3, ms)
            }
        }
    }
}
