//! One person's event times in ascending order, with no branch that depends on the times.
//!
//! `sort_unstable` decides by comparing: on a person's thirty times, in no order, about every
//! other decision is mispredicted, and the bucket stage's sorts were a tenth of a run's branch
//! misses. Here:
//!
//! - up to 16 times are one register through a sorting network: ten layers, each a permute,
//!   a minimum, a maximum and a blend;
//! - up to `NET` times are 2, 4, 8 or 16 registers, each sorted that way and then merged as
//!   registers, without leaving them (`net`);
//! - more are sorted by their bytes, four counting passes (a pass whose byte is the same in
//!   every time is skipped).
//!
//! `cut` is for the caller that only cuts the times into sessions: up to `NET` times it is
//! the sort, and more are not sorted at all.
//!
//! Without AVX-512 F at build time this is `sort_unstable`.

/// Buffers, one set per worker.
#[derive(Default)]
pub(crate) struct Scratch {
    spare: Vec<u32>,
    counts: Vec<[u32; 256]>,
    /// `cut`: per step of time, the earliest and the latest time in it, and how many.
    low: Vec<u32>,
    high: Vec<u32>,
    held: Vec<u32>,
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
pub(crate) fn sort(times: &mut [u32], _scratch: &mut Scratch) {
    times.sort_unstable();
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
pub(crate) fn sort(times: &mut [u32], scratch: &mut Scratch) {
    let n = times.len();
    let at = times.as_mut_ptr();
    // SAFETY: `n` lanes at `times`, at most the registers of the way taken.
    unsafe {
        if n <= 16 {
            if n >= 2 {
                leaf(at, n);
            }
        } else if n <= 32 {
            net::<2>(at, n);
        } else if n <= 64 {
            net::<4>(at, n);
        } else if n <= 128 {
            net::<8>(at, n);
        } else if n <= NET {
            net::<16>(at, n);
        } else {
            by_bytes(times, scratch);
        }
    }
}

/// Up to this many times, in registers (`net`); more, by bytes.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const NET: usize = 256;

/// The step of time `cut` counts by, as a multiplier: time `t` is in step
/// `(t - first) * STEP >> 32`, which is 1,799,316 ms a step.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const STEP: u64 = 2387;

// Two times of one step are less than a step apart, and so in one session.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const _: () = assert!((1u64 << 32) / STEP <= crate::partitioned::GAP);

/// One person's times for the session cut, which reads nothing of them but where a time is
/// more than the gap after the one before: `times` is left ascending and with the sessions
/// its sorted times have, each session as its first time, once for every time it holds.
///
/// Up to `NET` times that is `sort`. More are not sorted at all. A step of time shorter
/// than the gap holds times of one session only, so all the cut needs of a step is its
/// earliest time, its latest and how many it holds: one pass over the times. The steps in
/// order then give the sessions (a step begins one when its earliest time is more than the
/// gap after the latest of the step before it that holds any), and each writes its
/// session's first time over as many places as it holds times.
///
/// By their bytes, such a person's times took five passes, four of them writing every time
/// to a place the pass before had counted.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
pub(crate) fn cut(times: &mut [u32], scratch: &mut Scratch) {
    use core::arch::x86_64::*;
    let n = times.len();
    if n <= NET {
        return sort(times, scratch);
    }
    let gap = crate::partitioned::GAP as u32;
    let Scratch { low, high, held, .. } = scratch;
    // SAFETY: loads and stores of 16 lanes inside `times`, masked where fewer are left; a
    // time's step is below `steps`, which the three columns have room for and 64 more.
    unsafe {
        let at = times.as_mut_ptr();
        // The first and the last of the times.
        let (mut least, mut most) = (_mm512_set1_epi32(-1), _mm512_setzero_si512());
        let mut i = 0;
        while i + 16 <= n {
            let v = _mm512_loadu_si512(at.add(i).cast());
            (least, most) = (_mm512_min_epu32(least, v), _mm512_max_epu32(most, v));
            i += 16;
        }
        if i < n {
            let live = ((1u32 << (n - i)) - 1) as __mmask16;
            let v = _mm512_maskz_loadu_epi32(live, at.add(i).cast());
            (least, most) = (_mm512_mask_min_epu32(least, live, least, v), _mm512_mask_max_epu32(most, live, most, v));
        }
        let (first, last) = (_mm512_reduce_min_epu32(least), _mm512_reduce_max_epu32(most));
        let steps = ((u64::from(last - first) * STEP) >> 32) as usize + 1;
        for column in [&mut *low, &mut *high, &mut *held] {
            if column.len() < steps + 64 {
                column.resize(steps + 64, 0);
            }
        }
        low[..steps].fill(u32::MAX);
        high[..steps].fill(0);
        held[..steps + 64].fill(0);
        let (low, high, held) = (low.as_mut_ptr(), high.as_mut_ptr(), held.as_mut_ptr());
        for i in 0..n {
            let time = *at.add(i);
            let step = ((u64::from(time - first) * STEP) >> 32) as usize;
            *low.add(step) = (*low.add(step)).min(time);
            *high.add(step) = (*high.add(step)).max(time);
            *held.add(step) += 1;
        }
        // The steps in order, 64 a round: those that hold a time, a bit each.
        let (mut to, mut start, mut before) = (0usize, first, first);
        let mut from = 0;
        while from < steps {
            let mut some = 0u64;
            let mut part = 0;
            while part < 4 {
                let counts = _mm512_loadu_si512(held.add(from + 16 * part).cast());
                some |= u64::from(_mm512_test_epi32_mask(counts, counts)) << (16 * part);
                part += 1;
            }
            while some != 0 {
                let step = from + some.trailing_zeros() as usize;
                some &= some - 1;
                let (count, earliest, latest) = (*held.add(step) as usize, *low.add(step), *high.add(step));
                // A session begins here or goes on: by arithmetic, it is as good as a coin's toss.
                let begins = u32::from(earliest.wrapping_sub(before) > gap).wrapping_neg();
                start = (earliest & begins) | (start & !begins);
                before = latest;
                let value = _mm512_set1_epi32(start as i32);
                let mut done = 0;
                loop {
                    let place = to + done;
                    if place + 16 <= n {
                        _mm512_storeu_si512(at.add(place).cast(), value);
                    } else {
                        _mm512_mask_storeu_epi32(at.add(place).cast(), ((1u32 << (n - place)) - 1) as __mmask16, value);
                    }
                    done += 16;
                    if done >= count {
                        break;
                    }
                }
                to += count;
            }
            from += 64;
        }
        debug_assert_eq!(to, n);
    }
}

/// The lanes of a register, last first.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[repr(C, align(64))]
struct Lanes([u32; 16]);

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
static REVERSED: Lanes = Lanes([15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);

/// A register whose lanes rise and then fall, or fall and then rise, put in ascending order:
/// each lane against the one 8, 4, 2 and 1 on.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
unsafe fn settle(v: core::arch::x86_64::__m512i) -> core::arch::x86_64::__m512i {
    use core::arch::x86_64::*;
    // SAFETY: register arithmetic; AVX-512 F is compiled in.
    unsafe {
        let other = _mm512_shuffle_i64x2::<0x4E>(v, v);
        let v = _mm512_mask_blend_epi32(0xFF00, _mm512_min_epu32(v, other), _mm512_max_epu32(v, other));
        let other = _mm512_shuffle_i64x2::<0xB1>(v, v);
        let v = _mm512_mask_blend_epi32(0xF0F0, _mm512_min_epu32(v, other), _mm512_max_epu32(v, other));
        let other = _mm512_shuffle_epi32::<0x4E>(v);
        let v = _mm512_mask_blend_epi32(0xCCCC, _mm512_min_epu32(v, other), _mm512_max_epu32(v, other));
        let other = _mm512_shuffle_epi32::<0xB1>(v);
        _mm512_mask_blend_epi32(0xAAAA, _mm512_min_epu32(v, other), _mm512_max_epu32(v, other))
    }
}

/// Sorts the `n` times at `at`, at most `16 * K` of them, in `K` registers (2, 4, 8 or 16):
/// each register through the sorter of 16 lanes, then runs of one, two, four and eight
/// registers merged pair by pair. A merge is the bitonic one: the first run against the
/// second in reverse (the larger halves kept in that reversed order, which the steps after
/// do not mind), then each half against itself at half the distance, down to a lane.
///
/// No time is compared by a jump and none goes through memory between the load and the
/// store. (These were leaves of 16 merged one time after another, the smaller of two taken
/// by arithmetic: a loop whose every step waited for the load of the step before, 7 cycles a
/// time and round of merges, and two fifths of what the sorts cost.)
///
/// # Safety
/// `n` values readable and writable at `at`, `n <= 16 * K`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(never)]
unsafe fn net<const K: usize>(at: *mut u32, n: usize) {
    use core::arch::x86_64::*;
    debug_assert!(K.is_power_of_two() && K >= 2 && n <= 16 * K);
    // SAFETY: masked loads and stores touch the `n` live lanes only (a register past them
    // has no live lane, and its address is not followed); the rest is registers.
    unsafe {
        // The lanes past the times hold the largest value, and stay behind them.
        let top = _mm512_set1_epi32(-1);
        let mut live = [0u16; K];
        let mut v = [top; K];
        let mut i = 0;
        while i < K {
            live[i] = ((1u32 << n.saturating_sub(16 * i).min(16)) - 1) as u16;
            v[i] = _mm512_mask_loadu_epi32(top, live[i], at.wrapping_add(16 * i).cast());
            i += 1;
        }
        let mut layer = 0;
        while layer < LAYERS {
            let partner = _mm512_load_si512(NETWORK.partner[layer].as_ptr().cast());
            let larger = NETWORK.larger[layer];
            let mut i = 0;
            while i < K {
                let other = _mm512_permutexvar_epi32(partner, v[i]);
                v[i] = _mm512_mask_blend_epi32(larger, _mm512_min_epu32(v[i], other), _mm512_max_epu32(v[i], other));
                i += 1;
            }
            layer += 1;
        }
        let reversed = _mm512_load_si512(REVERSED.0.as_ptr().cast());
        // Runs of `width` registers into runs of twice that.
        let mut width = 1;
        while width < K {
            let old = v;
            let mut run = 0;
            while run < K {
                let mut i = 0;
                while i < width {
                    let (a, b) = (old[run + i], _mm512_permutexvar_epi32(reversed, old[run + 2 * width - 1 - i]));
                    v[run + i] = _mm512_min_epu32(a, b);
                    v[run + width + i] = _mm512_max_epu32(a, b);
                    i += 1;
                }
                run += 2 * width;
            }
            let mut apart = width / 2;
            while apart >= 1 {
                let mut from = 0;
                while from < K {
                    let mut i = 0;
                    while i < apart {
                        let (a, b) = (v[from + i], v[from + i + apart]);
                        v[from + i] = _mm512_min_epu32(a, b);
                        v[from + i + apart] = _mm512_max_epu32(a, b);
                        i += 1;
                    }
                    from += 2 * apart;
                }
                apart /= 2;
            }
            let mut i = 0;
            while i < K {
                v[i] = settle(v[i]);
                i += 1;
            }
            width *= 2;
        }
        let mut i = 0;
        while i < K {
            _mm512_mask_storeu_epi32(at.wrapping_add(16 * i).cast(), live[i], v[i]);
            i += 1;
        }
    }
}

/// The bitonic sorter of 16 lanes: per layer, each lane's partner (itself for none) and the
/// lanes that keep the larger of the two.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const LAYERS: usize = 10;

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[repr(C, align(64))]
struct Network {
    partner: [[u32; 16]; LAYERS],
    larger: [u16; LAYERS],
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const fn network() -> Network {
    let mut net = Network { partner: [[0; 16]; LAYERS], larger: [0; LAYERS] };
    let mut layer = 0;
    while layer < LAYERS {
        let mut lane = 0;
        while lane < 16 {
            net.partner[layer][lane] = lane as u32;
            lane += 1;
        }
        layer += 1;
    }
    let mut layer = 0;
    let mut size = 2;
    while size <= 16 {
        // Blocks of `size`: the first half against the second in reverse.
        let mut block = 0;
        while block < 16 {
            let mut i = 0;
            while i < size / 2 {
                let (low, high) = (block + i, block + size - 1 - i);
                net.partner[layer][low] = high as u32;
                net.partner[layer][high] = low as u32;
                net.larger[layer] |= 1 << high;
                i += 1;
            }
            block += size;
        }
        layer += 1;
        // Then halves of halves, each lane against the one half a block on.
        let mut part = size / 2;
        while part >= 2 {
            let mut block = 0;
            while block < 16 {
                let mut i = 0;
                while i < part / 2 {
                    let (low, high) = (block + i, block + i + part / 2);
                    net.partner[layer][low] = high as u32;
                    net.partner[layer][high] = low as u32;
                    net.larger[layer] |= 1 << high;
                    i += 1;
                }
                block += part;
            }
            layer += 1;
            part /= 2;
        }
        size *= 2;
    }
    assert!(layer == LAYERS);
    net
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
static NETWORK: Network = network();

/// Sorts the `n` times at `at`, 1 to 16 of them, in place.
///
/// # Safety
/// `n` values readable and writable at `at`, `1 <= n <= 16`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
unsafe fn leaf(at: *mut u32, n: usize) {
    use core::arch::x86_64::*;
    // SAFETY: masked loads and stores touch the `n` live lanes only; the rest is registers.
    unsafe {
        let live = ((1u32 << n) - 1) as __mmask16;
        // The lanes past the times hold the largest value, and stay behind them.
        let mut v = _mm512_mask_loadu_epi32(_mm512_set1_epi32(-1), live, at.cast());
        let mut layer = 0;
        while layer < LAYERS {
            let other = _mm512_permutexvar_epi32(_mm512_load_si512(NETWORK.partner[layer].as_ptr().cast()), v);
            v = _mm512_mask_blend_epi32(NETWORK.larger[layer], _mm512_min_epu32(v, other), _mm512_max_epu32(v, other));
            layer += 1;
        }
        _mm512_mask_storeu_epi32(at.cast(), live, v);
    }
}

/// More than `NET` times: by their four bytes, the lowest first.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
fn by_bytes(times: &mut [u32], scratch: &mut Scratch) {
    let n = times.len();
    let Scratch { spare, counts, .. } = scratch;
    counts.clear();
    counts.resize(4, [0; 256]);
    for &time in times.iter() {
        counts[0][(time & 255) as usize] += 1;
        counts[1][(time >> 8 & 255) as usize] += 1;
        counts[2][(time >> 16 & 255) as usize] += 1;
        counts[3][(time >> 24) as usize] += 1;
    }
    spare.clear();
    spare.reserve(n);
    // SAFETY: `spare` has room for `n` values, and a pass writes all `n` of its target (the
    // counts are of these same values) before the next reads them.
    unsafe {
        spare.set_len(n);
        let (mut from, mut to) = (times.as_mut_ptr(), spare.as_mut_ptr());
        for (byte, count) in counts.iter_mut().enumerate() {
            let shift = 8 * byte as u32;
            // Every time has this byte: nothing to move.
            if count[(*from >> shift & 255) as usize] as usize == n {
                continue;
            }
            let mut sum = 0u32;
            for slot in count.iter_mut() {
                (*slot, sum) = (sum, sum + *slot);
            }
            let next = count.as_mut_ptr();
            for at in 0..n {
                let time = *from.add(at);
                let slot = next.add((time >> shift & 255) as usize);
                *to.add(*slot as usize) = time;
                *slot += 1;
            }
            (from, to) = (to, from);
        }
        if from != times.as_mut_ptr() {
            std::ptr::copy_nonoverlapping(from, times.as_mut_ptr(), n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every length around the ways' bounds, times spread wide and times packed close
    /// (many equal bytes, many equal times), against `sort_unstable`.
    #[test]
    fn sorts_like_sort_unstable() {
        let mut scratch = Scratch::default();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let lengths = (0..=70).chain([127, 128, 129, 255, 256, 257, 383, 384, 385, 1000, 5000, 70_000]);
        for n in lengths {
            for spread in [1u64 << 31, 1 << 20, 300, 3] {
                for _ in 0..if n < 300 { 40 } else { 3 } {
                    let base = (next() % (1 << 30)) as u32;
                    let mut times: Vec<u32> = (0..n).map(|_| base + (next() % spread) as u32).collect();
                    let mut want = times.clone();
                    want.sort_unstable();
                    sort(&mut times, &mut scratch);
                    assert_eq!(times, want, "{n} times over {spread}");
                }
            }
        }
    }

    /// `cut` against the sessions of the sorted times: every length around its bounds, gaps
    /// at the session bound and at a step's length, one more and one less, times that repeat.
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    #[test]
    fn cut_keeps_the_sessions() {
        let gap = crate::partitioned::GAP as u32;
        // The sessions of ascending times: each one's first time and how many it holds.
        let sessions = |times: &[u32]| -> Vec<(u32, usize)> {
            let mut found: Vec<(u32, usize)> = Vec::new();
            for (at, &time) in times.iter().enumerate() {
                if at == 0 || time - times[at - 1] > gap {
                    found.push((time, 1));
                } else {
                    found.last_mut().unwrap().1 += 1;
                }
            }
            found
        };
        let mut scratch = Scratch::default();
        let mut state = 0xD1B5_4A32_D192_ED03u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let step = ((1u64 << 32) / STEP) as u32;
        for n in [1usize, 2, 16, 17, 100, NET - 1, NET, NET + 1, NET + 2, 300, 383, 384, 385, 700, 1000, 5000, 70_000, 400_000] {
            for round in 0..if n < 6000 { 60 } else { 6 } {
                // Long gaps for about `long` of the times, short ones for the rest.
                let long = [3u64, 40, 400, 1200][round % 4];
                let mut time = 604_800_000 + (next() % 3_000_000) as u32;
                let mut times = Vec::with_capacity(n);
                for _ in 0..n {
                    times.push(time);
                    let ahead = if next() % n as u64 >= long || time > 2_000_000_000 {
                        [0, 0, 1, (next() % 2000) as u32][(next() % 4) as usize]
                    } else {
                        match next() % 12 {
                            0 => gap - 1,
                            1 => gap,
                            2 => gap + 1,
                            3 => step - 1,
                            4 => step,
                            5 => step + 1,
                            6 => 2 * step,
                            7 => 2 * gap,
                            8 => gap / 2,
                            _ => (next() % 4_000_000) as u32,
                        }
                    };
                    time += ahead;
                }
                for at in (1..n).rev() {
                    times.swap(at, (next() % (at as u64 + 1)) as usize);
                }
                let mut sorted = times.clone();
                sorted.sort_unstable();
                let want = sessions(&sorted);
                cut(&mut times, &mut scratch);
                assert!(times.windows(2).all(|pair| pair[0] <= pair[1]), "{n} times, round {round}: not ascending");
                assert_eq!(sessions(&times), want, "{n} times, round {round}");
            }
        }
    }
}
