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
/// The multipliers' inverses modulo 2^40, for kernels that undo the scramble themselves.
pub const INV_A: u64 = inverse(A);
pub const INV_B: u64 = inverse(B);
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
#[cfg_attr(all(target_arch = "x86_64", target_feature = "ssse3"), allow(dead_code))]
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
    let (value, digits_ok) = base36(word);
    let leading_zero = (digits > 1) & (unsafe { *p.add(1) } == b'0');
    (digits_ok & !leading_zero & (value <= MASK)).then_some(value)
}

/// The value of 8 base 36 digits, the first in the low byte, and whether all 8 are digits.
/// Three multiply-adds side by side: the digit-by-digit loop below is a chain of 8 table loads
/// and multiplies, about 70 cycles a row before the event number is known, and with
/// speculative store bypass disabled (`x86.md`) the next row's loads wait for the store whose
/// address comes out of it.
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
#[inline(always)]
fn base36(word: u64) -> (u64, bool) {
    use core::arch::x86_64::*;
    // SAFETY: SSSE3 is compiled in (cfg above).
    #[allow(unused_unsafe)]
    unsafe {
        let chars = _mm_cvtsi64_si128(word as i64);
        let (digit, letter) = (_mm_sub_epi8(chars, _mm_set1_epi8(b'0' as i8)), _mm_sub_epi8(chars, _mm_set1_epi8(b'a' as i8)));
        // Unsigned `x <= n` as `min(x, n) == x`.
        let is_digit = _mm_cmpeq_epi8(_mm_min_epu8(digit, _mm_set1_epi8(9)), digit);
        let is_letter = _mm_cmpeq_epi8(_mm_min_epu8(letter, _mm_set1_epi8(25)), letter);
        let value = _mm_or_si128(_mm_and_si128(is_digit, digit), _mm_and_si128(is_letter, _mm_add_epi8(letter, _mm_set1_epi8(10))));
        let ok = _mm_movemask_epi8(_mm_or_si128(is_digit, is_letter)) & 0xFF == 0xFF;
        // Pairs of digits (first * 36 + second), then pairs of pairs (first * 36^2 + second).
        let pairs = _mm_maddubs_epi16(value, _mm_set1_epi16(0x0124));
        let quads = _mm_madd_epi16(pairs, _mm_set1_epi32(0x0001_0510));
        let both = _mm_cvtsi128_si64(quads) as u64;
        ((both & 0xFFFF_FFFF) * 1_679_616 + (both >> 32), ok)
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "ssse3")))]
#[inline(always)]
fn base36(word: u64) -> (u64, bool) {
    let (mut value, mut bad) = (0u64, 0u8);
    for c in word.to_le_bytes() {
        let digit = DIGIT[c as usize];
        bad |= digit;
        value = value * 36 + u64::from(digit & 63);
    }
    (value, bad < 64)
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
    // The digits, the last first: at most 8, since the value is below 2^40.
    let (mut digits, mut count) = ([0u8; 8], 0);
    loop {
        digits[count] = b"0123456789abcdefghijklmnopqrstuvwxyz"[(rest % 36) as usize];
        rest /= 36;
        count += 1;
        if rest == 0 {
            break;
        }
    }
    // Packed as `id::pack` packs the text: 7 bits a byte, the letter in bits 56..63, each
    // digit 7 bits below the one before it, so the 8th lands in the low bits where a ninth
    // byte goes. (Through the text and `pack` this was 350 instructions, and every person
    // without links is named by it.)
    let (mut packed, mut shift) = (u64::from(letter) << 56, 49);
    for &digit in digits[..count].iter().rev() {
        packed |= u64::from(digit) << shift;
        shift -= 7;
    }
    packed
}

/// Eight IDs at once (AVX-512), for the positional parse's batches (`ingest::pos`).
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq"
))]
pub mod wide {
    use core::arch::x86_64::*;

    use super::{INV_A, INV_B, MASK};

    /// `canonical_at` and `number` for 8 IDs. Lane `i` is an ID of `len[i]` bytes whose bytes
    /// 0..8 are `first[i]` and bytes 1..9 `digits[i]` (as little-endian words; bytes past the
    /// ID are anything). Returns the numbers and the lanes whose ID is spelled the generator's
    /// way; the number of any other lane means nothing. Agrees with the scalar pair on every
    /// input (`tests::wide_agrees_with_scalar`). The words are loaded by the caller, row by
    /// row: a gather would do it in one instruction, but gathers are slow on AMD cores and on
    /// Intel ones under the Downfall mitigation.
    #[inline(always)]
    pub fn numbers(first: __m512i, digits: __m512i, len: __m512i, letter: u8, key: u64) -> (__m512i, __mmask8) {
        // SAFETY: register arithmetic; AVX-512 F, BW and DQ are compiled in (cfg above).
        #[allow(unused_unsafe)]
        unsafe {
            let low_byte = _mm512_set1_epi64(0xFF);
            let ok_len = _mm512_cmple_epu64_mask(_mm512_sub_epi64(len, _mm512_set1_epi64(2)), _mm512_set1_epi64(7));
            let ok_letter = _mm512_cmpeq_epi64_mask(_mm512_and_si512(first, low_byte), _mm512_set1_epi64(i64::from(letter)));
            // Right-align the digits and fill the front with '0': a shift of 72 - 8 * len bits.
            // A count of 64 or more gives 0, which is what `len == 9` needs of the fill.
            let shift = _mm512_sub_epi64(_mm512_set1_epi64(72), _mm512_slli_epi64::<3>(len));
            let fill = _mm512_srlv_epi64(_mm512_set1_epi64(0x3030_3030_3030_3030), _mm512_sub_epi64(_mm512_set1_epi64(64), shift));
            let chars = _mm512_or_si512(_mm512_sllv_epi64(digits, shift), fill);
            let (digit, alpha) = (_mm512_sub_epi8(chars, _mm512_set1_epi8(b'0' as i8)), _mm512_sub_epi8(chars, _mm512_set1_epi8(b'a' as i8)));
            let is_digit = _mm512_cmple_epu8_mask(digit, _mm512_set1_epi8(9));
            let is_alpha = _mm512_cmple_epu8_mask(alpha, _mm512_set1_epi8(25));
            let value = _mm512_or_si512(_mm512_maskz_mov_epi8(is_digit, digit), _mm512_maskz_add_epi8(is_alpha, alpha, _mm512_set1_epi8(10)));
            let ok_digits = _mm512_cmpeq_epi64_mask(_mm512_movm_epi8(is_digit | is_alpha), _mm512_set1_epi64(-1));
            // Pairs of digits, pairs of pairs, then the two halves: the first is the low dword.
            let pairs = _mm512_maddubs_epi16(value, _mm512_set1_epi16(0x0124));
            let quads = _mm512_madd_epi16(pairs, _mm512_set1_epi32(0x0001_0510));
            let scrambled = _mm512_add_epi64(_mm512_mul_epu32(quads, _mm512_set1_epi64(1_679_616)), _mm512_srli_epi64::<32>(quads));
            let mask = _mm512_set1_epi64(MASK as i64);
            let in_range = _mm512_cmple_epu64_mask(scrambled, mask);
            let zero_first = _mm512_cmpeq_epi64_mask(_mm512_and_si512(_mm512_srli_epi64::<8>(first), low_byte), _mm512_set1_epi64(i64::from(b'0')));
            let long = _mm512_cmpgt_epu64_mask(len, _mm512_set1_epi64(2));
            let ok = ok_len & ok_letter & ok_digits & in_range & !(zero_first & long);

            let m = _mm512_and_si512(_mm512_mullo_epi64(scrambled, _mm512_set1_epi64(INV_B as i64)), mask);
            let m = _mm512_xor_si512(m, _mm512_xor_si512(_mm512_srli_epi64::<17>(m), _mm512_srli_epi64::<34>(m)));
            let number = _mm512_and_si512(_mm512_mullo_epi64(_mm512_xor_si512(m, _mm512_set1_epi64(key as i64)), _mm512_set1_epi64(INV_A as i64)), mask);
            (number, ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq"))]
    #[test]
    fn wide_agrees_with_scalar() {
        use core::arch::x86_64::*;
        let key = 0x9A_BCDE_F012;
        // Generated spellings of every length, then every way of spoiling one.
        let mut texts: Vec<Vec<u8>> = Vec::new();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..4000u64 {
            let number = match round % 5 {
                0 => next() & MASK,
                1 => next() % 40_000_000,
                2 => next() % 36u64.pow((round % 8) as u32 + 1),
                3 => MASK - next() % 1000,
                _ => next() % 2000,
            };
            let mut text = Vec::new();
            crate::id::unpack(id_of(number, key, b'u'), &mut text);
            texts.push(text.clone());
            let at = (next() % text.len() as u64) as usize;
            let spoiled: [Vec<u8>; 8] = [
                text[..at].to_vec(),
                [&text[..at], &[b"A:`{/@ \0\x80\xff0Z"[(next() % 12) as usize]][..], &text[at + 1..]].concat(),
                [&text[..], &b"7"[..]].concat(),
                [&b"u0"[..], &text[1..]].concat(),
                [&b"e"[..], &text[1..]].concat(),
                [&text[..], &b"zzzzzzzz"[..]].concat(),
                text.to_ascii_uppercase(),
                b"u".to_vec(),
            ];
            texts.extend(spoiled);
        }
        texts.extend([b"".to_vec(), b"u0".to_vec(), b"u00".to_vec(), b"uzzzzzzzz".to_vec(), b"ue10tbp8f".to_vec()]);
        // What follows an ID in a line: a comma or a quote, then anything.
        let padded: Vec<Vec<u8>> = texts.iter().enumerate().map(|(i, text)| [&text[..], if i % 2 == 0 { &b",abc\"def,12345678"[..] } else { &b"\",zzzzzzzzzzzzzzz"[..] }].concat()).collect();
        let word = |bytes: &[u8]| u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let mut canonical = 0;
        for (buffers, lens) in padded.chunks(8).zip(texts.chunks(8)) {
            let (mut first, mut digits, mut len) = ([0u64; 8], [0u64; 8], [0u64; 8]);
            for (lane, (buffer, text)) in buffers.iter().zip(lens).enumerate() {
                (first[lane], digits[lane], len[lane]) = (word(buffer), word(&buffer[1..]), text.len() as u64);
            }
            // SAFETY: each array holds 8 words.
            let load = |words: &[u64; 8]| unsafe { _mm512_loadu_si512(words.as_ptr().cast()) };
            let (numbers, ok) = wide::numbers(load(&first), load(&digits), load(&len), b'u', key);
            let mut got = [0u64; 8];
            unsafe { _mm512_storeu_si512(got.as_mut_ptr().cast(), numbers) };
            for (lane, (buffer, text)) in buffers.iter().zip(lens).enumerate() {
                let want = unsafe { canonical_at(buffer.as_ptr(), text.len(), b'u') }.map(|scrambled| number(scrambled, key));
                let have = (ok >> lane & 1 == 1).then_some(got[lane]);
                assert_eq!(have, want, "{:?}", String::from_utf8_lossy(text));
                canonical += usize::from(want.is_some());
            }
        }
        assert!(canonical > 4000, "only {canonical} canonical spellings tested");
    }

    #[test]
    fn id_of_packs_the_text() {
        let key = 0x7F_0123_4567;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..20_000u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Every length of the scrambled value, the ends of the range among them.
            let value = match round % 4 {
                0 => state & MASK,
                1 => (state % 36u64.pow((round % 9) as u32)).min(MASK),
                2 => MASK - state % 5,
                _ => state % 3,
            };
            // The number that scrambles to `value`.
            let number = number_of_half(half(value), key);
            assert_eq!(scrambled(number, key), value);
            let mut text = vec![b'u'];
            let mut digits = Vec::new();
            let mut rest = value;
            loop {
                digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(rest % 36) as usize]);
                rest /= 36;
                if rest == 0 {
                    break;
                }
            }
            text.extend(digits.iter().rev());
            assert_eq!(id_of(number, key, b'u'), id::pack(&text), "{:?}", String::from_utf8_lossy(&text));
        }
    }

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
        for bad in [
            &b""[..], b"u", b"U1", b"u01", b"u00", b"uA", b"u1 ", b" u1", b"e1", b"u123456789", b"uzzzzzzzz", b"u1\r", b"u1:", b"u1`",
            b"u1{", b"u1/", b"u1@", b"u1Z", b"u1\x80", b"u\xffz", b"uab_cd",
        ] {
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
