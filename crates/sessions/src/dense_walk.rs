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

use core::arch::x86_64::*;

use types::record::{PAY_CATEGORY_SHIFT, PAY_PRICE_MASK};
use types::rows::{NO_CATEGORY, UserDaily};
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
/// # Safety
/// `ends` and `click_ends` hold `span` ascending positions inside `ts` and `clicks`; `fits`.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn walk(
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
    out: &mut UserDaily,
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
        sort_times::sort(&mut ts[from..end], sorter);
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
        let (add, magic) = (_mm512_set1_epi32(in_day as i32), _mm512_set1_epi64(6_515_625));
        let behind = _mm512_set1_epi32((i64::from(ctx.day0) - origin_day) as i32);
        let organic = _mm512_set1_epi32(ctx.organic() as i32);
        let mut day_before = _mm512_set1_epi32(-1);
        let (mut nrow, mut s) = (0usize, 0usize);
        while s < ns {
            let live: __mmask16 = if ns - s >= 16 { 0xFFFF } else { (1u16 << (ns - s)) - 1 };
            let x = _mm512_srli_epi32::<10>(_mm512_add_epi32(_mm512_loadu_si512(s_ts.as_ptr().add(s).cast()), add));
            let even = _mm512_srli_epi64::<39>(_mm512_mul_epu32(x, magic));
            let odd = _mm512_srli_epi64::<39>(_mm512_mul_epu32(_mm512_srli_epi64::<32>(x), magic));
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
        wide.person(&clicks[from_click..end_click], &s_ts[s0..s1], &s_day[s0..s1], &mut s_camp[s0..s1], counters.wide());
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
            unsafe { _mm512_mask_cmple_epu32_mask(live, _mm512_loadu_si512(s_ts.as_ptr().add(s0).cast()), _mm512_set1_epi32(at as i32)) }.count_ones() as usize
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

    // The rows: five columns copied 16 rows a step, then each person's name over its rows.
    out.reserve(nrow + 32);
    let at = out.person.len();
    debug_assert!([out.day.len(), out.sessions.len(), out.events.len(), out.revenue_cents.len(), out.top_category.len()].iter().all(|&len| len == at));
    // SAFETY: room for `nrow + 32` rows in every column; a step writes at most 15 rows past
    // the last and reads as far past it in the rows' columns, which have that room; a
    // person's rows are at most 16 (a row a day), and its name is stored over 16 of them
    // from its first, the persons in order, so each row keeps its own person's.
    unsafe {
        let (person, day, sessions, events, revenue, category) =
            (out.person.as_mut_ptr().add(at), out.day.as_mut_ptr().add(at), out.sessions.as_mut_ptr().add(at), out.events.as_mut_ptr().add(at), out.revenue_cents.as_mut_ptr().add(at), out.top_category.as_mut_ptr().add(at));
        let day0 = _mm512_set1_epi32(ctx.day0);
        let mut row = 0;
        while row < nrow {
            _mm512_storeu_si512(day.add(row).cast(), _mm512_add_epi32(_mm512_loadu_si512(row_day.as_ptr().add(row).cast()), day0));
            let counts = _mm512_sub_epi32(_mm512_loadu_si512(row_s0.as_ptr().add(row + 1).cast()), _mm512_loadu_si512(row_s0.as_ptr().add(row).cast()));
            _mm512_storeu_si512(sessions.add(row).cast(), _mm512_cvtepu32_epi64(_mm512_castsi512_si256(counts)));
            _mm512_storeu_si512(sessions.add(row + 8).cast(), _mm512_cvtepu32_epi64(_mm512_extracti64x4_epi64::<1>(counts)));
            let counts = _mm512_sub_epi32(_mm512_loadu_si512(row_e0.as_ptr().add(row + 1).cast()), _mm512_loadu_si512(row_e0.as_ptr().add(row).cast()));
            _mm512_storeu_si512(events.add(row).cast(), _mm512_cvtepu32_epi64(_mm512_castsi512_si256(counts)));
            _mm512_storeu_si512(events.add(row + 8).cast(), _mm512_cvtepu32_epi64(_mm512_extracti64x4_epi64::<1>(counts)));
            _mm512_storeu_si512(revenue.add(row).cast(), _mm512_loadu_si512(row_rev.as_ptr().add(row).cast()));
            _mm512_storeu_si512(revenue.add(row + 8).cast(), _mm512_loadu_si512(row_rev.as_ptr().add(row + 8).cast()));
            _mm_storeu_si128(category.add(row).cast(), _mm_loadu_si128(row_cat.as_ptr().add(row).cast()));
            row += 16;
        }
        for &p in people.iter() {
            let name = _mm512_set1_epi64((Id::from(base) + Id::from(p)) as i64);
            let rows = person.add(*s_row.get_unchecked(rank(stretch(p as usize).0)) as usize);
            _mm512_storeu_si512(rows.cast(), name);
            _mm512_storeu_si512(rows.add(8).cast(), name);
        }
        out.person.set_len(at + nrow);
        out.day.set_len(at + nrow);
        out.sessions.set_len(at + nrow);
        out.events.set_len(at + nrow);
        out.revenue_cents.set_len(at + nrow);
        out.top_category.set_len(at + nrow);
    }
}
