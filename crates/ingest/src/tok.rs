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
    pub(super) unsafe fn masks(p: *const u8) -> [u64; 3] {
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
    pub(super) unsafe fn masks(p: *const u8) -> [u64; 3] {
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

#[cfg(all(feature = "tok-shape", any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64")))]
pub(crate) use shape::Shape;
#[cfg(all(feature = "tok-shape", not(any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64"))))]
pub(crate) use Scalar as Shape;

/// Slot `tok-shape`: the event and click lines read by their shape instead of by position
/// lists. An event line's first three fields (`event_id`, `user_id`, `ts`) end within 64 bytes
/// of its first byte whenever they are well-formed and generated, and the other four within 64
/// bytes of the third comma, so a line is two 64-bit windows of the comma, newline and quote
/// masks: the separators come out with a few bit operations, and the quote rules are mask
/// algebra (every quote at a field's edge, an even count before every separator), with no
/// byte loaded to look for a quote. A click line is one window. A line that does not fit this
/// shape (a wrong field count, a misplaced quote, a field over 60 bytes, a chunk's last line)
/// is handed, alone, to `Scalar`, so the fields are those of `Scalar` for any input.
///
/// One core, 20M events: 320 to 340 ms for the event files against about 515 ms for `Mask`
/// (`x86.md`). Several lines in turn, to overlap one line's wait for its newline, measured no
/// gain: a line is work, not waiting.
#[cfg(all(feature = "tok-shape", any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64")))]
mod shape {
    use memchr::memchr;

    use super::mask::masks;
    use super::{Field, Scalar, Scratch, Tok};

    pub(crate) struct Shape;

    impl Tok for Shape {
        unsafe fn lines<const N: usize>(p: *const u8, len: usize, s: &mut Scratch, mut row: impl FnMut(&[Field; N])) {
            // SAFETY: the caller's contract is each loop's; a `[Field; N]` is a `[Field; 7]`
            // or a `[Field; 4]` in the arm that says so.
            unsafe {
                match N {
                    7 => events(p, len, s, |fields| row(&*(fields as *const [Field; 7]).cast::<[Field; N]>())),
                    4 => clicks(p, len, s, |fields| row(&*(fields as *const [Field; 4]).cast::<[Field; N]>())),
                    _ => super::Mask::lines::<N>(p, len, s, row),
                }
            }
        }
    }

    /// The lowest set bit of `bits` as a mask, and `bits` without it.
    #[inline(always)]
    fn take(bits: u64) -> (u64, u64) {
        let lowest = bits & bits.wrapping_neg();
        (lowest, bits ^ lowest)
    }

    /// Whether an odd number of quotes precedes any of the separators `stops`, or the end of
    /// the stretch `quotes` covers: a field with one quote, or three.
    #[inline(always)]
    fn odd<const K: usize>(quotes: u64, stops: [u64; K]) -> bool {
        let mut counts = quotes.count_ones();
        for stop in stops {
            counts |= (quotes & stop.wrapping_sub(1)).count_ones();
        }
        counts & 1 != 0
    }

    /// One line through `Scalar`: the line at `at`, to its newline. Returns the next line's start.
    ///
    /// # Safety
    /// As `Tok::lines`, with `at < len`.
    #[cold]
    #[inline(never)]
    pub(super) unsafe fn general<const N: usize>(p: *const u8, len: usize, at: usize, s: &mut Scratch, row: &mut impl FnMut(&[Field; N])) -> usize {
        // SAFETY: `at..len` is inside the segment, which ends with a newline.
        let rest = unsafe { std::slice::from_raw_parts(p.add(at), len - at) };
        let line = memchr(b'\n', rest).map_or(rest.len(), |newline| newline + 1);
        unsafe { Scalar::lines::<N>(p.add(at), line, s, row) };
        at + line
    }

    /// # Safety
    /// As `Tok::lines`.
    #[inline(always)]
    unsafe fn events(p: *const u8, len: usize, s: &mut Scratch, mut row: impl FnMut(&[Field; 7])) {
        let mut at = 0;
        while at < len {
            // Both windows are read whole: 64 bytes at the line, 64 more from at most 64 on.
            if at + 128 <= len + crate::PAD {
                // SAFETY: 128 readable bytes from `at`, and the second window starts at most
                // 64 after it.
                let [commas, newlines, quotes] = unsafe { masks(p.add(at)) };
                let (c1, rest) = take(commas);
                let (c2, rest) = take(rest);
                let (c3, _) = take(rest);
                let before = c3.wrapping_sub(1);
                let qa = quotes & before;
                let fits_a = (c3 != 0)
                    & (newlines & before == 0)
                    & (qa & !(1 | (c1 | c2) << 1 | (c1 | c2 | c3) >> 1) == 0)
                    & !odd(qa, [c1, c2]);
                let (p1, p2, p3) = (c1.trailing_zeros() as usize, c2.trailing_zeros() as usize, c3.trailing_zeros() as usize & 63);
                let second = at + p3 + 1;
                let [commas, newlines, quotes] = unsafe { masks(p.add(second)) };
                let (c4, rest) = take(commas | newlines);
                let (c5, rest) = take(rest);
                let (c6, rest) = take(rest);
                let (end, _) = take(rest);
                let qb = quotes & end.wrapping_sub(1);
                let fits_b = (newlines & end != 0)
                    & (newlines & (c4 | c5 | c6) == 0)
                    & (qb & !(1 | (c4 | c5 | c6) << 1 | (c4 | c5 | c6 | end) >> 1) == 0)
                    & !odd(qb, [c4, c5, c6]);
                if fits_a & fits_b {
                    let (p4, p5, p6, p7) =
                        (c4.trailing_zeros() as usize, c5.trailing_zeros() as usize, c6.trailing_zeros() as usize, end.trailing_zeros() as usize);
                    // A quote at a field's first byte: the field is wrapped (its last byte is
                    // the other quote, by the checks above).
                    let wrapped_a = |start: usize| (qa >> start) as usize & 1;
                    let wrapped_b = |start: usize| (qb >> start) as usize & 1;
                    let (w0, w1, w2) = (wrapped_a(0), wrapped_a(p1 + 1), wrapped_a(p2 + 1));
                    let (w3, w4, w5, w6) = (wrapped_b(0), wrapped_b(p4 + 1), wrapped_b(p5 + 1), wrapped_b(p6 + 1));
                    let (a, b) = (p.wrapping_add(at), p.wrapping_add(second));
                    let fields = [
                        Field { p: a.wrapping_add(w0), len: p1 - 2 * w0 },
                        Field { p: a.wrapping_add(p1 + 1 + w1), len: p2 - p1 - 1 - 2 * w1 },
                        Field { p: a.wrapping_add(p2 + 1 + w2), len: p3 - p2 - 1 - 2 * w2 },
                        Field { p: b.wrapping_add(w3), len: p4 - 2 * w3 },
                        Field { p: b.wrapping_add(p4 + 1 + w4), len: p5 - p4 - 1 - 2 * w4 },
                        Field { p: b.wrapping_add(p5 + 1 + w5), len: p6 - p5 - 1 - 2 * w5 },
                        Field { p: b.wrapping_add(p6 + 1 + w6), len: p7 - p6 - 1 - 2 * w6 },
                    ];
                    row(&fields);
                    at = second + p7 + 1;
                    continue;
                }
            }
            at = unsafe { general(p, len, at, s, &mut row) };
        }
    }

    /// # Safety
    /// As `Tok::lines`.
    #[inline(always)]
    unsafe fn clicks(p: *const u8, len: usize, s: &mut Scratch, mut row: impl FnMut(&[Field; 4])) {
        let mut at = 0;
        while at < len {
            // SAFETY: `at < len`, and `PAD` (64) bytes after the segment are readable.
            let [commas, newlines, quotes] = unsafe { masks(p.add(at)) };
            let (c1, rest) = take(commas | newlines);
            let (c2, rest) = take(rest);
            let (c3, rest) = take(rest);
            let (end, _) = take(rest);
            let q = quotes & end.wrapping_sub(1);
            let fits = (newlines & end != 0)
                & (newlines & (c1 | c2 | c3) == 0)
                & (q & !(1 | (c1 | c2 | c3) << 1 | (c1 | c2 | c3 | end) >> 1) == 0)
                & !odd(q, [c1, c2, c3]);
            if fits {
                let (p1, p2, p3, p4) =
                    (c1.trailing_zeros() as usize, c2.trailing_zeros() as usize, c3.trailing_zeros() as usize, end.trailing_zeros() as usize);
                let wrapped = |start: usize| (q >> start) as usize & 1;
                let (w0, w1, w2, w3) = (wrapped(0), wrapped(p1 + 1), wrapped(p2 + 1), wrapped(p3 + 1));
                let a = p.wrapping_add(at);
                let fields = [
                    Field { p: a.wrapping_add(w0), len: p1 - 2 * w0 },
                    Field { p: a.wrapping_add(p1 + 1 + w1), len: p2 - p1 - 1 - 2 * w1 },
                    Field { p: a.wrapping_add(p2 + 1 + w2), len: p3 - p2 - 1 - 2 * w2 },
                    Field { p: a.wrapping_add(p3 + 1 + w3), len: p4 - p3 - 1 - 2 * w3 },
                ];
                row(&fields);
                at += p4 + 1;
            } else {
                at = unsafe { general(p, len, at, s, &mut row) };
            }
        }
    }
}

#[cfg(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2"))]
pub(crate) use fused::Fused;

/// The tokenizer for what one thread reads before the workers start (the lookup files, the
/// key search's sample): the shapes where the build has them, `Scalar` elsewhere. Every
/// tokenizer gives the same fields; at 20M `Scalar` took about 2 ms of serial time here.
#[cfg(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2"))]
pub(crate) type Quick = fused::Fused;
#[cfg(not(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2")))]
pub(crate) type Quick = Scalar;

/// Slot `pos-fused`: a line's shape as numbers (where each field starts and how long it is),
/// for a caller that reads the fields' bytes itself (`pos::wide::fused`) instead of taking
/// `Field`s from a callback. The windows are `Shape`'s: an event line's first three fields
/// within 64 bytes, the other four within the 64 after the third comma; a click line in 64.
/// A window's three masks are one 64-byte compare each (AVX-512), and the quote rules are two
/// bit extractions: the quote bits at the fields' first bytes and at their last bytes must be
/// the same fields' (`wrapped`). `None` for any line that is not of this shape, which then
/// goes to `Scalar`; so the fields are `Scalar`'s for any input (`tests::both`).
#[cfg(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2"))]
pub(crate) mod fused {
    use core::arch::x86_64::*;

    use super::shape::general;
    use super::{Field, Scratch, Tok};

    /// Bytes `event_shape` may read from the line's first byte; `click_shape` reads 64.
    pub(crate) const EVENT_WINDOW: usize = 128;

    /// A well-formed line of `N` fields: each field's first byte (from the line's) and length,
    /// wrapping quotes stripped, and the line's length with its newline.
    #[derive(Clone, Copy)]
    pub(crate) struct Line<const N: usize> {
        pub start: [usize; N],
        pub len: [usize; N],
        #[cfg_attr(not(test), allow(dead_code))]
        pub next: usize,
    }

    impl<const N: usize> Line<N> {
        #[inline(always)]
        pub(crate) fn fields(&self, line: *const u8) -> [Field; N] {
            std::array::from_fn(|field| Field { p: line.wrapping_add(self.start[field]), len: self.len[field] })
        }
    }

    /// The `,`, `\n` and `"` bits of the 64 bytes at `p`.
    #[inline(always)]
    pub(crate) unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        // SAFETY: the caller guarantees 64 readable bytes; AVX-512BW is compiled in (cfg above).
        unsafe {
            let bytes = _mm512_loadu_si512(p.cast());
            let bits = |byte: u8| _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(byte as i8));
            (bits(b','), bits(b'\n'), bits(b'"'))
        }
    }

    /// For fields that start at bit 0 and end at the separators `stops`, and the quote bits
    /// below the last of them: which fields are wrapped (bit i for field i), and whether every
    /// quote is one of a wrapping pair.
    ///
    /// `starts` and `ends` are the fields' first and last bytes, in field order, so the two
    /// extractions line up field by field: a field has a quote at its first byte exactly when
    /// it has one at its last. With no quote anywhere else, and none on a byte that is both (a
    /// field that is one quote), each field has no quote or is wrapped in one pair. The first
    /// field must not be empty, or its last byte would be before bit 0 and the extractions
    /// would not line up; the caller leaves such a line to `Scalar`.
    #[inline(always)]
    fn wrapped(quotes: u64, stops: u64) -> (u64, bool) {
        let (starts, ends) = (stops << 1 | 1, stops >> 1);
        // SAFETY: BMI2 is compiled in (cfg above).
        #[allow(unused_unsafe)]
        let (first, last) = unsafe { (_pext_u64(quotes, starts), _pext_u64(quotes, ends)) };
        (first, (stops & 1 == 0) & (quotes & !(starts | ends) == 0) & (quotes & starts & ends == 0) & (first == last))
    }

    /// The lowest set bit's position, and `bits` without it.
    #[inline(always)]
    fn next(bits: u64) -> (usize, u64) {
        (bits.trailing_zeros() as usize, bits & bits.wrapping_sub(1))
    }

    /// The event line at `line`, when its first three fields end within 64 bytes and the
    /// other four within the next 64, the first and the fourth are not empty and the quotes
    /// are wrapping pairs.
    ///
    /// # Safety
    /// `EVENT_WINDOW` bytes readable at `line`.
    #[inline(always)]
    pub(crate) unsafe fn event_shape(line: *const u8) -> Option<Line<7>> {
        // SAFETY: 64 bytes at `line`, then 64 from at most 64 further.
        let (commas, newlines, quotes) = unsafe { masks(line) };
        let (p1, rest) = next(commas);
        let (p2, rest) = next(rest);
        let third = rest & rest.wrapping_neg();
        let p3 = rest.trailing_zeros() as usize & 63;
        let before = third.wrapping_sub(1);
        let (wa, pairs_a) = wrapped(quotes & before, commas & (before | third));
        // Each window leaves by its own branch: with both checks in one block the compiler
        // pairs the two windows' bit operations in vector registers, about 40 instructions
        // of moves a line.
        if !((third != 0) & (newlines & before == 0) & pairs_a) {
            return None;
        }

        let second = p3 + 1;
        let (commas, newlines, quotes) = unsafe { masks(line.add(second)) };
        let stops = commas | newlines;
        let (p4, rest) = next(stops);
        let (p5, rest) = next(rest);
        let (p6, rest) = next(rest);
        let end = rest & rest.wrapping_neg();
        let p7 = rest.trailing_zeros() as usize;
        let before = end.wrapping_sub(1);
        let (wb, pairs_b) = wrapped(quotes & before, stops & (before | end));
        if !((newlines & end != 0) & (newlines & before == 0) & pairs_b) {
            return None;
        }
        let q = |bits: u64, field: u32| (bits >> field & 1) as usize;
        let (q0, q1, q2) = (q(wa, 0), q(wa, 1), q(wa, 2));
        let (q3, q4, q5, q6) = (q(wb, 0), q(wb, 1), q(wb, 2), q(wb, 3));
        Some(Line {
            start: [q0, p1 + 1 + q1, p2 + 1 + q2, second + q3, second + p4 + 1 + q4, second + p5 + 1 + q5, second + p6 + 1 + q6],
            len: [p1 - 2 * q0, p2 - p1 - 1 - 2 * q1, p3 - p2 - 1 - 2 * q2, p4 - 2 * q3, p5 - p4 - 1 - 2 * q4, p6 - p5 - 1 - 2 * q5, p7 - p6 - 1 - 2 * q6],
            next: second + p7 + 1,
        })
    }

    /// The click line at `line`, when it ends within 64 bytes, its first field is not empty
    /// and the quotes are wrapping pairs.
    ///
    /// # Safety
    /// 64 bytes readable at `line`.
    #[inline(always)]
    pub(crate) unsafe fn click_shape(line: *const u8) -> Option<Line<4>> {
        let (commas, newlines, quotes) = unsafe { masks(line) };
        let stops = commas | newlines;
        let (p1, rest) = next(stops);
        let (p2, rest) = next(rest);
        let (p3, rest) = next(rest);
        let end = rest & rest.wrapping_neg();
        let p4 = rest.trailing_zeros() as usize;
        let before = end.wrapping_sub(1);
        let (w, pairs) = wrapped(quotes & before, stops & (before | end));
        if !((newlines & end != 0) & (newlines & before == 0) & pairs) {
            return None;
        }
        let q = |field: u32| (w >> field & 1) as usize;
        let (q0, q1, q2, q3) = (q(0), q(1), q(2), q(3));
        Some(Line {
            start: [q0, p1 + 1 + q1, p2 + 1 + q2, p3 + 1 + q3],
            len: [p1 - 2 * q0, p2 - p1 - 1 - 2 * q1, p3 - p2 - 1 - 2 * q2, p4 - p3 - 1 - 2 * q3],
            next: p4 + 1,
        })
    }

    /// The line of two fields at `line` (a links line), when it ends within 64 bytes, its
    /// first field is not empty and the quotes are wrapping pairs.
    ///
    /// # Safety
    /// 64 bytes readable at `line`.
    #[inline(always)]
    pub(crate) unsafe fn pair_shape(line: *const u8) -> Option<Line<2>> {
        let (commas, newlines, quotes) = unsafe { masks(line) };
        let stops = commas | newlines;
        let (p1, rest) = next(stops);
        let end = rest & rest.wrapping_neg();
        let p2 = rest.trailing_zeros() as usize;
        let before = end.wrapping_sub(1);
        let (w, pairs) = wrapped(quotes & before, stops & (before | end));
        if !((newlines & end != 0) & (newlines & before == 0) & pairs) {
            return None;
        }
        let (q0, q1) = ((w & 1) as usize, (w >> 1 & 1) as usize);
        Some(Line { start: [q0, p1 + 1 + q1], len: [p1 - 2 * q0, p2 - p1 - 1 - 2 * q1], next: p2 + 1 })
    }

    /// The shapes as a tokenizer: for the links (`pos::Ingest::pos_links`), and for the tests
    /// to compare every shape with `Scalar`.
    pub(crate) struct Fused;

    impl Tok for Fused {
        unsafe fn lines<const N: usize>(p: *const u8, len: usize, s: &mut Scratch, mut row: impl FnMut(&[Field; N])) {
            let mut at = 0;
            while at < len {
                // SAFETY: the windows are readable where the shapes are asked for, and a
                // `[Field; N]` is the array of the arm's size in the arm that says so.
                let next = unsafe {
                    let line = p.add(at);
                    match N {
                        7 if at + EVENT_WINDOW <= len + crate::PAD => event_shape(line).map(|shape| {
                            row(&*(&shape.fields(line) as *const [Field; 7]).cast::<[Field; N]>());
                            shape.next
                        }),
                        4 => click_shape(line).map(|shape| {
                            row(&*(&shape.fields(line) as *const [Field; 4]).cast::<[Field; N]>());
                            shape.next
                        }),
                        2 => pair_shape(line).map(|shape| {
                            row(&*(&shape.fields(line) as *const [Field; 2]).cast::<[Field; N]>());
                            shape.next
                        }),
                        _ => None,
                    }
                };
                at = match next {
                    Some(next) => at + next,
                    None => unsafe { general(p, len, at, s, &mut row) },
                };
            }
        }
    }
}
