//! Slot `id-u64`: every ID (event, user, click, product, campaign) is one `u64`.
//!
//! - `0`: the empty ID, an empty field. Never a key in any table, so tables use it for "free".
//! - top bit clear: 1 to 9 ASCII bytes, 7 bits each, the first byte in bits 62..56, zero-padded
//!   below. Two packed IDs compare as their bytes do.
//! - top bit set (`SLOW`): the slow path, an index into the global intern list. Taken by an ID
//!   longer than 9 bytes, or holding a byte >= 0x80 or a 0x00 (which would pack like padding).
//!   Equal bytes always give the equal `u64`, so hashing and `==` need no special case; only
//!   order does, and `less` handles it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub type Id = u64;
pub const EMPTY: Id = 0;
pub const SLOW: Id = 1 << 63;

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
        out.extend_from_slice(&interner().lock().unwrap().list[(id & !SLOW) as usize]);
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

#[derive(Default)]
struct Interner {
    map: HashMap<Box<[u8]>, Id>,
    list: Vec<Box<[u8]>>,
}

fn interner() -> &'static Mutex<Interner> {
    static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();
    INTERNER.get_or_init(Default::default)
}

#[cold]
#[inline(never)]
unsafe fn intern_at(p: *const u8, len: usize) -> Id {
    intern(unsafe { std::slice::from_raw_parts(p, len) })
}

/// Only for bytes that cannot pack, or one string would get two IDs.
fn intern(bytes: &[u8]) -> Id {
    let mut interner = interner().lock().unwrap();
    if let Some(&id) = interner.map.get(bytes) {
        return id;
    }
    let id = SLOW | interner.list.len() as u64;
    interner.list.push(bytes.into());
    interner.map.insert(bytes.into(), id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

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
