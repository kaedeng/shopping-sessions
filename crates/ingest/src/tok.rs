//! Tokenizer slot: lines to quote-stripped fields (rules "CSV dialect"). `tok-mask` is the pick,
//! `tok-scalar` the portable baseline, the fallback on other targets, and the tests' oracle.

/// One field of a well-formed line, its wrapping quotes already stripped.
#[derive(Clone, Copy)]
pub(crate) struct Field {
    pub p: *const u8,
    pub len: usize,
}

pub(crate) trait Tok {
    /// Calls `row` with the `N` fields of every well-formed line of the `len` bytes at `p`:
    /// exactly `N` fields, each free of quotes or wrapped in one pair. Others are dropped.
    ///
    /// # Safety
    /// The bytes are whole lines (the last byte is `\n`), and the byte before `p` and
    /// `crate::PAD` bytes after the end are readable.
    unsafe fn lines<const N: usize>(p: *const u8, len: usize, scratch: &mut Scratch, row: impl FnMut(&[Field; N]));
}

/// Bytes `Mask` indexes before it parses them, so a batch is still in L1 for its second pass.
const BATCH: usize = 8 << 10;

/// A worker's index buffers for `Mask`, reused across chunks; `Scalar` needs none.
#[cfg_attr(not(feature = "tok-mask"), allow(dead_code))]
pub(crate) struct Scratch {
    pub batch: usize,
    commas: Vec<u32>,
    newlines: Vec<u32>,
    blocks: Vec<Block>,
}

/// One 64-byte block's comma and quote bits, and how many of each come before it in the batch.
#[derive(Clone, Copy, Default)]
#[cfg_attr(not(feature = "tok-mask"), allow(dead_code))]
struct Block {
    commas: u64,
    quotes: u64,
    commas_before: u32,
    quotes_before: u32,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch { batch: BATCH, commas: Vec::new(), newlines: Vec::new(), blocks: Vec::new() }
    }
}

pub(crate) struct Scalar;

impl Tok for Scalar {
    unsafe fn lines<const N: usize>(p: *const u8, len: usize, _: &mut Scratch, mut row: impl FnMut(&[Field; N])) {
        let mut fields = [Field { p, len: 0 }; N];
        let (mut count, mut start, mut quotes, mut ok) = (0, 0, 0, true);
        for at in 0..len {
            // SAFETY: `at < len`; `start <= at`.
            let byte = unsafe { *p.add(at) };
            quotes += usize::from(byte == b'"');
            if byte == b',' || byte == b'\n' {
                let size = at - start;
                let wrapped = size >= 2 && unsafe { *p.add(start) == b'"' && *p.add(at - 1) == b'"' };
                ok &= quotes == 0 || (wrapped && quotes == 2);
                if count < N {
                    let strip = usize::from(wrapped);
                    fields[count] = Field { p: p.wrapping_add(start + strip), len: size - 2 * strip };
                }
                (count, start, quotes) = (count + 1, at + 1, 0);
                if byte == b'\n' {
                    if ok && count == N {
                        row(&fields);
                    }
                    (count, ok) = (0, true);
                }
            }
        }
    }
}

#[cfg(all(feature = "tok-mask", any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64")))]
pub(crate) use mask::Mask;
#[cfg(all(feature = "tok-mask", not(any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64"))))]
pub(crate) use Scalar as Mask;

/// Each batch in two passes. Index: 64-byte blocks to comma, newline and quote bitmasks, the comma
/// and newline positions written out (simdjson's flattening). Rows: per line, its comma count and
/// quote count from the masks decide well-formedness with one compare each, and the fields come
/// from the positions at a fixed stride, so a well-formed line takes no data-dependent branch.
#[cfg(all(feature = "tok-mask", any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64")))]
mod mask {
    use super::{Block, Field, Scratch, Tok};

    pub(crate) struct Mask;

    impl Tok for Mask {
        unsafe fn lines<const N: usize>(p: *const u8, len: usize, s: &mut Scratch, mut row: impl FnMut(&[Field; N])) {
            let mut start = 0;
            while start < len {
                let mut size = s.batch;
                let lines = loop {
                    let end = len.min(start + size);
                    if s.commas.len() < end - start + 64 {
                        s.commas.resize(end - start + 64, 0);
                        s.newlines.resize(end - start + 64, 0);
                        s.blocks.resize((end - start) / 64 + 2, Block::default());
                    }
                    // SAFETY: `start..end` is inside the segment, and the buffers were sized for it.
                    let lines = unsafe { index(p.add(start), end - start, s) };
                    if lines > 0 {
                        break lines;
                    }
                    // A line longer than the batch, never generated: index again with room for it.
                    // shortcut: the scratch grows to ~9 bytes per byte of the longest line, and
                    // positions are u32; hand such a line to `Scalar` if multi-GB lines ever occur.
                    size *= 2;
                };
                // SAFETY: as above; the batch ends at its last `\n`, the rest is indexed again.
                unsafe { rows(p.add(start), s, lines, &mut row) };
                start += s.newlines[lines - 1] as usize + 1;
            }
        }
    }

    /// Fills `s` for the `len` bytes at `p`; returns the number of newlines.
    #[inline(always)]
    unsafe fn index(p: *const u8, len: usize, s: &mut Scratch) -> usize {
        let blocks = len.div_ceil(64);
        let (commas, newlines, out) = (s.commas.as_mut_ptr(), s.newlines.as_mut_ptr(), s.blocks.as_mut_ptr());
        let (mut n_commas, mut n_newlines, mut quotes_before) = (0, 0, 0);
        for b in 0..blocks {
            // SAFETY: a block starts inside the batch, and PAD bytes after it are readable.
            let [mut c, mut n, mut q] = unsafe { masks(p.add(64 * b)) };
            if b + 1 == blocks {
                let keep = !0u64 >> (64 * blocks - len);
                (c, n, q) = (c & keep, n & keep, q & keep);
            }
            // SAFETY: `blocks` holds `len / 64 + 2` and both position lists `len + 64`, while a
            // flatten writes at most 64 past the positions so far.
            unsafe {
                *out.add(b) = Block { commas: c, quotes: q, commas_before: n_commas as u32, quotes_before };
                flatten::<8>(c, 64 * b as u32, commas, &mut n_commas);
                flatten::<2>(n, 64 * b as u32, newlines, &mut n_newlines);
            }
            quotes_before += q.count_ones();
        }
        n_newlines
    }

    /// Appends the positions of the set bits. Always `W` stores, so a block with at most `W`
    /// takes no data-dependent branch; the extra stores land past the end and are overwritten.
    #[inline(always)]
    unsafe fn flatten<const W: usize>(mut bits: u64, base: u32, out: *mut u32, n: &mut usize) {
        let count = bits.count_ones() as usize;
        // SAFETY: the caller sized `out` for `*n + 64`.
        let at = unsafe { out.add(*n) };
        for i in 0..W {
            unsafe { *at.add(i) = base + bits.trailing_zeros() };
            bits &= bits.wrapping_sub(1);
        }
        if count > W {
            let mut i = W;
            while bits != 0 {
                unsafe { *at.add(i) = base + bits.trailing_zeros() };
                bits &= bits - 1;
                i += 1;
            }
        }
        *n += count;
    }

    #[inline(always)]
    unsafe fn rows<const N: usize>(p: *const u8, s: &Scratch, lines: usize, row: &mut impl FnMut(&[Field; N])) {
        let (mut start, mut commas0, mut quotes0) = (0, 0, 0);
        for &end in &s.newlines[..lines] {
            let end = end as usize;
            // SAFETY: `end < len`, and `index` wrote a block for every 64 bytes of the batch.
            let block = unsafe { s.blocks.get_unchecked(end / 64) };
            let below = (1u64 << (end % 64)) - 1;
            let commas1 = block.commas_before + (block.commas & below).count_ones();
            let quotes1 = block.quotes_before + (block.quotes & below).count_ones();
            if commas1 - commas0 == N as u32 - 1 {
                let mut fields = [Field { p, len: 0 }; N];
                let (mut from, mut wrapped) = (start, 0);
                for (j, field) in fields.iter_mut().enumerate() {
                    // SAFETY: the line holds exactly the commas `commas0..commas1`.
                    let to = if j + 1 < N { unsafe { *s.commas.get_unchecked(commas0 as usize + j) as usize } } else { end };
                    // SAFETY: `from <= to` are in the batch; `to - 1` is at worst the byte before it.
                    let (first, last) = unsafe { (*p.add(from), *p.wrapping_add(to).wrapping_sub(1)) };
                    let strip = usize::from((to - from >= 2) & (first == b'"') & (last == b'"'));
                    wrapped += strip;
                    *field = Field { p: p.wrapping_add(from + strip), len: to - from - 2 * strip };
                    from = to + 1;
                }
                // Quotes only around whole fields: every quote in the line is one of those pairs.
                if 2 * wrapped as u32 == quotes1 - quotes0 {
                    row(&fields);
                }
            }
            (start, commas0, quotes0) = (end + 1, commas1, quotes1);
        }
    }

    /// The `,`, `\n` and `"` bits of the 64 bytes at `p`.
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> [u64; 3] {
        use core::arch::aarch64::*;
        // SAFETY: the caller guarantees 64 readable bytes.
        unsafe {
            // De-interleaved, vector j holds bytes 4i + j, so packing a nibble per i keeps byte order.
            let v = vld4q_u8(p);
            let bits = |byte: u8| {
                let k = vdupq_n_u8(byte);
                let (m0, m1, m2, m3) = (vceqq_u8(v.0, k), vceqq_u8(v.1, k), vceqq_u8(v.2, k), vceqq_u8(v.3, k));
                let m = vsriq_n_u8::<2>(vsriq_n_u8::<1>(m3, m2), vsriq_n_u8::<1>(m1, m0));
                let m = vshrn_n_u16::<4>(vreinterpretq_u16_u8(vsriq_n_u8::<4>(m, m)));
                vget_lane_u64::<0>(vreinterpret_u64_u8(m))
            };
            [bits(b','), bits(b'\n'), bits(b'"')]
        }
    }

    /// The `,`, `\n` and `"` bits of the 64 bytes at `p`.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> [u64; 3] {
        use core::arch::x86_64::*;
        // SAFETY: the caller guarantees 64 readable bytes; AVX2 is compiled in (cfg above).
        unsafe {
            let (lo, hi) = (_mm256_loadu_si256(p.cast()), _mm256_loadu_si256(p.add(32).cast()));
            let bits = |byte: u8| {
                let k = _mm256_set1_epi8(byte as i8);
                let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, k)) as u32 as u64;
                let hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, k)) as u32 as u64;
                lo | hi << 32
            };
            [bits(b','), bits(b'\n'), bits(b'"')]
        }
    }
}
