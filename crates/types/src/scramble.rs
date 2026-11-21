//! Slot `pipe-positional`: the generator's ID numbering, read back.
//!
//! The generator writes every event, user and click ID as a letter and the base 36 of
//! `scramble(number, key)`, a bijection on 40 bits with fixed multipliers and one key per ID
//! kind and dataset (`generator/src/text.rs`). With the key, an ID is its number again:
//! events are numbered `0..` in (person, time) order, and a user ID is `person << 9 | alias`.
//!
//! Nothing here can make a result wrong. `canonical_at` accepts exactly the spellings `base36`
//! writes, so two different IDs never share a number, whatever the key is; a wrong key only
//! makes the numbers large and scattered, and the pipeline sends what does not fit its dense
//! arrays down the general path.

use crate::id::{self, Id};

pub const MASK: u64 = (1 << 40) - 1;
const A: u64 = 0x9E37_79B9_7F4B;
const B: u64 = 0xD6E8_FEB8_6659;
const INV_A: u64 = inverse(A);
const INV_B: u64 = inverse(B);
/// A user number is `person << ALIAS_BITS | alias` (`ALIAS_SLOTS` in `generator/src/user.rs`).
pub const ALIAS_BITS: u32 = 9;
pub const ALIAS_MASK: u32 = (1 << ALIAS_BITS) - 1;

/// The inverse of an odd number modulo 2^40 (Newton: each step doubles the correct bits).
const fn inverse(x: u64) -> u64 {
    let mut inv = x;
    let mut step = 0;
    while step < 6 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(x.wrapping_mul(inv)));
        step += 1;
    }
    inv & MASK
}

const _: () = assert!(A.wrapping_mul(INV_A) & MASK == 1 && B.wrapping_mul(INV_B) & MASK == 1);

/// The scramble undone up to the key: `(number * A) ^ key`.
#[inline(always)]
pub fn half(scrambled: u64) -> u64 {
    let m = scrambled.wrapping_mul(INV_B) & MASK;
    m ^ (m >> 17) ^ (m >> 34)
}

/// The key under which `half` is the half of `number`.
#[inline(always)]
pub fn key_for(half: u64, number: u64) -> u64 {
    half ^ (number.wrapping_mul(A) & MASK)
}

/// The number whose half is `half` under `key`.
#[inline(always)]
pub fn number_of_half(half: u64, key: u64) -> u64 {
    (half ^ key).wrapping_mul(INV_A) & MASK
}

/// The number a scrambled value stands for under `key`.
#[inline(always)]
pub fn number(scrambled: u64, key: u64) -> u64 {
    number_of_half(half(scrambled), key)
}

/// The generator's `scramble`.
#[inline(always)]
pub fn scrambled(number: u64, key: u64) -> u64 {
    let mut m = (number.wrapping_mul(A) ^ key) & MASK;
    m ^= m >> 17;
    m.wrapping_mul(B) & MASK
}

/// ASCII to base 36 digit; 0xFF for anything but `0-9a-z`.
static DIGIT: [u8; 256] = {
    let mut table = [0xFF; 256];
    let mut c = 0;
    while c < 10 {
        table[b'0' as usize + c] = c as u8;
        c += 1;
    }
    let mut c = 0;
    while c < 26 {
        table[b'a' as usize + c] = 10 + c as u8;
        c += 1;
    }
    table
};

/// The scrambled value of an ID spelled exactly as the generator's `base36` spells one:
/// `letter`, then 1 to 8 lowercase base 36 digits without a leading zero, at most 2^40 - 1.
/// `None` for every other spelling, so distinct IDs never share a value.
///
/// # Safety
/// 9 bytes readable at `p`, whatever `len` is.
#[inline(always)]
pub unsafe fn canonical_at(p: *const u8, len: usize, letter: u8) -> Option<u64> {
    if len.wrapping_sub(2) > 7 || unsafe { *p } != letter {
        return None;
    }
    let digits = len - 1;
    let word = u64::from_le(unsafe { (p.add(1) as *const u64).read_unaligned() });
    // Right-align the digits in the word and fill the front with '0'.
    let shift = 8 * (8 - digits) as u32;
    let word = word << shift | (0x3030_3030_3030_3030 & ((1u64 << shift) - 1));
    let (mut value, mut bad) = (0u64, 0u8);
    for c in word.to_le_bytes() {
        let digit = DIGIT[c as usize];
        bad |= digit;
        value = value * 36 + u64::from(digit & 63);
    }
    let leading_zero = (digits > 1) & (unsafe { *p.add(1) } == b'0');
    ((bad < 64) & !leading_zero & (value <= MASK)).then_some(value)
}

/// `canonical_at` for an ID already packed (`types::id`).
pub fn canonical_packed(id: Id, letter: u8) -> Option<u64> {
    if id == 0 || id & id::SLOW != 0 {
        return None;
    }
    let (word, ninth, len) = id::spread(id);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&word.to_le_bytes());
    bytes[8] = ninth;
    // SAFETY: 16 bytes are readable.
    unsafe { canonical_at(bytes.as_ptr(), len, letter) }
}

/// The packed ID of `letter` and the base 36 of `scrambled(number, key)`: the spelling
/// `canonical_at` maps back to `number`.
pub fn id_of(number: u64, key: u64, letter: u8) -> Id {
    let mut rest = scrambled(number, key);
    let mut bytes = [0u8; 9];
    let mut at = bytes.len();
    loop {
        at -= 1;
        bytes[at] = b"0123456789abcdefghijklmnopqrstuvwxyz"[(rest % 36) as usize];
        rest /= 36;
        if rest == 0 {
            break;
        }
    }
    at -= 1;
    bytes[at] = letter;
    id::pack(&bytes[at..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scramble_round_trips_and_ids_come_back() {
        let key = 0x12_3456_789A;
        for n in [0u64, 1, 511, 512, 19_999_999, 1 << 39, MASK] {
            assert_eq!(number(scrambled(n, key), key), n);
            assert_eq!(key_for(half(scrambled(n, key)), n), key);
            let id = id_of(n, key, b'u');
            assert_eq!(canonical_packed(id, b'u').map(|s| number(s, key)), Some(n), "{n}");
            assert_eq!(canonical_packed(id, b'e'), None);
        }
    }

    #[test]
    fn only_the_generators_spelling_is_canonical() {
        let value = |text: &[u8]| {
            let mut bytes = [0u8; 32];
            bytes[..text.len()].copy_from_slice(text);
            // SAFETY: 32 bytes are readable.
            unsafe { canonical_at(bytes.as_ptr(), text.len(), b'u') }
        };
        assert_eq!(value(b"u0"), Some(0));
        assert_eq!(value(b"uz"), Some(35));
        assert_eq!(value(b"u10"), Some(36));
        assert_eq!(value(b"u4k2nn2cj"), Some(((((((4 * 36 + 20) * 36 + 2) * 36 + 23) * 36 + 23) * 36 + 2) * 36 + 12) * 36 + 19));
        for bad in [&b""[..], b"u", b"U1", b"u01", b"u00", b"uA", b"u1 ", b" u1", b"e1", b"u123456789", b"uzzzzzzzz", b"u1\r"] {
            assert_eq!(value(bad), None, "{:?}", std::str::from_utf8(bad));
        }
        // 36^8 - 1 is over 2^40, so `uzzzzzzzz` is not canonical, and the boundary is exact.
        let spell = |mut rest: u64| {
            let mut digits = Vec::new();
            while rest > 0 {
                digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(rest % 36) as usize]);
                rest /= 36;
            }
            digits.push(b'u');
            digits.reverse();
            digits
        };
        assert_eq!(value(&spell(MASK)), Some(MASK));
        assert_eq!(value(&spell(MASK + 1)), None);
    }
}
