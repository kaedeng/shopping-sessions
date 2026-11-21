//! Slot `sort-grouped`: orders one bucket's keys by grouping them first.
//!
//! Contract, as `sort-std`: `sort(keys, key_bits, low_bits, scratch)` leaves `keys` ascending.
//!
//! A key is `local person << low_bits | ts`, and the persons are numbered densely, so one
//! counting scatter puts every person's keys together and only each person's own keys are left
//! to sort: about 30 of them at the generator's sizes, which `sort_unstable` does with its small
//! sorting networks. A comparison sort of the whole bucket does `log2(rows)` rounds over all of
//! it instead, and was 11% of the run's cycles on the x86 box (`x86.md`). A bucket that is one
//! huge person (a whale, a shared device) gains nothing here and costs one extra pass.

use crate::SortScratch;

/// Below this a bucket is sorted directly: the grouping passes would not pay for themselves.
const SMALL: usize = 64;

pub fn sort(keys: &mut Vec<u64>, key_bits: u32, low_bits: u32, scratch: &mut SortScratch) {
    let groups = 1usize << key_bits.saturating_sub(low_bits).min(usize::BITS - 1);
    if keys.len() < SMALL || groups < 2 {
        return keys.sort_unstable();
    }
    let SortScratch { keys: grouped, starts } = scratch;
    // `starts[g + 1]` counts group g, so the running sum leaves group g's first position in
    // `starts[g]`, and the scatter then moves it to one past g's last.
    starts.clear();
    starts.resize(groups + 1, 0);
    for &key in keys.iter() {
        // Checked: a key at or above `1 << key_bits` breaks the contract, and must not write
        // outside the table.
        starts[(key >> low_bits) as usize + 1] += 1;
    }
    let mut sum = 0;
    for start in starts.iter_mut() {
        sum += *start;
        *start = sum;
    }
    grouped.clear();
    // Room for the caller's sentinel too, so its push does not grow the buffer.
    grouped.reserve(keys.len() + 1);
    let (out, next) = (grouped.as_mut_ptr(), starts.as_mut_ptr());
    // Eight keys a round, their loads before the stores that depend on them: under the
    // store-bypass mitigation a load waits for every earlier store's address, and a key's
    // store address comes out of a load, so key by key each waits for the one before
    // (`sort_radix.rs`, `x86.md`).
    let mut chunks = keys.chunks_exact(8);
    for chunk in &mut chunks {
        let batch: [u64; 8] = chunk.try_into().expect("chunks of 8");
        let mut at = [0u32; 8];
        for (at, key) in at.iter_mut().zip(batch) {
            // SAFETY: the count pass indexed the same groups. Two keys of one group update
            // its position in turn.
            unsafe {
                let slot = next.add((key >> low_bits) as usize);
                *at = *slot;
                *slot = *at + 1;
            }
        }
        for (at, key) in at.into_iter().zip(batch) {
            // SAFETY: the running sums make the positions a permutation of `0..keys.len()`,
            // inside the reserved room.
            unsafe { out.add(at as usize).write(key) };
        }
    }
    for &key in chunks.remainder() {
        // SAFETY: as above.
        unsafe {
            let slot = next.add((key >> low_bits) as usize);
            out.add(*slot as usize).write(key);
            *slot += 1;
        }
    }
    // SAFETY: every position in `0..keys.len()` was written once above.
    unsafe { grouped.set_len(keys.len()) };
    let mut first = 0;
    for &end in &starts[..groups] {
        let end = end as usize;
        if end - first > 1 {
            grouped[first..end].sort_unstable();
        }
        first = end;
    }
    std::mem::swap(keys, grouped);
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
        // Sizes around the direct-sort bound, group counts from one to more groups than rows,
        // timestamps with and without ties.
        for rows in [0, 1, 2, 63, 64, 65, 1000, 20_000] {
            for (group_bits, low_bits) in [(0, 31), (1, 31), (4, 8), (11, 31), (17, 3)] {
                let mut keys: Vec<u64> = (0..rows).map(|_| next() >> (64 - group_bits - low_bits)).collect();
                let mut want = keys.clone();
                want.sort_unstable();
                super::sort(&mut keys, group_bits + low_bits, low_bits, &mut scratch);
                assert_eq!(keys, want, "{rows} rows, {group_bits} + {low_bits} bits");
            }
        }
    }
}
