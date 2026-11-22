//! One person's event times in ascending order, with no branch that depends on the times.
//!
//! `sort_unstable` decides by comparing: on a person's thirty times, in no order, about every
//! other decision is mispredicted, and the bucket stage's sorts were a tenth of a run's branch
//! misses. Here:
//!
//! - up to 16 times are one register through a sorting network: ten layers, each a permute,
//!   a minimum, a maximum and a blend;
//! - up to `RADIX` times are leaves of 16 sorted that way and merged, pair by pair, with the
//!   smaller of two taken by arithmetic, not by a jump;
//! - more are sorted by their bytes, four counting passes (a pass whose byte is the same in
//!   every time is skipped).
//!
//! Without AVX-512 F at build time this is `sort_unstable`.
//!
//! `sort_narrow` is the same in 256-bit registers (`dense_walk` says why): up to 8 times are
//! one register through a network of six layers, up to 16 two registers, each sorted so and
//! then merged by four layers more.

/// From this many times on, by bytes.
const RADIX: usize = 384;

/// Buffers, one set per worker.
#[derive(Default)]
pub(crate) struct Scratch {
    spare: Vec<u32>,
    counts: Vec<[u32; 256]>,
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
pub(crate) fn sort(times: &mut [u32], _scratch: &mut Scratch) {
    times.sort_unstable();
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl")))]
pub(crate) fn sort_narrow(times: &mut [u32], scratch: &mut Scratch) {
    sort(times, scratch)
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
pub(crate) fn sort(times: &mut [u32], scratch: &mut Scratch) {
    sort_as::<false>(times, scratch)
}

/// `sort` with no register wider than 256 bits.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
pub(crate) fn sort_narrow(times: &mut [u32], scratch: &mut Scratch) {
    sort_as::<true>(times, scratch)
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
fn sort_as<const NARROW: bool>(times: &mut [u32], scratch: &mut Scratch) {
    let n = times.len();
    if n <= 16 {
        if n >= 2 {
            // SAFETY: `n` lanes at `times`.
            unsafe { leaf_as::<NARROW>(times.as_mut_ptr(), n) };
        }
    } else if n < RADIX {
        merged::<NARROW>(times, &mut scratch.spare);
    } else {
        by_bytes(times, scratch);
    }
}

/// `leaf`, or `leaf_narrow` for `NARROW`.
///
/// # Safety
/// As `leaf`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
unsafe fn leaf_as<const NARROW: bool>(at: *mut u32, n: usize) {
    // SAFETY: the caller's contract.
    unsafe {
        #[cfg(target_feature = "avx512vl")]
        if NARROW {
            return leaf_narrow(at, n);
        }
        leaf(at, n)
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

/// One layer of a sorting network on 8 lanes: each lane against its `partner`, the lanes of
/// `LARGER` keeping the larger of the two.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
#[inline(always)]
fn layer<const LARGER: i32>(v: core::arch::x86_64::__m256i, partner: core::arch::x86_64::__m256i) -> core::arch::x86_64::__m256i {
    use core::arch::x86_64::*;
    // SAFETY: AVX2 is compiled in.
    unsafe { _mm256_blend_epi32::<LARGER>(_mm256_min_epu32(v, partner), _mm256_max_epu32(v, partner)) }
}

/// Each lane's partner in a layer of 8: lane `i ^ 1`, `i ^ 2`, `i ^ 3`, `i ^ 4` or `i ^ 7`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
mod partner {
    use core::arch::x86_64::*;

    #[inline(always)]
    pub(super) fn one(v: __m256i) -> __m256i {
        // SAFETY: AVX2 is compiled in.
        unsafe { _mm256_shuffle_epi32::<0b10_11_00_01>(v) }
    }

    #[inline(always)]
    pub(super) fn two(v: __m256i) -> __m256i {
        // SAFETY: as above.
        unsafe { _mm256_shuffle_epi32::<0b01_00_11_10>(v) }
    }

    #[inline(always)]
    pub(super) fn three(v: __m256i) -> __m256i {
        // SAFETY: as above.
        unsafe { _mm256_shuffle_epi32::<0b00_01_10_11>(v) }
    }

    #[inline(always)]
    pub(super) fn four(v: __m256i) -> __m256i {
        // SAFETY: as above.
        unsafe { _mm256_permute4x64_epi64::<0b01_00_11_10>(v) }
    }

    #[inline(always)]
    pub(super) fn seven(v: __m256i) -> __m256i {
        // SAFETY: as above.
        unsafe { _mm256_permutevar8x32_epi32(v, _mm256_setr_epi32(7, 6, 5, 4, 3, 2, 1, 0)) }
    }
}

/// 8 lanes sorted: the bitonic sorter's first six layers, as `network` lays them out for 16.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
#[inline(always)]
fn sort8(v: core::arch::x86_64::__m256i) -> core::arch::x86_64::__m256i {
    let v = layer::<0b1010_1010>(v, partner::one(v));
    let v = layer::<0b1100_1100>(v, partner::three(v));
    let v = layer::<0b1010_1010>(v, partner::one(v));
    clean8(layer::<0b1111_0000>(v, partner::seven(v)))
}

/// 8 lanes in a bitonic order sorted: each lane against the one 4, 2 and 1 lanes away.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
#[inline(always)]
fn clean8(v: core::arch::x86_64::__m256i) -> core::arch::x86_64::__m256i {
    let v = layer::<0b1111_0000>(v, partner::four(v));
    let v = layer::<0b1100_1100>(v, partner::two(v));
    layer::<0b1010_1010>(v, partner::one(v))
}

/// `leaf` in 256-bit registers. Up to 8 times are one register through `sort8`. Up to 16 are
/// two, each through `sort8`, then lane `i` of the first against lane `7 - i` of the second
/// (the sorter's layer of 16): the 8 smaller and the 8 larger are then each in a bitonic
/// order, and `clean8` sorts each.
///
/// # Safety
/// As `leaf`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
#[inline(always)]
unsafe fn leaf_narrow(at: *mut u32, n: usize) {
    use core::arch::x86_64::*;
    // SAFETY: masked loads and stores touch the `n` live lanes only; the rest is registers.
    unsafe {
        // The lanes past the times hold the largest value, and stay behind them.
        let max = _mm256_set1_epi32(-1);
        if n <= 8 {
            let live = ((1u32 << n) - 1) as __mmask8;
            let v = sort8(_mm256_mask_loadu_epi32(max, live, at.cast()));
            _mm256_mask_storeu_epi32(at.cast(), live, v);
        } else {
            let live = ((1u32 << (n - 8)) - 1) as __mmask8;
            let low = sort8(_mm256_loadu_si256(at.cast()));
            let high = partner::seven(sort8(_mm256_mask_loadu_epi32(max, live, at.add(8).cast())));
            let (small, large) = (clean8(_mm256_min_epu32(low, high)), clean8(_mm256_max_epu32(low, high)));
            _mm256_storeu_si256(at.cast(), small);
            _mm256_mask_storeu_epi32(at.add(8).cast(), live, large);
        }
    }
}

/// 17 to `RADIX` times: leaves of 16, then merges of twice the width each round.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
fn merged<const NARROW: bool>(times: &mut [u32], spare: &mut Vec<u32>) {
    let n = times.len();
    // SAFETY: a leaf is at most 16 lanes inside `times`.
    unsafe {
        let mut at = 0;
        while at < n {
            leaf_as::<NARROW>(times.as_mut_ptr().add(at), (n - at).min(16));
            at += 16;
        }
    }
    spare.clear();
    spare.reserve(n);
    // SAFETY: `spare` has room for `n` values, and every round writes all `n` of its target
    // before the next reads them.
    unsafe {
        spare.set_len(n);
        let (mut from, mut to) = (times.as_mut_ptr(), spare.as_mut_ptr());
        let mut width = 16;
        while width < n {
            let mut at = 0;
            while at < n {
                let (middle, end) = ((at + width).min(n), (at + 2 * width).min(n));
                merge(from.add(at), middle - at, from.add(middle), end - middle, to.add(at));
                at = end;
            }
            (from, to) = (to, from);
            width *= 2;
        }
        if from != times.as_mut_ptr() {
            std::ptr::copy_nonoverlapping(from, times.as_mut_ptr(), n);
        }
    }
}

/// `a` then `b`, each ascending, merged into `out`.
///
/// # Safety
/// `na` values at `a`, `nb` at `b`, room for `na + nb` at `out`, which overlaps neither.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
unsafe fn merge(a: *const u32, na: usize, b: *const u32, nb: usize, out: *mut u32) {
    // SAFETY: the caller's contract; `i < na` and `j < nb` while both are read.
    unsafe {
        let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
        while i < na && j < nb {
            let (x, y) = (*a.add(i), *b.add(j));
            // The smaller, and the step in its run, by arithmetic.
            let first = usize::from(x <= y);
            *out.add(k) = y ^ ((x ^ y) & (first as u32).wrapping_neg());
            i += first;
            j += 1 - first;
            k += 1;
        }
        std::ptr::copy_nonoverlapping(a.add(i), out.add(k), na - i);
        std::ptr::copy_nonoverlapping(b.add(j), out.add(k + na - i), nb - j);
    }
}

/// `RADIX` times or more: by their four bytes, the lowest first.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
fn by_bytes(times: &mut [u32], scratch: &mut Scratch) {
    let n = times.len();
    let Scratch { spare, counts } = scratch;
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

    /// Every length around the three ways' bounds, times spread wide and times packed close
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
        let lengths = (0..=70).chain([127, 128, 129, 255, 256, 257, RADIX - 1, RADIX, RADIX + 1, 1000, 5000, 70_000]);
        for n in lengths {
            for spread in [1u64 << 31, 1 << 20, 300, 3] {
                for _ in 0..if n < 300 { 40 } else { 3 } {
                    let base = (next() % (1 << 30)) as u32;
                    let mut times: Vec<u32> = (0..n).map(|_| base + (next() % spread) as u32).collect();
                    let mut want = times.clone();
                    want.sort_unstable();
                    let mut narrow = times.clone();
                    sort(&mut times, &mut scratch);
                    assert_eq!(times, want, "{n} times over {spread}");
                    sort_narrow(&mut narrow, &mut scratch);
                    assert_eq!(narrow, want, "{n} times over {spread}, narrow");
                }
            }
        }
    }
}
