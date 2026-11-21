//! Slot `sort-radix`: orders one bucket's keys without comparing them.
//!
//! Contract, as `sort-std`: `sort(keys, key_bits, low_bits, scratch)` leaves `keys` ascending.
//!
//! A least-significant-digit radix sort whose digits follow the key's two parts. The low
//! `low_bits` (the timestamp) are split into digits of at most 11 bits, and the bits above (the
//! bucket-local person, a dense number) go last, as one digit when the bucket has few enough
//! persons, since a pass costs the same whatever it sorts by. A 14-day window and under 4,096
//! persons make four passes. One read of the keys counts every digit, and a digit that is the
//! same in every key costs no pass.
//!
//! Both loops take their keys eight at a time: the loads first, then the stores whose addresses
//! depend on them. Docker's seccomp filter on a kernel before 5.16 turns on the store-bypass
//! mitigation, under which a load waits until every earlier store's address is known, and a
//! scatter's store address comes out of a load (the digit's position). Key by key, each load
//! then waits for the key before it. `x86.md` has the measurement: on the x86 box a pass took
//! 3.5 ns a key that way and 1.6 ns in eights, and this sort 10 ns a key against 16 to 18 for
//! `sort_unstable` at a bucket's size.

use crate::SortScratch;

/// Below this a bucket is sorted directly: the counting would cost more than it saves.
const SMALL: usize = 256;
/// Most bits in a timestamp digit: 2,048 write positions. The x86 box measured 8, 11 and 16
/// bits at 12.3, 10.3 and 12.4 ns a key for 20,000 keys.
const DIGIT: u32 = 11;
/// The persons are one digit up to this many bits (4,096 write positions); beyond it they are
/// split like the timestamp.
const GROUP_DIGIT_MAX: u32 = 12;
/// Keys per round of loads-then-stores; 16 and 32 measured slower than 8.
const BATCH: usize = 8;

pub fn sort(keys: &mut Vec<u64>, key_bits: u32, low_bits: u32, scratch: &mut SortScratch) {
    let rows = keys.len();
    let (key_bits, low_bits) = (key_bits.min(64), low_bits.min(key_bits.min(64)));
    if rows < SMALL || rows > u32::MAX as usize {
        return keys.sort_unstable();
    }
    // The digits, least significant first, as (shift, bits): each part in the fewest digits
    // its limit allows, of near-equal width.
    let mut digits = [(0u32, 0u32); 16];
    let mut count = 0;
    let mut split = |from: u32, to: u32, most: u32| {
        let parts = (to - from).div_ceil(most);
        let mut at = from;
        for part in 0..parts {
            let bits = (to - at).div_ceil(parts - part);
            digits[count] = (at, bits);
            count += 1;
            at += bits;
        }
    };
    split(0, low_bits, DIGIT);
    split(low_bits, key_bits, if key_bits - low_bits <= GROUP_DIGIT_MAX { GROUP_DIGIT_MAX } else { DIGIT });
    let digits = &digits[..count];

    // Every digit's counts from one read of the keys; digit `d`'s start at `base[d]`.
    let SortScratch { keys: spare, starts: counts } = scratch;
    let mut base = [0usize; 17];
    for (d, &(_, bits)) in digits.iter().enumerate() {
        base[d + 1] = base[d] + (1usize << bits);
    }
    counts.clear();
    counts.resize(base[count], 0);
    let tables = counts.as_mut_ptr();
    let mut chunks = keys.chunks_exact(4);
    let mut tally = |key: u64| {
        for (d, &(shift, bits)) in digits.iter().enumerate() {
            // SAFETY: a digit is below `1 << bits`, and `counts` holds that many from `base[d]`.
            unsafe { *tables.add(base[d] + ((key >> shift) as usize & ((1 << bits) - 1))) += 1 };
        }
    };
    for chunk in &mut chunks {
        // Four keys in registers before the first increment's store (module docs).
        let four: [u64; 4] = chunk.try_into().expect("chunks of 4");
        four.into_iter().for_each(&mut tally);
    }
    chunks.remainder().iter().copied().for_each(&mut tally);

    spare.clear();
    spare.reserve(rows + 1);
    // SAFETY: `u64` has no invalid values, and every slot is written by a pass before it is read.
    unsafe { spare.set_len(rows) };
    let (mut from, mut to) = (&mut keys[..], &mut spare[..]);
    let mut swapped = false;
    for (d, &(shift, bits)) in digits.iter().enumerate() {
        let counts = &mut counts[base[d]..base[d + 1]];
        // Every key has the same digit: the pass would copy them in order.
        if counts.iter().any(|&count| count as usize == rows) {
            continue;
        }
        // Counts to first positions.
        let mut sum = 0;
        for count in counts.iter_mut() {
            (*count, sum) = (sum, sum + *count);
        }
        // SAFETY: every digit is below `1 << bits` = `counts.len()`, and the counts make the
        // positions a permutation of `0..rows`, the length of `to`.
        unsafe { scatter(from, to.as_mut_ptr(), counts.as_mut_ptr(), shift, (1usize << bits) - 1) };
        (from, to) = (to, from);
        swapped = !swapped;
    }
    if swapped {
        // The sorted keys are in the spare buffer, which has room for the caller's sentinel.
        std::mem::swap(keys, spare);
    }
}

/// Moves every key of `from` to the next position of its digit, `BATCH` keys a round.
///
/// # Safety
/// `next` holds a position for every digit `key >> shift & mask` can be, and following them
/// stays inside `to`.
unsafe fn scatter(from: &[u64], to: *mut u64, next: *mut u32, shift: u32, mask: usize) {
    let mut chunks = from.chunks_exact(BATCH);
    for chunk in &mut chunks {
        let batch: [u64; BATCH] = chunk.try_into().expect("chunks of BATCH");
        let mut at = [0u32; BATCH];
        for (at, key) in at.iter_mut().zip(batch) {
            // SAFETY: the caller's contract. Two keys of one digit update it in turn.
            unsafe {
                let slot = next.add((key >> shift) as usize & mask);
                *at = *slot;
                *slot = *at + 1;
            }
        }
        for (at, key) in at.into_iter().zip(batch) {
            // SAFETY: the caller's contract.
            unsafe { *to.add(at as usize) = key };
        }
    }
    for &key in chunks.remainder() {
        // SAFETY: the caller's contract.
        unsafe {
            let slot = next.add((key >> shift) as usize & mask);
            *to.add(*slot as usize) = key;
            *slot += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::SortScratch;

    #[test]
    fn sorts_like_sort_unstable() {
        let mut scratch = SortScratch::default();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Sizes around the direct-sort bound and the batch; key shapes from no group bits to
        // wide ones, a full 64-bit key, and keys that share their upper digits.
        for rows in [0, 1, 255, 256, 257, 263, 5000, 70_000] {
            for (group_bits, low_bits) in [(0, 31), (1, 31), (4, 8), (11, 31), (12, 31), (13, 31), (17, 3), (32, 32), (10, 0)] {
                let bits = group_bits + low_bits;
                for narrow in [false, true] {
                    let mut keys: Vec<u64> = (0..rows)
                        .map(|_| {
                            let key = next() >> (64 - bits);
                            if narrow { key & 0xFFFF } else { key }
                        })
                        .collect();
                    let mut want = keys.clone();
                    want.sort_unstable();
                    super::sort(&mut keys, bits, low_bits, &mut scratch);
                    assert_eq!(keys, want, "{rows} rows, {group_bits} + {low_bits} bits, narrow {narrow}");
                }
            }
        }
    }
}
