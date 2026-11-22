//! The dense bucket's sessions, rows and counters as passes over the whole bucket, 16 entries a
//! step, where `partitioned::bucket_dense` walked person by person and session by session.
//!
//! Walked that way every session ended a loop, began or did not begin a row and had or had
//! not a purchase, each a branch the processor could not foresee: the walk and its rows were
//! an eighth of a run's branch misses. Here a person is met in three short loops only (its
//! times sorted, its clicks credited, its name written); everything else goes by position:
//!
//! - the times lie person after person, each person's ascending, with a bit set where a
//!   person's begin. A session begins at such a bit or after a gap: 16 times a step, the
//!   starts' times and positions packed where they are stored;
//! - a session's day is a multiply, a row begins where a person does or the day changes, and
//!   a row's counts are differences of positions;
//! - a purchase finds its session among its person's starts;
//! - the credited sessions are counted in one pass, and the rows' columns copied 16 a step.
//!
//! Needs `attr-wide` and its table of counts (a window of at most 16 days), and AVX-512 F, BW,
//! DQ and VL with BMI2 at build time; `bucket_dense` keeps its own walk for everything else.
//!
//! `walk::<true>` (`SS_SESS_WIDTH=256`) does the same steps of 16 in two halves of 8 lanes, with
//! no 512-bit register and no 256-bit multiply anywhere in the stage: Intel's server cores of
//! the scorer's generation (Skylake to Cascade Lake) run any 512-bit instruction, and 256-bit
//! multiplies, a step below their top clock, for both threads of the core and for about 650
//! us after the last one, and the stage spent three quarters of its cycles there. AVX-512 VL
//! on 256 bits (masks, compress, compares into masks) and 128-bit multiplies keep it.

use core::arch::x86_64::*;

use types::record::{PAY_CATEGORY_SHIFT, PAY_PRICE_MASK};
use types::rows::{EVENTS_HELD, NO_CATEGORY, SESSIONS_HELD, UserDailyNarrow};
use types::{Ctx, Id};

use crate::agg_dense::Counters;
use crate::partitioned::GAP;
use crate::{Click, attr_wide, sort_times};

/// In a session's position: the session is its person's first.
const RUN: u32 = 1 << 31;

/// The walk's buffers, one set per worker.
#[derive(Default)]
pub(crate) struct Walk {
    /// The bucket's persons with events, and those of them with clicks.
    people: Vec<u32>,
    clickers: Vec<u32>,
    /// A bit per position of the times: a person's first.
    first: Vec<u64>,
    /// Per 16 positions: the sessions before them, and which of them begin one.
    before: Vec<u32>,
    begins: Vec<u16>,
    /// Per session: its start, its first position (with `RUN`), its day, its row, its
    /// campaign, its revenue.
    s_ts: Vec<u32>,
    s_at: Vec<u32>,
    s_day: Vec<u32>,
    s_row: Vec<u32>,
    s_camp: Vec<u32>,
    s_rev: Vec<u64>,
    /// Per row: its day, its first session, its first position, its revenue, its category.
    row_day: Vec<u32>,
    row_s0: Vec<u32>,
    row_e0: Vec<u32>,
    row_rev: Vec<u64>,
    row_cat: Vec<u8>,
    /// The sessions with a purchase; per category the row it was last summed for, and the sum.
    paid: Vec<u32>,
    cat_row: Vec<u32>,
    cat_sum: Vec<u64>,
}

/// `_mm512_mul_epu32(x, by)` as two 256-bit multiplies, each written as the instruction it
/// is: given the intrinsic on the two halves, the compiler joins them into one 512-bit
/// multiply again. A 512-bit multiply is what Intel's server cores of the scorer's
/// generation (Skylake to Cascade Lake) answer by dropping to their lowest clock, the
/// "AVX-512 heavy" licence, for every instruction of both threads of the core and for
/// about a millisecond after the last one; a 256-bit multiply asks for the licence the line
/// kernel's own multiplies already hold.
#[inline(always)]
fn mul_halves(x: __m512i, by: __m256i) -> __m512i {
    let (low, high): (__m256i, __m256i);
    // SAFETY: two AVX2 instructions on registers; `low` is written before `b` and `by` are
    // read, so it is a register of its own (`out`, not `lateout`).
    unsafe {
        let (a, b) = (_mm512_castsi512_si256(x), _mm512_extracti64x4_epi64::<1>(x));
        core::arch::asm!("vpmuludq {l}, {a}, {m}", "vpmuludq {h}, {b}, {m}", a = in(ymm_reg) a, b = in(ymm_reg) b, m = in(ymm_reg) by, l = out(ymm_reg) low, h = lateout(ymm_reg) high, options(pure, nomem, nostack, preserves_flags));
        _mm512_inserti64x4::<1>(_mm512_castsi256_si512(low), high)
    }
}

/// `mul_halves` for 256 bits: `_mm256_mul_epu32(x, by)` as two 128-bit multiplies, written as
/// the instructions they are for the same reason. A 128-bit multiply keeps the top clock.
#[inline(always)]
fn mul_quarters(x: __m256i, by: __m128i) -> __m256i {
    let (low, high): (__m128i, __m128i);
    // SAFETY: two AVX instructions on registers; `low` is written before `b` and `by` are
    // read, so it is a register of its own (`out`, not `lateout`).
    unsafe {
        let (a, b) = (_mm256_castsi256_si128(x), _mm256_extracti128_si256::<1>(x));
        core::arch::asm!("vpmuludq {l}, {a}, {m}", "vpmuludq {h}, {b}, {m}", a = in(xmm_reg) a, b = in(xmm_reg) b, m = in(xmm_reg) by, l = out(xmm_reg) low, h = lateout(xmm_reg) high, options(pure, nomem, nostack, preserves_flags));
        _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(low), high)
    }
}

/// How many of a byte's bits are set at or below each bit: eight counts, a byte each.
static UP_TO: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut byte = 0;
    while byte < 256 {
        let (mut bit, mut count) = (0, 0u64);
        while bit < 8 {
            count += (byte >> bit & 1) as u64;
            table[byte] |= count << (8 * bit);
            bit += 1;
        }
        byte += 1;
    }
    table
};

fn grow<T: Clone + Default>(column: &mut Vec<T>, len: usize) {
    if column.len() < len {
        column.resize(len, T::default());
    }
}

/// Lists the rows `which` names of the 16 from `row` on, each with its count: the rows a
/// narrow column does not hold (`UserDailyNarrow::wide_events`), one in a thousand.
#[cold]
#[inline(never)]
fn wide_rows(row: usize, mut which: __mmask16, counts: __m512i, list: &mut Vec<(u32, u32)>) {
    let mut held = [0u32; 16];
    // SAFETY: 16 lanes into 16 entries.
    unsafe { _mm512_storeu_si512(held.as_mut_ptr().cast(), counts) };
    while which != 0 {
        let lane = which.trailing_zeros() as usize;
        list.push(((row + lane) as u32, held[lane]));
        which &= which - 1;
    }
}

/// `wide_rows` for the 8 rows from `row` on, their counts in a 256-bit register: the
/// narrow walk's (`walk::<true>`), which makes no 512-bit register.
#[cold]
#[inline(never)]
fn wide_rows_half(row: usize, mut which: __mmask8, counts: __m256i, list: &mut Vec<(u32, u32)>) {
    let mut held = [0u32; 8];
    // SAFETY: 8 lanes into 8 entries.
    unsafe { _mm256_storeu_si256(held.as_mut_ptr().cast(), counts) };
    while which != 0 {
        let lane = which.trailing_zeros() as usize;
        list.push(((row + lane) as u32, held[lane]));
        which &= which - 1;
    }
}

/// Whether the walk can run for this window: a `ts` and the origin's time of day must add up
/// in 32 bits.
pub(crate) fn fits(ctx: &Ctx) -> bool {
    u64::from(ctx.ts_end) + ctx.day_split().1 < 1 << 32
}

/// The bucket's persons through their sessions: the times of person `p` (of `span`) are
/// `ts[ends[p - 1]..ends[p]]` in any order, its clicks `clicks[click_ends[p - 1]..click_ends[p]]`,
/// and `side` holds the purchases as `(p << 32 | ts, pay)`, ascending, then one with `p` at
/// or past `span`.
///
/// `NARROW`: in registers of 256 bits and fewer, with the kernels' narrow forms
/// (`sort_times::sort_narrow`, `attr_wide::Attr::person_narrow`).
///
/// # Safety
/// `ends` and `click_ends` hold `span` ascending positions inside `ts` and `clicks`; `fits`.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(crate) unsafe fn walk<const NARROW: bool>(
    ctx: &Ctx,
    base: u32,
    span: usize,
    ends: &[u32],
    click_ends: &[u32],
    ts: &mut [u32],
    clicks: &mut [Click],
    side: &[(u64, u32)],
    w: &mut Walk,
    sorter: &mut sort_times::Scratch,
    wide: &mut attr_wide::Attr,
    counters: &mut Counters,
    out: &mut UserDailyNarrow,
) {
    let m = ends[span - 1] as usize;
    if m == 0 {
        return;
    }
    let Walk { people, clickers, first, before, begins, s_ts, s_at, s_day, s_row, s_camp, s_rev, row_day, row_s0, row_e0, row_rev, row_cat, paid, cat_row, cat_sum } = w;
    // Where person `p`'s times begin and end.
    let stretch = |p: usize| (if p == 0 { 0 } else { ends[p - 1] as usize }, ends[p] as usize);

    // The persons with events, listed without a branch: each is stored, and kept if it has
    // any. Then each one's times in order, and a bit at its first.
    people.clear();
    people.reserve(span + 1);
    clickers.clear();
    clickers.reserve(span + 1);
    first.clear();
    first.resize(m / 64 + 2, 0);
    // SAFETY: both lists have room for `span` entries and one is stored for each person.
    unsafe {
        let (list, mut count, mut from) = (people.as_mut_ptr(), 0usize, 0u32);
        let (clicking, mut clicked, mut from_click) = (clickers.as_mut_ptr(), 0usize, 0u32);
        for (p, (&end, &end_click)) in ends[..span].iter().zip(&click_ends[..span]).enumerate() {
            *list.add(count) = p as u32;
            *clicking.add(clicked) = p as u32;
            count += usize::from(end != from);
            clicked += usize::from((end != from) & (end_click != from_click));
            (from, from_click) = (end, end_click);
        }
        people.set_len(count);
        clickers.set_len(clicked);
    }
    for &p in people.iter() {
        let (from, end) = stretch(p as usize);
        if NARROW {
            sort_times::sort_narrow(&mut ts[from..end], sorter);
        } else {
            sort_times::sort(&mut ts[from..end], sorter);
        }
        first[from >> 6] |= 1 << (from & 63);
    }

    let blocks = m.div_ceil(16);
    grow(before, blocks + 2);
    grow(begins, blocks + 2);
    for column in [&mut *s_ts, &mut *s_at, &mut *s_day, &mut *s_row, &mut *s_camp, &mut *row_day, &mut *row_s0, &mut *row_e0] {
        grow(column, m + 48);
    }
    grow(s_rev, m + 48);
    grow(row_rev, m + 48);
    grow(row_cat, m + 48);

    // SAFETY: a step loads the live lanes of 16 positions inside `ts` and stores at most 16
    // entries from a count that is at most the positions so far, inside the room made
    // above; the bit map holds a bit for every position.
    let (ns, nrow) = unsafe {
        if NARROW {
            cut_narrow(ctx, m, blocks, ts, first, before, begins, s_ts, s_at, s_day, s_row, s_camp, row_day, row_s0, row_e0)
        } else {
            cut(ctx, m, blocks, ts, first, before, begins, s_ts, s_at, s_day, s_row, s_camp, row_day, row_s0, row_e0)
        }
    };
    (row_s0[nrow], row_e0[nrow], s_row[ns]) = (ns as u32, m as u32, nrow as u32);
    s_rev[..ns].fill(0);
    row_rev[..nrow].fill(0);
    row_cat[..nrow].fill(NO_CATEGORY);
    // The sessions before position `at`.
    let rank = |at: usize| (before[at >> 4] + (u32::from(begins[at >> 4]) & ((1u32 << (at & 15)) - 1)).count_ones()) as usize;

    // The credit of the persons with clicks.
    for &p in clickers.iter() {
        let p = p as usize;
        let (from_click, end_click) = (if p == 0 { 0 } else { click_ends[p - 1] as usize }, click_ends[p] as usize);
        let (from, end) = stretch(p);
        let (s0, s1) = (rank(from), rank(end));
        if NARROW {
            wide.person_narrow(&clicks[from_click..end_click], &s_ts[s0..s1], &s_day[s0..s1], &mut s_camp[s0..s1], counters.wide());
        } else {
            wide.person(&clicks[from_click..end_click], &s_ts[s0..s1], &s_day[s0..s1], &mut s_camp[s0..s1], counters.wide());
        }
    }

    // The purchases, each to its session (the last of its person's that starts at or before
    // it) and its row. They come person after person and in time order, so row after row.
    paid.clear();
    paid.reserve(side.len() + 1);
    cat_row.clear();
    cat_row.resize(256, u32::MAX);
    grow(cat_sum, 256);
    let (mut row_now, mut best, mut paying) = (u32::MAX, 0u64, 0usize);
    for &(key, pay) in side {
        let p = (key >> 32) as usize;
        if p >= span {
            break;
        }
        let (from, end) = stretch(p);
        let (s0, s1) = (rank(from), rank(end));
        let at = key as u32;
        // SAFETY: the sessions' columns have 16 entries of room past any session.
        let started = if s1 - s0 <= 16 {
            let live = ((1u32 << (s1 - s0)) - 1) as __mmask16;
            if NARROW {
                // SAFETY: as above, 8 entries a half.
                unsafe {
                    let at = _mm256_set1_epi32(at as i32);
                    let low = _mm256_mask_cmple_epu32_mask(live as __mmask8, _mm256_loadu_si256(s_ts.as_ptr().add(s0).cast()), at);
                    let high = _mm256_mask_cmple_epu32_mask((live >> 8) as __mmask8, _mm256_loadu_si256(s_ts.as_ptr().add(s0 + 8).cast()), at);
                    (low.count_ones() + high.count_ones()) as usize
                }
            } else {
                unsafe { _mm512_mask_cmple_epu32_mask(live, _mm512_loadu_si512(s_ts.as_ptr().add(s0).cast()), _mm512_set1_epi32(at as i32)) }.count_ones() as usize
            }
        } else {
            s_ts[s0..s1].partition_point(|&start| start <= at)
        };
        if started == 0 {
            // No event of this person at or before it: not a purchase of a surviving event.
            continue;
        }
        let session = s0 + started - 1;
        let (price, category) = (u64::from(pay & PAY_PRICE_MASK), (pay >> PAY_CATEGORY_SHIFT) as usize);
        let row = s_row[session];
        // SAFETY: room for an entry per purchase and one more.
        unsafe { *paid.as_mut_ptr().add(paying) = session as u32 };
        paying += usize::from(s_rev[session] == 0);
        s_rev[session] += price;
        row_rev[row as usize] += price;
        // The row's top category so far: the largest sum, the smallest index among equals.
        let sum = if cat_row[category] == row { cat_sum[category] } else { 0 } + price;
        (cat_row[category], cat_sum[category]) = (row, sum);
        if row != row_now {
            (row_now, best) = (row, 0);
        }
        let weight = sum << 8 | (255 - category as u64);
        if weight > best {
            best = weight;
            row_cat[row as usize] = category as u8;
        }
    }
    // SAFETY: `paying` entries were written.
    unsafe { paid.set_len(paying) };

    // Every session is one more in its campaign's count for its day; revenue is added for
    // the few sessions that have any.
    {
        let counted = counters.counted();
        debug_assert!(counted.len() >= ctx.campaigns.len() * 16);
        for s in 0..ns {
            // SAFETY: a campaign is below the campaigns' count, a day below 16.
            unsafe { *counted.get_unchecked_mut(*s_camp.get_unchecked(s) as usize * 16 + *s_day.get_unchecked(s) as usize) += 1 };
        }
    }
    for &session in paid.iter() {
        let session = session as usize;
        counters.paid(s_camp[session], s_day[session] as usize, s_rev[session]);
    }

    // The rows, 16 a step: the day and the two counts narrowed where they are stored (a
    // byte, a byte, two bytes: `UserDailyNarrow`), the revenue and the category copied. Then
    // each person once, with its first row.
    out.reserve(nrow + 32, people.len());
    let (at, runs) = (out.day.len(), out.person.len());
    debug_assert!([out.sessions.len(), out.events.len(), out.revenue_cents.len(), out.top_category.len()].iter().all(|&len| len == at));
    debug_assert_eq!(out.first.len(), runs);
    // SAFETY: room for `nrow + 32` rows in every column; a step writes at most 15 rows past
    // the last and reads as far past it in the rows' columns, which have that room; and
    // room for an entry per person of the bucket in the persons' two.
    unsafe {
        let (day, sessions, events, revenue, category) = (out.day.as_mut_ptr().add(at), out.sessions.as_mut_ptr().add(at), out.events.as_mut_ptr().add(at), out.revenue_cents.as_mut_ptr().add(at), out.top_category.as_mut_ptr().add(at));
        if NARROW {
            // The same steps of 16 rows in two halves of 8.
            let (sessions_held, events_held) = (_mm256_set1_epi32(SESSIONS_HELD as i32), _mm256_set1_epi32(EVENTS_HELD as i32));
            let mut row = 0;
            while row < nrow {
                let live: u16 = if nrow - row >= 16 { 0xFFFF } else { (1u16 << (nrow - row)) - 1 };
                for half in 0..2 {
                    let (row, live) = (row + 8 * half, (live >> (8 * half)) as __mmask8);
                    _mm_storel_epi64(day.add(row).cast(), _mm256_cvtepi32_epi8(_mm256_loadu_si256(row_day.as_ptr().add(row).cast())));
                    let in_row = _mm256_sub_epi32(_mm256_loadu_si256(row_s0.as_ptr().add(row + 1).cast()), _mm256_loadu_si256(row_s0.as_ptr().add(row).cast()));
                    _mm_storel_epi64(sessions.add(row).cast(), _mm256_cvtusepi32_epi8(in_row));
                    let counted = _mm256_sub_epi32(_mm256_loadu_si256(row_e0.as_ptr().add(row + 1).cast()), _mm256_loadu_si256(row_e0.as_ptr().add(row).cast()));
                    _mm_storeu_si128(events.add(row).cast(), _mm256_cvtusepi32_epi16(counted));
                    // The rows a narrow column does not hold: listed with their counts.
                    let (many, long) = (_mm256_mask_cmpge_epu32_mask(live, in_row, sessions_held), _mm256_mask_cmpge_epu32_mask(live, counted, events_held));
                    if many | long != 0 {
                        wide_rows_half(at + row, many, in_row, &mut out.wide_sessions);
                        wide_rows_half(at + row, long, counted, &mut out.wide_events);
                    }
                    _mm256_storeu_si256(revenue.add(row).cast(), _mm256_loadu_si256(row_rev.as_ptr().add(row).cast()));
                    _mm256_storeu_si256(revenue.add(row + 4).cast(), _mm256_loadu_si256(row_rev.as_ptr().add(row + 4).cast()));
                }
                _mm_storeu_si128(category.add(row).cast(), _mm_loadu_si128(row_cat.as_ptr().add(row).cast()));
                row += 16;
            }
        } else {
            let (sessions_held, events_held) = (_mm512_set1_epi32(SESSIONS_HELD as i32), _mm512_set1_epi32(EVENTS_HELD as i32));
            let mut row = 0;
            while row < nrow {
                let live: __mmask16 = if nrow - row >= 16 { 0xFFFF } else { (1u16 << (nrow - row)) - 1 };
                _mm_storeu_si128(day.add(row).cast(), _mm512_cvtepi32_epi8(_mm512_loadu_si512(row_day.as_ptr().add(row).cast())));
                let in_row = _mm512_sub_epi32(_mm512_loadu_si512(row_s0.as_ptr().add(row + 1).cast()), _mm512_loadu_si512(row_s0.as_ptr().add(row).cast()));
                _mm_storeu_si128(sessions.add(row).cast(), _mm512_cvtusepi32_epi8(in_row));
                let counted = _mm512_sub_epi32(_mm512_loadu_si512(row_e0.as_ptr().add(row + 1).cast()), _mm512_loadu_si512(row_e0.as_ptr().add(row).cast()));
                _mm256_storeu_si256(events.add(row).cast(), _mm512_cvtusepi32_epi16(counted));
                // The rows a narrow column does not hold: listed with their counts.
                let (many, long) = (_mm512_mask_cmpge_epu32_mask(live, in_row, sessions_held), _mm512_mask_cmpge_epu32_mask(live, counted, events_held));
                if many | long != 0 {
                    wide_rows(at + row, many, in_row, &mut out.wide_sessions);
                    wide_rows(at + row, long, counted, &mut out.wide_events);
                }
                _mm512_storeu_si512(revenue.add(row).cast(), _mm512_loadu_si512(row_rev.as_ptr().add(row).cast()));
                _mm512_storeu_si512(revenue.add(row + 8).cast(), _mm512_loadu_si512(row_rev.as_ptr().add(row + 8).cast()));
                _mm_storeu_si128(category.add(row).cast(), _mm_loadu_si128(row_cat.as_ptr().add(row).cast()));
                row += 16;
            }
        }
        let (person, first) = (out.person.as_mut_ptr().add(runs), out.first.as_mut_ptr().add(runs));
        for (run, &p) in people.iter().enumerate() {
            *person.add(run) = Id::from(base) + Id::from(p);
            *first.add(run) = (at + *s_row.get_unchecked(rank(stretch(p as usize).0)) as usize) as u32;
        }
        out.person.set_len(runs + people.len());
        out.first.set_len(runs + people.len());
        out.day.set_len(at + nrow);
        out.sessions.set_len(at + nrow);
        out.events.set_len(at + nrow);
        out.revenue_cents.set_len(at + nrow);
        out.top_category.set_len(at + nrow);
    }
}

/// The sessions and the rows, 16 entries a step (`walk`'s first two passes): the sessions'
/// starts, first positions, days, rows and campaigns (all `organic`), the rows' days, first
/// sessions and first positions, the counts per 16 positions; returns the sessions and the
/// rows.
///
/// # Safety
/// As `walk`, the columns grown as it grows them.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
unsafe fn cut(
    ctx: &Ctx,
    m: usize,
    blocks: usize,
    ts: &[u32],
    first: &[u64],
    before: &mut [u32],
    begins: &mut [u16],
    s_ts: &mut [u32],
    s_at: &mut [u32],
    s_day: &mut [u32],
    s_row: &mut [u32],
    s_camp: &mut [u32],
    row_day: &mut [u32],
    row_s0: &mut [u32],
    row_e0: &mut [u32],
) -> (usize, usize) {
    // SAFETY: the caller's contract.
    unsafe {
        // The sessions: where a person begins, or after a gap.
        let (gap, lanes, run) = (_mm512_set1_epi32(GAP as i32), _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15), _mm512_set1_epi32(RUN as i32));
        let mut last = _mm512_setzero_si512();
        let mut ns = 0usize;
        for block in 0..blocks {
            let at = 16 * block;
            let live: __mmask16 = if m - at >= 16 { 0xFFFF } else { (1u16 << (m - at)) - 1 };
            let times = _mm512_maskz_loadu_epi32(live, ts.as_ptr().add(at).cast());
            // Each time beside the one before it (the last of the step before in lane 0).
            let gone = _mm512_sub_epi32(times, _mm512_alignr_epi32::<15>(times, last));
            let persons = (*first.get_unchecked(at >> 6) >> (at & 63)) as u16;
            let start = (_mm512_cmpgt_epu32_mask(gone, gap) | persons) & live;
            let position = _mm512_add_epi32(_mm512_set1_epi32(at as i32), lanes);
            _mm512_storeu_si512(s_at.as_mut_ptr().add(ns).cast(), _mm512_maskz_compress_epi32(start, _mm512_mask_or_epi32(position, persons, position, run)));
            _mm512_storeu_si512(s_ts.as_mut_ptr().add(ns).cast(), _mm512_maskz_compress_epi32(start, times));
            *before.get_unchecked_mut(block) = ns as u32;
            *begins.get_unchecked_mut(block) = start;
            ns += start.count_ones() as usize;
            last = times;
        }
        // Past the last step: no session begins, and a session's count ends at `m`.
        for block in blocks..blocks + 2 {
            (before[block], begins[block]) = (ns as u32, 0);
        }
        s_at[ns] = m as u32 | RUN;

        // Each session's day, and the rows: a person's sessions of one day.
        let (origin_day, in_day) = ctx.day_split();
        // A day is 2^10 * 84375 ms, and for x below 2^22, x / 84375 is x * 6515625 >> 39.
        // The two multiplies a half at a time (`mul_halves`).
        let (add, magic) = (_mm512_set1_epi32(in_day as i32), _mm256_set1_epi64x(6_515_625));
        let behind = _mm512_set1_epi32((i64::from(ctx.day0) - origin_day) as i32);
        let organic = _mm512_set1_epi32(ctx.organic() as i32);
        let mut day_before = _mm512_set1_epi32(-1);
        let (mut nrow, mut s) = (0usize, 0usize);
        while s < ns {
            let live: __mmask16 = if ns - s >= 16 { 0xFFFF } else { (1u16 << (ns - s)) - 1 };
            let x = _mm512_srli_epi32::<10>(_mm512_add_epi32(_mm512_loadu_si512(s_ts.as_ptr().add(s).cast()), add));
            let even = _mm512_srli_epi64::<39>(mul_halves(x, magic));
            let odd = _mm512_srli_epi64::<39>(mul_halves(_mm512_srli_epi64::<32>(x), magic));
            let day = _mm512_sub_epi32(_mm512_or_si512(even, _mm512_slli_epi64::<32>(odd)), behind);
            _mm512_storeu_si512(s_day.as_mut_ptr().add(s).cast(), day);
            let position = _mm512_loadu_si512(s_at.as_ptr().add(s).cast());
            // A row starts with a person (`RUN` is the sign bit) and where the day changes.
            let rows = (_mm512_movepi32_mask(position) | _mm512_cmpneq_epi32_mask(day, _mm512_alignr_epi32::<15>(day, day_before))) & live;
            _mm512_storeu_si512(row_day.as_mut_ptr().add(nrow).cast(), _mm512_maskz_compress_epi32(rows, day));
            _mm512_storeu_si512(row_s0.as_mut_ptr().add(nrow).cast(), _mm512_maskz_compress_epi32(rows, _mm512_add_epi32(_mm512_set1_epi32(s as i32), lanes)));
            _mm512_storeu_si512(row_e0.as_mut_ptr().add(nrow).cast(), _mm512_maskz_compress_epi32(rows, _mm512_andnot_si512(run, position)));
            // A session's row: the rows before the step, and the starts up to its lane.
            let (low, high) = (rows as u8, (rows >> 8) as u8);
            let up_to = _mm512_cvtepu8_epi32(_mm_set_epi64x(UP_TO[high as usize] as i64, UP_TO[low as usize] as i64));
            let row = _mm512_add_epi32(_mm512_set1_epi32(nrow as i32 - 1), up_to);
            _mm512_storeu_si512(s_row.as_mut_ptr().add(s).cast(), _mm512_mask_add_epi32(row, 0xFF00, row, _mm512_set1_epi32(low.count_ones() as i32)));
            _mm512_storeu_si512(s_camp.as_mut_ptr().add(s).cast(), organic);
            nrow += rows.count_ones() as usize;
            day_before = day;
            s += 16;
        }
        (ns, nrow)
    }
}

/// `cut` in halves of 8 lanes, and with 128-bit multiplies for the days: the same columns,
/// the same counts per 16 positions.
///
/// # Safety
/// As `cut`.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
unsafe fn cut_narrow(
    ctx: &Ctx,
    m: usize,
    blocks: usize,
    ts: &[u32],
    first: &[u64],
    before: &mut [u32],
    begins: &mut [u16],
    s_ts: &mut [u32],
    s_at: &mut [u32],
    s_day: &mut [u32],
    s_row: &mut [u32],
    s_camp: &mut [u32],
    row_day: &mut [u32],
    row_s0: &mut [u32],
    row_e0: &mut [u32],
) -> (usize, usize) {
    // SAFETY: the caller's contract; a half past the last position loads no lane, and its
    // pointer is formed by `wrapping_add`.
    unsafe {
        // The sessions: where a person begins, or after a gap.
        let (gap, lanes, run) = (_mm256_set1_epi32(GAP as i32), _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7), _mm256_set1_epi32(RUN as i32));
        let mut last = _mm256_setzero_si256();
        let mut ns = 0usize;
        for block in 0..blocks {
            let at = 16 * block;
            let live: u16 = if m - at >= 16 { 0xFFFF } else { (1u16 << (m - at)) - 1 };
            let persons = (*first.get_unchecked(at >> 6) >> (at & 63)) as u16;
            *before.get_unchecked_mut(block) = ns as u32;
            let mut start = 0u16;
            for half in 0..2 {
                let (at, live, persons) = (at + 8 * half, (live >> (8 * half)) as __mmask8, (persons >> (8 * half)) as __mmask8);
                let times = _mm256_maskz_loadu_epi32(live, ts.as_ptr().wrapping_add(at).cast());
                // Each time beside the one before it (the last of the half before in lane 0).
                let gone = _mm256_sub_epi32(times, _mm256_alignr_epi32::<7>(times, last));
                let begin = (_mm256_cmpgt_epu32_mask(gone, gap) | persons) & live;
                let position = _mm256_add_epi32(_mm256_set1_epi32(at as i32), lanes);
                _mm256_storeu_si256(s_at.as_mut_ptr().add(ns).cast(), _mm256_maskz_compress_epi32(begin, _mm256_mask_or_epi32(position, persons, position, run)));
                _mm256_storeu_si256(s_ts.as_mut_ptr().add(ns).cast(), _mm256_maskz_compress_epi32(begin, times));
                ns += begin.count_ones() as usize;
                start |= u16::from(begin) << (8 * half);
                last = times;
            }
            *begins.get_unchecked_mut(block) = start;
        }
        // Past the last step: no session begins, and a session's count ends at `m`.
        for block in blocks..blocks + 2 {
            (before[block], begins[block]) = (ns as u32, 0);
        }
        s_at[ns] = m as u32 | RUN;

        // Each session's day, and the rows: a person's sessions of one day. The day as in
        // `cut`, the multiplies a quarter at a time (`mul_quarters`).
        let (origin_day, in_day) = ctx.day_split();
        let (add, magic) = (_mm256_set1_epi32(in_day as i32), _mm_set1_epi64x(6_515_625));
        let behind = _mm256_set1_epi32((i64::from(ctx.day0) - origin_day) as i32);
        let organic = _mm256_set1_epi32(ctx.organic() as i32);
        let mut day_before = _mm256_set1_epi32(-1);
        let (mut nrow, mut s) = (0usize, 0usize);
        while s < ns {
            let live: u16 = if ns - s >= 16 { 0xFFFF } else { (1u16 << (ns - s)) - 1 };
            for half in 0..2 {
                let (s, live) = (s + 8 * half, (live >> (8 * half)) as __mmask8);
                let x = _mm256_srli_epi32::<10>(_mm256_add_epi32(_mm256_loadu_si256(s_ts.as_ptr().add(s).cast()), add));
                let even = _mm256_srli_epi64::<39>(mul_quarters(x, magic));
                let odd = _mm256_srli_epi64::<39>(mul_quarters(_mm256_srli_epi64::<32>(x), magic));
                let day = _mm256_sub_epi32(_mm256_or_si256(even, _mm256_slli_epi64::<32>(odd)), behind);
                _mm256_storeu_si256(s_day.as_mut_ptr().add(s).cast(), day);
                let position = _mm256_loadu_si256(s_at.as_ptr().add(s).cast());
                // A row starts with a person (`RUN` is the sign bit) and where the day changes.
                let rows = (_mm256_movepi32_mask(position) | _mm256_cmpneq_epi32_mask(day, _mm256_alignr_epi32::<7>(day, day_before))) & live;
                _mm256_storeu_si256(row_day.as_mut_ptr().add(nrow).cast(), _mm256_maskz_compress_epi32(rows, day));
                _mm256_storeu_si256(row_s0.as_mut_ptr().add(nrow).cast(), _mm256_maskz_compress_epi32(rows, _mm256_add_epi32(_mm256_set1_epi32(s as i32), lanes)));
                _mm256_storeu_si256(row_e0.as_mut_ptr().add(nrow).cast(), _mm256_maskz_compress_epi32(rows, _mm256_andnot_si256(run, position)));
                // A session's row: the rows before the half, and the starts up to its lane.
                let up_to = _mm256_cvtepu8_epi32(_mm_cvtsi64_si128(UP_TO[rows as usize] as i64));
                _mm256_storeu_si256(s_row.as_mut_ptr().add(s).cast(), _mm256_add_epi32(_mm256_set1_epi32(nrow as i32 - 1), up_to));
                _mm256_storeu_si256(s_camp.as_mut_ptr().add(s).cast(), organic);
                nrow += rows.count_ones() as usize;
                day_before = day;
            }
            s += 16;
        }
        (ns, nrow)
    }
}
