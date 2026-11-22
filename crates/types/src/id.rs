//! Slot `id-u64`: every ID (event, user, click, product, campaign) is one `u64`.
//!
//! - `0`: the empty ID, an empty field. Never a key in any table, so tables use it for "free".
//! - top bit clear: 1 to 9 ASCII bytes, 7 bits each, the first byte in bits 62..56, zero-padded
//!   below. Two packed IDs compare as their bytes do.
//! - top bit set (`SLOW`): the slow path, a place in the global intern tables. Taken by an ID
//!   longer than 9 bytes, or holding a byte >= 0x80 or a 0x00 (which would pack like padding).
//!   Equal bytes always give the equal `u64`, so hashing and `==` need no special case; only
//!   order does, and `less` handles it.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Mutex, OnceLock};

pub type Id = u64;
pub const EMPTY: Id = 0;
pub const SLOW: Id = 1 << 63;
/// With `SLOW`: not a place in the intern tables but a hash of the ID's bytes (`key_at`).
pub const HASHED: Id = SLOW | 1 << 62;

const HIGH: u64 = 0x8080_8080_8080_8080;
const ONES: u64 = 0x0101_0101_0101_0101;

/// The 7 low bits of each of 8 bytes, squeezed into 56 bits.
#[inline(always)]
fn squeeze(x: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    #[allow(unused_unsafe)]
    unsafe {
        core::arch::x86_64::_pext_u64(x, 0x7F7F_7F7F_7F7F_7F7F)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let x = ((x & 0x7F00_7F00_7F00_7F00) >> 1) | (x & 0x007F_007F_007F_007F);
        let x = ((x & 0x3FFF_0000_3FFF_0000) >> 2) | (x & 0x0000_3FFF_0000_3FFF);
        ((x & 0x0FFF_FFFF_0000_0000) >> 4) | (x & 0x0000_0000_0FFF_FFFF)
    }
}

#[inline(always)]
fn unsqueeze(x: u64) -> u64 {
    let x = ((x << 4) & 0x0FFF_FFFF_0000_0000) | (x & 0x0000_0000_0FFF_FFFF);
    let x = ((x << 2) & 0x3FFF_0000_3FFF_0000) | (x & 0x0000_3FFF_0000_3FFF);
    ((x << 1) & 0x7F00_7F00_7F00_7F00) | (x & 0x007F_007F_007F_007F)
}

/// The ID of the `len` bytes at `p`. Branch-free unless the ID takes the slow path.
///
/// # Safety
/// 16 bytes must be readable at `p`, whatever `len` is (the parser's chunks are padded).
#[inline(always)]
pub unsafe fn pack_at(p: *const u8, len: usize) -> Id {
    if len > 9 {
        return unsafe { intern_at(p, len) };
    }
    let word = u64::from_le(unsafe { (p as *const u64).read_unaligned() });
    let half = 4 * (8 - len.min(8)) as u32;
    let mask = (!0u64 >> half) >> half;
    let ninth = unsafe { *p.add(8) } as u64 & ((len == 9) as u64).wrapping_neg();
    let word = word & mask;
    let filled = word | !mask;
    let zero_byte = filled.wrapping_sub(ONES) & !filled & HIGH;
    let unpackable =
        (word & HIGH) | zero_byte | (ninth & 0x80) | ((len == 9) & (ninth == 0)) as u64;
    if unpackable != 0 {
        return unsafe { intern_at(p, len) };
    }
    (squeeze(word.swap_bytes()) << 7) | ninth
}

/// An ID that is only ever compared for equality (an `event_id`: nothing reads its bytes or
/// orders it): `pack_at` for up to 9 bytes, and for more a hash of the bytes with `HASHED` on
/// top. One ID in eight is 33 bytes, each `event_id` is its own, and interning two and a
/// half million of them was half of the parse at 20M events. Two long IDs get one key with a
/// chance of 2^-62 a pair: about one run in 200,000 at 60M events.
///
/// # Safety
/// As `pack_at`, and `len` bytes readable at `p`.
#[inline(always)]
pub unsafe fn key_at(p: *const u8, len: usize) -> Id {
    if len > 9 {
        return unsafe { hashed_at(p, len) };
    }
    unsafe { pack_at(p, len) }
}

#[inline(never)]
unsafe fn hashed_at(p: *const u8, len: usize) -> Id {
    let mut hasher = Mixer::default();
    hasher.write(unsafe { std::slice::from_raw_parts(p, len) });
    HASHED | hasher.finish() >> 2
}

/// The ID of `bytes`, for cold paths: the lookup files, tests.
pub fn pack(bytes: &[u8]) -> Id {
    if bytes.len() > 9 {
        return intern(bytes);
    }
    let mut padded = [0u8; 16];
    padded[..bytes.len()].copy_from_slice(bytes);
    unsafe { pack_at(padded.as_ptr(), bytes.len()) }
}

/// A packed (not `SLOW`) ID back as bytes: the first 8 in a little-endian word, the 9th, the length.
#[inline(always)]
pub fn spread(id: Id) -> (u64, u8, usize) {
    let ninth = (id & 0x7F) as u8;
    let word = unsqueeze(id >> 7).swap_bytes();
    let len = if ninth != 0 { 9 } else { 8 - (word.leading_zeros() / 8) as usize };
    (word, ninth, len)
}

/// Appends the ID's bytes to `out`.
pub fn unpack(id: Id, out: &mut Vec<u8>) {
    if id & SLOW != 0 {
        with_bytes(id, |bytes| out.extend_from_slice(bytes));
        return;
    }
    let (word, ninth, len) = spread(id);
    let mut bytes = [0u8; 9];
    bytes[..8].copy_from_slice(&word.to_le_bytes());
    bytes[8] = ninth;
    out.extend_from_slice(&bytes[..len]);
}

/// Byte-wise `a < b`. One compare unless a slow-path ID is involved.
#[inline(always)]
pub fn less(a: Id, b: Id) -> bool {
    if (a | b) & SLOW == 0 { a < b } else { less_slow(a, b) }
}

#[cold]
#[inline(never)]
fn less_slow(a: Id, b: Id) -> bool {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    unpack(a, &mut x);
    unpack(b, &mut y);
    x < y
}

/// Eight IDs packed at once (AVX-512), for the positional parse's batches (`ingest::pos`).
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw"))]
pub mod wide {
    use core::arch::x86_64::*;

    use super::{HIGH, ONES};

    /// `pack_at` for 8 IDs. Lane `i` is an ID of `len[i]` bytes whose bytes 0..8 are
    /// `first[i]` (a little-endian word; bytes past the ID are anything) and whose byte 8 is
    /// the low byte of `ninth[i]`. Returns the packed IDs and the lanes that pack. Any other
    /// lane's ID takes `pack_at`'s slow path (over 9 bytes, or a byte that does not pack), and
    /// its value here means nothing. Agrees with `pack_at` on every input
    /// (`tests::wide_packs_as_pack_at`).
    #[inline(always)]
    pub fn pack(first: __m512i, ninth: __m512i, len: __m512i) -> (__m512i, __mmask8) {
        // SAFETY: register arithmetic; AVX-512 F and BW are compiled in (cfg above).
        #[allow(unused_unsafe)]
        unsafe {
            let all = |value: u64| _mm512_set1_epi64(value as i64);
            let short = _mm512_cmple_epu64_mask(len, all(9));
            // The ID's first min(len, 8) bytes: ones shifted right by the bytes not kept (a
            // count of 64 gives 0, which is what an empty ID needs).
            let kept = _mm512_min_epu64(len, all(8));
            let mask = _mm512_srlv_epi64(all(u64::MAX), _mm512_slli_epi64::<3>(_mm512_sub_epi64(all(8), kept)));
            let nine = _mm512_cmpeq_epi64_mask(len, all(9));
            let ninth = _mm512_maskz_mov_epi64(nine, _mm512_and_si512(ninth, all(0xFF)));
            let word = _mm512_and_si512(first, mask);
            let filled = _mm512_or_si512(word, _mm512_xor_si512(mask, all(u64::MAX)));
            let zero_byte = _mm512_and_si512(_mm512_andnot_si512(filled, _mm512_sub_epi64(filled, all(ONES))), all(HIGH));
            let unpackable = _mm512_or_si512(_mm512_or_si512(_mm512_and_si512(word, all(HIGH)), zero_byte), _mm512_and_si512(ninth, all(0x80)));
            let bad = _mm512_test_epi64_mask(unpackable, unpackable) | (nine & _mm512_testn_epi64_mask(ninth, ninth));
            // The bytes in order of significance, then the 7 low bits of each squeezed
            // together (`squeeze`'s shifts: there is no `pext` on a vector).
            let reverse = _mm512_broadcast_i32x4(_mm_set_epi8(8, 9, 10, 11, 12, 13, 14, 15, 0, 1, 2, 3, 4, 5, 6, 7));
            let x = _mm512_shuffle_epi8(word, reverse);
            let x = _mm512_or_si512(_mm512_srli_epi64::<1>(_mm512_and_si512(x, all(0x7F00_7F00_7F00_7F00))), _mm512_and_si512(x, all(0x007F_007F_007F_007F)));
            let x = _mm512_or_si512(_mm512_srli_epi64::<2>(_mm512_and_si512(x, all(0x3FFF_0000_3FFF_0000))), _mm512_and_si512(x, all(0x0000_3FFF_0000_3FFF)));
            let x = _mm512_or_si512(_mm512_srli_epi64::<4>(_mm512_and_si512(x, all(0x0FFF_FFFF_0000_0000))), _mm512_and_si512(x, all(0x0000_0000_0FFF_FFFF)));
            (_mm512_or_si512(_mm512_slli_epi64::<7>(x), ninth), short & !bad)
        }
    }
}

/// The slow path's IDs, in `SHARDS` tables picked by the bytes' hash: an ID is its table's
/// number and its place in that table's list. (As one table behind one mutex, every worker
/// of the parse queued for it once one ID in eight was longer than 9 bytes: 6.3 s of a 6.7 s
/// run at 20M events.)
#[derive(Default)]
struct Shard {
    map: HashMap<Box<[u8]>, Id, BuildHasherDefault<Mixer>>,
    list: Vec<Box<[u8]>>,
}

const SHARD_BITS: u32 = 10;
const SHARDS: usize = 1 << SHARD_BITS;
/// Bits of an ID's place in its shard's list, below the shard's number.
const PLACE_BITS: u32 = 40;

/// A multiply and a fold a word: the IDs are short, and the default hasher was most of the
/// time spent here.
#[derive(Default)]
struct Mixer(u64);

impl Mixer {
    #[inline(always)]
    fn mix(&mut self, word: u64) {
        let wide = u128::from(self.0 ^ word) * 0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835u128;
        self.0 = (wide as u64) ^ (wide >> 64) as u64;
    }
}

impl Hasher for Mixer {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.mix(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(last) ^ (rest.len() as u64) << 56);
        }
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.mix(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

fn shards() -> &'static [Mutex<Shard>] {
    static SHARDS_OF: OnceLock<Vec<Mutex<Shard>>> = OnceLock::new();
    SHARDS_OF.get_or_init(|| (0..SHARDS).map(|_| Mutex::default()).collect())
}

/// The bytes of slow-path ID `id`, handed to `read`.
fn with_bytes<T>(id: Id, read: impl FnOnce(&[u8]) -> T) -> T {
    let (shard, place) = (((id & !SLOW) >> PLACE_BITS) as usize, (id & ((1 << PLACE_BITS) - 1)) as usize);
    let shard = shards()[shard].lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    read(&shard.list[place])
}

#[cold]
#[inline(never)]
unsafe fn intern_at(p: *const u8, len: usize) -> Id {
    intern(unsafe { std::slice::from_raw_parts(p, len) })
}

/// The interned ID of any bytes, those that pack too: for a table that keeps IDs of one
/// kind apart from keys of its own (`types::users`), and never mixes them with `pack`'s.
pub fn intern_any(bytes: &[u8]) -> Id {
    intern(bytes)
}

/// Only for bytes that cannot pack, or one string would get two IDs.
fn intern(bytes: &[u8]) -> Id {
    let mut hasher = Mixer::default();
    hasher.write(bytes);
    // Not the bits the table itself takes from this hash: its low ones and its top seven.
    let number = (hasher.finish() >> 32) as usize & (SHARDS - 1);
    let mut shard = shards()[number].lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&id) = shard.map.get(bytes) {
        return id;
    }
    let id = SLOW | (number as u64) << PLACE_BITS | shard.list.len() as u64;
    shard.list.push(bytes.into());
    shard.map.insert(bytes.into(), id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw"))]
    #[test]
    fn wide_packs_as_pack_at() {
        use core::arch::x86_64::*;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut packed = 0;
        for round in 0..40_000u64 {
            // 8 IDs of 0 to 11 bytes, mostly of bytes that pack, each followed by other bytes.
            let alphabet: &[u8] = if round % 3 == 0 { b"k0z9aA_-\x00\x7f\x80\xff\x01," } else { b"kuec0123456789abcxyz" };
            let mut buffers = [[0u8; 16]; 8];
            let (mut first, mut ninth, mut len) = ([0u64; 8], [0u64; 8], [0u64; 8]);
            for lane in 0..8 {
                for byte in buffers[lane].iter_mut() {
                    *byte = alphabet[(next() % alphabet.len() as u64) as usize];
                }
                len[lane] = next() % 12;
                first[lane] = u64::from_le_bytes(buffers[lane][..8].try_into().unwrap());
                // The low byte is the ID's ninth; the rest is whatever came with it.
                ninth[lane] = u64::from(buffers[lane][8]) | next() << 8;
            }
            // SAFETY: each array holds 8 words.
            let load = |words: &[u64; 8]| unsafe { _mm512_loadu_si512(words.as_ptr().cast()) };
            let (ids, ok) = wide::pack(load(&first), load(&ninth), load(&len));
            let mut got = [0u64; 8];
            unsafe { _mm512_storeu_si512(got.as_mut_ptr().cast(), ids) };
            for lane in 0..8 {
                let want = pack(&buffers[lane][..len[lane] as usize]);
                let text = &buffers[lane][..len[lane] as usize];
                assert_eq!(ok >> lane & 1 == 1, want & SLOW == 0, "{text:?}");
                if want & SLOW == 0 {
                    assert_eq!(got[lane], want, "{text:?}");
                    packed += 1;
                }
            }
        }
        assert!(packed > 100_000, "only {packed} IDs packed");
    }

    #[test]
    fn packs_round_trip_and_keep_byte_order() {
        let ids: [&[u8]; 14] = [
            b"", b"\t", b" u1", b"A\tB", b"U1", b"e3\r", b"u1", b"u1 ", b"u10", b"u2",
            b"u2abcdefg", b"user_id_a", b"user_id_b", b"zzzzzzzzz",
        ];
        for pair in ids.windows(2) {
            assert!(pair[0] < pair[1], "the list is sorted");
            assert!(less(pack(pair[0]), pack(pair[1])), "{pair:?}");
            assert!(!less(pack(pair[1]), pack(pair[0])), "{pair:?}");
        }
        for bytes in ids {
            let id = pack(bytes);
            assert_eq!(id & SLOW, 0);
            let mut back = Vec::new();
            unpack(id, &mut back);
            assert_eq!(back, bytes);
        }
        assert_eq!(pack(b""), EMPTY);
    }

    #[test]
    fn unpackable_ids_are_interned_once_and_ordered_by_bytes() {
        let slow: [&[u8]; 4] = [b"u\0", b"0123456789", "u\u{661}".as_bytes(), b"12345678\0"];
        for bytes in slow {
            let id = pack(bytes);
            assert_ne!(id & SLOW, 0, "{bytes:?}");
            assert_eq!(id, pack(bytes));
            let mut back = Vec::new();
            unpack(id, &mut back);
            assert_eq!(back, bytes);
        }
        // "0123456789" < "u" < "u\0" < "u1" byte-wise, across the two representations.
        assert!(less(pack(b"0123456789"), pack(b"u")));
        assert!(less(pack(b"u"), pack(b"u\0")));
        assert!(less(pack(b"u\0"), pack(b"u1")));
        assert!(!less(pack(b"u1"), pack(b"u\0")));
    }
}
