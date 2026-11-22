//! Slot `attr-wide`: one person's credit and assists in 16-lane registers, for the positional
//! pipeline's fast lane. The answers are `attr-interval`'s (`tests` compares the two on random
//! persons); the work is of another order:
//!
//! - A person with at most 16 clicks, which is nearly everyone: the clicks sit in two
//!   registers, unsorted. A session is one compare against all of them (which are in its
//!   week), the latest of those is its credit, and the campaigns among them, each once
//!   (`vpconflictd` marks a campaign's later clicks, once a person), are its assists: their
//!   counter cells are stored together and counted after. No sort, no state between sessions.
//! - A person with more: the clicks in time order, and each click covers a stretch of the
//!   sessions, from the first that starts at or after it to the last within a week. Both ends
//!   only move forward. A campaign assists the union of its clicks' stretches, so each click
//!   adds the part of its stretch past the end of the campaign's last one; how many of those
//!   sessions fall on each day is a difference of two prefix sums (a count per day up to each
//!   session, 16 days a register), added to the campaign's 16 counters at once. A session's
//!   credit is the last click whose stretch holds it: each click fills its stretch up to the
//!   next click's.
//!
//! `attr-interval` walks a window of clicks per session and keeps, per campaign in it, a count
//! and a place in a list; at 20M events that was half of the stages after the parse.
//!
//! The assists are counted in a table of their own, `[campaign * DAYS + day]`, so that a
//! campaign's days are one register (`agg_dense::Counters::wide`). Needs at most `DAYS` days.
//!
//! `person_narrow` gives the same answers in registers of 256 bits (`dense_walk` says why):
//! the clicks of a person with at most 8 are one register, with at most 16 two; a campaign's
//! 16 days are two.

use core::arch::x86_64::*;

use types::time::WEEK;

use crate::Click;

/// Days a campaign's assists are counted for at once.
pub(crate) const DAYS: usize = 16;
/// Clicks the first kernel takes.
const LIGHT: usize = 16;

#[derive(Default)]
pub(crate) struct Attr {
    /// Per campaign: `base` plus the end of the last stretch of sessions it assisted, for
    /// the person in hand. A value below `base` is an earlier person's: nothing yet.
    last: Vec<u32>,
    base: u32,
    /// Per session of the person in hand, and one more: the sessions before it on each day.
    prefix: Vec<[u32; DAYS]>,
    /// The counter cells to add one to.
    cells: Vec<u32>,
    /// A long run of clicks in order: `ts << 32 | position`, then the two columns.
    keys: Vec<u64>,
    ts: Vec<u32>,
    campaign: Vec<u32>,
    /// `heavy` sorts the clicks (`SS_ATTR_HEAVY=sort`); `heavy_table` does not (the pick), and
    /// `flat` is its form without jumps on the data (`SS_ATTR_HEAVY=flat`).
    sorted: bool,
    flat: bool,
    /// `heavy_table`: the person's starts with `u32::MAX` after them, and how many starts lie
    /// before each step of 2^`STEP` ms.
    st: Vec<u32>,
    table: Vec<u16>,
    /// Per session, and one more: the latest click whose stretch begins there.
    best_ts: Vec<u32>,
    best_at: Vec<u32>,
    /// Per campaign, `words` words: the sessions it assists, a bit each. All zero between
    /// persons.
    cmask: Vec<u64>,
    words: usize,
    /// Per campaign: `stamp` once it is in `touched` for the person in hand.
    seen: Vec<u32>,
    stamp: u32,
    touched: Vec<u32>,
}

/// A step of `Attr::table`, as a shift of milliseconds: 17.5 minutes, under the 30 that part
/// two sessions' starts.
const STEP: u32 = 20;
/// The same for `heavy_table::<_, true>`: 70 minutes, which hold at most three starts.
const FLAT_STEP: u32 = 22;
/// No click yet, in `Attr::best_at`.
const NO_CLICK: u32 = u32::MAX;

impl Attr {
    pub(crate) fn new(campaigns: usize) -> Attr {
        let heavy = types::env::pick("SS_ATTR_HEAVY", &["table", "sort", "flat"]);
        Attr { last: vec![0; campaigns], seen: vec![0; campaigns], sorted: heavy == 1, flat: heavy == 2, ..Attr::default() }
    }

    /// Credits one person's sessions and counts their assists. Session `i` starts at
    /// `starts[i]` on day `days[i]` (below `DAYS`), in time order; `credits[i]` is set to its
    /// campaign when a click credits it and left alone otherwise (the caller has `organic`
    /// there). `clicks` are the person's, in any order, with campaigns below the count given
    /// to `new`. `assisted` is `[campaign * DAYS + day]`.
    #[inline]
    pub(crate) fn person(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
        debug_assert!(starts.len() == days.len() && starts.len() == credits.len());
        if clicks.is_empty() || starts.is_empty() {
            return;
        }
        if clicks.len() <= LIGHT {
            self.light(clicks, starts, days, credits, assisted)
        } else if self.sorted || starts.len() >= usize::from(u16::MAX) {
            self.heavy::<false>(clicks, starts, days, credits, assisted)
        } else if self.flat {
            self.heavy_table::<false, true>(clicks, starts, days, credits, assisted)
        } else {
            self.heavy_table::<false, false>(clicks, starts, days, credits, assisted)
        }
    }

    /// `person` with no register wider than 256 bits.
    #[inline]
    pub(crate) fn person_narrow(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
        debug_assert!(starts.len() == days.len() && starts.len() == credits.len());
        if clicks.is_empty() || starts.is_empty() {
            return;
        }
        if clicks.len() <= LIGHT {
            self.light_narrow(clicks, starts, days, credits, assisted)
        } else if self.sorted || starts.len() >= usize::from(u16::MAX) {
            self.heavy::<true>(clicks, starts, days, credits, assisted)
        } else if self.flat {
            self.heavy_table::<true, true>(clicks, starts, days, credits, assisted)
        } else {
            self.heavy_table::<true, false>(clicks, starts, days, credits, assisted)
        }
    }

    /// `person` for any number of clicks, in any order: nothing is sorted.
    ///
    /// A click covers the sessions `lo..hi`: from the first that starts at or after it to the
    /// first that starts more than a week after it. Both are counts of starts, read from a
    /// table of the starts by steps of 2^`STEP` ms: two sessions start more than 30 minutes
    /// apart, so a step holds at most one start and a count is one entry and one compare (more
    /// starts in a step would only cost more compares). With at most 16 sessions the counts
    /// are two compares against all the starts instead.
    ///
    /// - The credit of session `s` is the latest click with `lo <= s`, if it is within the
    ///   week: the latest click per `lo` (clicks of one `ts` share theirs, and the smallest
    ///   `click_id` wins), then the latest so far, session by session.
    /// - A campaign assists the union of its clicks' stretches: a bit per session, and each
    ///   run of bits is a difference of two prefix sums, as in `heavy`.
    ///
    /// `NARROW`: a campaign's days in two 256-bit registers, the starts in two.
    ///
    /// `FLAT`: no jump on the data where the steps above took one, about one a session and
    /// one a click. The table is of steps of 2^`FLAT_STEP` ms from the first start's (a step
    /// holds at most three starts, a count is three compares), made as a count per step and
    /// the sums of the counts, where each start filled its stretch of the table; and the
    /// latest click of a `lo` and so far are selects (the compiler made both jumps).
    fn heavy_table<const NARROW: bool, const FLAT: bool>(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
        let (n, sessions) = (clicks.len(), starts.len());
        let week = WEEK as u32;
        // Room for a 16-lane load, and two entries past the last start.
        self.st.clear();
        self.st.extend_from_slice(starts);
        self.st.resize(sessions.max(LIGHT) + 2, u32::MAX);
        if self.prefix.len() < sessions + 1 {
            self.prefix.resize(sessions + 1, [0; DAYS]);
        }
        self.best_ts.clear();
        self.best_ts.resize(sessions + 1, 0);
        self.best_at.clear();
        self.best_at.resize(sessions + 1, NO_CLICK);
        let words = sessions.div_ceil(64);
        if words > self.words {
            self.words = words;
            self.cmask.clear();
            self.cmask.resize(self.seen.len() * words, 0);
        }
        if self.stamp == u32::MAX {
            self.seen.fill(0);
            self.stamp = 0;
        }
        self.stamp += 1;
        self.touched.clear();
        self.touched.reserve(n + 1);
        // The table: `table[g]` starts lie in the steps before step `g`; `cap` is the step
        // after the last start's, and stands for every later one.
        let wide = sessions > LIGHT;
        let (mut cap, mut first_step) = (0usize, 0u32);
        if wide && FLAT {
            // `table[g]` starts lie in the steps before step `first_step + g`.
            first_step = starts[0] >> FLAT_STEP;
            cap = ((starts[sessions - 1] >> FLAT_STEP) - first_step) as usize + 1;
            self.table.clear();
            self.table.resize(cap + 1, 0);
            for &start in starts {
                self.table[((start >> FLAT_STEP) - first_step) as usize + 1] += 1;
            }
            let mut sum = 0u16;
            for entry in self.table.iter_mut() {
                sum += *entry;
                *entry = sum;
            }
        } else if wide {
            cap = (starts[sessions - 1] >> STEP) as usize + 1;
            self.table.clear();
            self.table.resize(cap + 1, 0);
            let mut from = 0usize;
            for (at, &start) in starts.iter().enumerate() {
                let step = (start >> STEP) as usize;
                if step >= from {
                    self.table[from..=step].fill(at as u16);
                    from = step + 1;
                }
            }
            self.table[cap] = sessions as u16;
        }
        let (stride, stamp) = (self.words, self.stamp);
        let (st, table, prefix) = (self.st.as_ptr(), self.table.as_ptr(), self.prefix.as_mut_ptr());
        let (best_ts, best_at) = (self.best_ts.as_mut_ptr(), self.best_at.as_mut_ptr());
        let (cmask, seen, touched) = (self.cmask.as_mut_ptr(), self.seen.as_mut_ptr(), self.touched.as_mut_ptr());
        // SAFETY: `st` holds the starts and `u32::MAX` after them, so a count stops at
        // `sessions` at the latest (no click is at `u32::MAX`, and a week's end is kept below
        // it); `table` has `cap + 1` entries; `best_*` and `prefix` have `sessions + 1`; a
        // campaign is below the counters' count (the caller's contract), which is `seen`'s, and
        // its `stride >= words` words of `cmask` hold bits below `sessions` only; `touched` has
        // room for a campaign per click. AVX-512 F and BMI2 are compiled in.
        unsafe {
            prefix_rows::<NARROW>(prefix, days);

            let mut marked = 0usize;
            for (k, click) in clicks.iter().enumerate() {
                let ts = click.ts;
                let end = ts.saturating_add(week).min(u32::MAX - 1);
                let (lo, hi) = if wide && FLAT {
                    let step = |at: u32| ((at >> FLAT_STEP).saturating_sub(first_step) as usize).min(cap);
                    let mut lo = usize::from(*table.add(step(ts)));
                    for _ in 0..3 {
                        lo += usize::from(*st.add(lo) < ts);
                    }
                    while *st.add(lo) < ts {
                        lo += 1;
                    }
                    let mut hi = usize::from(*table.add(step(end)));
                    for _ in 0..3 {
                        hi += usize::from(*st.add(hi) <= end);
                    }
                    while *st.add(hi) <= end {
                        hi += 1;
                    }
                    (lo, hi)
                } else if wide {
                    let mut lo = usize::from(*table.add(((ts >> STEP) as usize).min(cap)));
                    lo += usize::from(*st.add(lo) < ts);
                    while *st.add(lo) < ts {
                        lo += 1;
                    }
                    let mut hi = usize::from(*table.add(((end >> STEP) as usize).min(cap)));
                    hi += usize::from(*st.add(hi) <= end);
                    while *st.add(hi) <= end {
                        hi += 1;
                    }
                    (lo, hi)
                } else {
                    count_starts::<NARROW>(st, ts, end)
                };
                // The latest click of this `lo`.
                let (held_ts, held) = (*best_ts.add(lo), *best_at.add(lo));
                let take = (held == NO_CLICK) | (ts > held_ts);
                if FLAT {
                    *best_ts.add(lo) = core::hint::select_unpredictable(take, ts, held_ts);
                    *best_at.add(lo) = core::hint::select_unpredictable(take, k as u32, held);
                } else {
                    *best_ts.add(lo) = if take { ts } else { held_ts };
                    *best_at.add(lo) = if take { k as u32 } else { held };
                }
                if !take && ts == held_ts {
                    let other = clicks.get_unchecked(held as usize);
                    if crate::id_order(click, other) == std::cmp::Ordering::Less {
                        *best_at.add(lo) = k as u32;
                    }
                }
                // Its campaign's sessions.
                let campaign = click.campaign as usize;
                *touched.add(marked) = campaign as u32;
                marked += usize::from(*seen.add(campaign) != stamp);
                *seen.add(campaign) = stamp;
                let row = cmask.add(campaign * stride);
                if words == 1 {
                    *row |= _bzhi_u64(!0, hi as u32) & !_bzhi_u64(!0, lo as u32);
                } else {
                    for word in lo >> 6..=(hi.max(1) - 1) >> 6 {
                        let (from, to) = (lo.saturating_sub(64 * word).min(64), hi.saturating_sub(64 * word).min(64));
                        *row.add(word) |= _bzhi_u64(!0, to as u32) & !_bzhi_u64(!0, from as u32);
                    }
                }
            }

            // The credit: the latest click so far, while it is within the week.
            // Whether a session has a click of its own, and whether the latest is in its
            // week, are as good as coin tosses: selects, not jumps. Click 0 stands in where
            // there is none yet.
            let (mut latest_ts, mut latest) = (0u32, NO_CLICK);
            for at in 0..sessions {
                let here = *best_at.add(at);
                let own = here != NO_CLICK;
                if FLAT {
                    latest_ts = core::hint::select_unpredictable(own, *best_ts.add(at), latest_ts);
                    latest = core::hint::select_unpredictable(own, here, latest);
                } else {
                    latest_ts = if own { *best_ts.add(at) } else { latest_ts };
                    latest = if own { here } else { latest };
                }
                let any = latest != NO_CLICK;
                let campaign = clicks.get_unchecked(if any { latest as usize } else { 0 }).campaign;
                let credit = credits.get_unchecked_mut(at);
                *credit = if any & (starts.get_unchecked(at).wrapping_sub(latest_ts) <= week) { campaign } else { *credit };
            }

            // The assists: each campaign's runs of sessions, a day's count each.
            for &campaign in std::slice::from_raw_parts(touched, marked) {
                let row = std::slice::from_raw_parts_mut(cmask.add(campaign as usize * stride), words);
                let cells = assisted.as_mut_ptr().add(campaign as usize * DAYS);
                debug_assert!(campaign as usize * DAYS + DAYS <= assisted.len());
                if NARROW {
                    let (mut low, mut high) = (_mm256_loadu_si256(cells.cast()), _mm256_loadu_si256(cells.add(8).cast()));
                    each_run(row, |from, to| {
                        let (from, to) = (prefix.add(from).cast::<u32>(), prefix.add(to).cast::<u32>());
                        low = _mm256_add_epi32(low, _mm256_sub_epi32(_mm256_loadu_si256(to.cast()), _mm256_loadu_si256(from.cast())));
                        high = _mm256_add_epi32(high, _mm256_sub_epi32(_mm256_loadu_si256(to.add(8).cast()), _mm256_loadu_si256(from.add(8).cast())));
                    });
                    _mm256_storeu_si256(cells.cast(), low);
                    _mm256_storeu_si256(cells.add(8).cast(), high);
                } else {
                    let mut sum = _mm512_loadu_si512(cells.cast());
                    each_run(row, |from, to| {
                        sum = _mm512_add_epi32(sum, _mm512_sub_epi32(_mm512_loadu_si512(prefix.add(to).cast()), _mm512_loadu_si512(prefix.add(from).cast())));
                    });
                    _mm512_storeu_si512(cells.cast(), sum);
                }
                row.fill(0);
            }
        }
    }

    /// `person` for at most `LIGHT` clicks.
    #[inline]
    fn light(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
        let (n, sessions) = (clicks.len(), starts.len());
        let (mut ts, mut campaign) = ([0u32; LIGHT], [0u32; LIGHT]);
        for (lane, click) in clicks.iter().enumerate() {
            (ts[lane], campaign[lane]) = (click.ts, click.campaign);
        }
        // A session's cells: at most one per click.
        self.cells.clear();
        self.cells.reserve(sessions * LIGHT + LIGHT);
        let (list, mut cells) = (self.cells.as_mut_ptr(), 0usize);
        // SAFETY: register arithmetic on the two arrays; each store of 16 cells starts at
        // most `sessions * LIGHT` in, inside the room reserved; AVX-512 F and CD are compiled
        // in (this module's cfg).
        unsafe {
            let live = ((1u32 << n) - 1) as __mmask16;
            let (times, campaigns) = (_mm512_loadu_si512(ts.as_ptr().cast()), _mm512_loadu_si512(campaign.as_ptr().cast()));
            // Per lane, the earlier lanes with its campaign, a bit each.
            let earlier = _mm512_conflict_epi32(campaigns);
            let rows = _mm512_slli_epi32::<4>(campaigns);
            let week = _mm512_set1_epi32(WEEK as i32);
            for at in 0..sessions {
                let start = *starts.get_unchecked(at);
                // In the session's week: `ts <= start` and `start - ts <= WEEK`, as one
                // unsigned compare (a later click wraps to more than a week).
                let window = _mm512_cmple_epu32_mask(_mm512_sub_epi32(_mm512_set1_epi32(start as i32), times), week) & live;
                // The credit: the latest of them, and among equals the smallest `click_id`.
                // A session with no click in its week goes through the same steps and keeps
                // the campaign it has (whether it has one is as good as a coin's toss, and
                // a jump over the rest was mispredicted about that often): no lane is the
                // latest, lane 0 stands in, and nothing is stored or counted.
                let latest = _mm512_mask_reduce_max_epu32(window, times);
                let top = _mm512_mask_cmpeq_epu32_mask(window, times, _mm512_set1_epi32(latest as i32));
                let lane = if top & top.wrapping_sub(1) == 0 { top.trailing_zeros() as usize & (LIGHT - 1) } else { smallest(clicks, top) };
                let credit = credits.get_unchecked_mut(at);
                *credit = if window != 0 { *campaign.get_unchecked(lane) } else { *credit };
                // The assists: each campaign in the window once, at its first lane there.
                let repeated = _mm512_mask_test_epi32_mask(window, earlier, _mm512_set1_epi32(i32::from(window)));
                let first = window & !repeated;
                let day = _mm512_set1_epi32(*days.get_unchecked(at) as i32);
                _mm512_storeu_si512(list.add(cells).cast(), _mm512_maskz_compress_epi32(first, _mm512_add_epi32(rows, day)));
                cells += first.count_ones() as usize;
            }
            for at in 0..cells {
                *assisted.get_unchecked_mut(*list.add(at) as usize) += 1;
            }
        }
    }

    /// `light` in 256-bit registers: at most 8 clicks are one, at most 16 two. The latest
    /// click of the window is a maximum over the lanes by three shuffles, and a lane's
    /// earlier lanes of its campaign are compares against each click's campaign in turn.
    #[inline]
    fn light_narrow(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
        let (n, sessions) = (clicks.len(), starts.len());
        let mut campaign = [0u32; LIGHT];
        // A session's cells: at most one per click.
        self.cells.clear();
        self.cells.reserve(sessions * LIGHT + LIGHT);
        let (list, mut cells) = (self.cells.as_mut_ptr(), 0usize);
        // SAFETY: register arithmetic on the clicks' lanes, the array and the tables; each
        // store of 8 cells starts at most `sessions * LIGHT + 8` in, inside the room reserved;
        // AVX-512 F and VL are compiled in (this module's cfg and the caller's).
        unsafe {
            let week = _mm256_set1_epi32(WEEK as i32);
            let (low_ts, low_campaigns) = click_lanes(clicks, 0);
            _mm256_storeu_si256(campaign.as_mut_ptr().cast(), low_campaigns);
            let low_rows = _mm256_slli_epi32::<4>(low_campaigns);
            // The latest time among the lanes of `window` (0 if none), in every lane.
            let latest = |a: __m256i| {
                let a = _mm256_max_epu32(a, _mm256_permute4x64_epi64::<0b01_00_11_10>(a));
                let a = _mm256_max_epu32(a, _mm256_shuffle_epi32::<0b01_00_11_10>(a));
                _mm256_max_epu32(a, _mm256_shuffle_epi32::<0b10_11_00_01>(a))
            };
            if n <= 8 {
                let live = ((1u32 << n) - 1) as __mmask8;
                // Per lane, the earlier lanes with its campaign, a bit each.
                let mut earlier = _mm256_setzero_si256();
                for lane in 0..7 {
                    let same = _mm256_cmpeq_epi32(low_campaigns, _mm256_set1_epi32(campaign[lane] as i32));
                    earlier = _mm256_ternarylogic_epi32::<0xF8>(earlier, same, _mm256_loadu_si256(LATER[lane].as_ptr().cast()));
                }
                for at in 0..sessions {
                    let start = _mm256_set1_epi32(*starts.get_unchecked(at) as i32);
                    // As in `light`, by lanes of 8.
                    let window = _mm256_mask_cmple_epu32_mask(live, _mm256_sub_epi32(start, low_ts), week);
                    let top = _mm256_mask_cmpeq_epi32_mask(window, low_ts, latest(_mm256_maskz_mov_epi32(window, low_ts)));
                    let lane = if top & top.wrapping_sub(1) == 0 { top.trailing_zeros() as usize & (LIGHT - 1) } else { smallest(clicks, u16::from(top)) };
                    let credit = credits.get_unchecked_mut(at);
                    *credit = if window != 0 { *campaign.get_unchecked(lane) } else { *credit };
                    let repeated = _mm256_mask_test_epi32_mask(window, earlier, _mm256_set1_epi32(i32::from(window)));
                    let first = window & !repeated;
                    let day = _mm256_set1_epi32(*days.get_unchecked(at) as i32);
                    _mm256_storeu_si256(list.add(cells).cast(), _mm256_maskz_compress_epi32(first, _mm256_add_epi32(low_rows, day)));
                    cells += first.count_ones() as usize;
                }
            } else {
                let live = ((1u32 << (n - 8)) - 1) as __mmask8;
                let (high_ts, high_campaigns) = click_lanes(clicks, 8);
                _mm256_storeu_si256(campaign.as_mut_ptr().add(8).cast(), high_campaigns);
                let high_rows = _mm256_slli_epi32::<4>(high_campaigns);
                let (mut low_earlier, mut high_earlier) = (_mm256_setzero_si256(), _mm256_setzero_si256());
                for lane in 0..LIGHT - 1 {
                    let this = _mm256_set1_epi32(campaign[lane] as i32);
                    if lane < 7 {
                        let same = _mm256_cmpeq_epi32(low_campaigns, this);
                        low_earlier = _mm256_ternarylogic_epi32::<0xF8>(low_earlier, same, _mm256_loadu_si256(LATER[lane].as_ptr().cast()));
                    }
                    let same = _mm256_cmpeq_epi32(high_campaigns, this);
                    high_earlier = _mm256_ternarylogic_epi32::<0xF8>(high_earlier, same, _mm256_loadu_si256(LATER[lane].as_ptr().add(8).cast()));
                }
                for at in 0..sessions {
                    let start = _mm256_set1_epi32(*starts.get_unchecked(at) as i32);
                    let low = _mm256_cmple_epu32_mask(_mm256_sub_epi32(start, low_ts), week);
                    let high = _mm256_mask_cmple_epu32_mask(live, _mm256_sub_epi32(start, high_ts), week);
                    let most = latest(_mm256_max_epu32(_mm256_maskz_mov_epi32(low, low_ts), _mm256_maskz_mov_epi32(high, high_ts)));
                    let top = u16::from(_mm256_mask_cmpeq_epi32_mask(low, low_ts, most)) | u16::from(_mm256_mask_cmpeq_epi32_mask(high, high_ts, most)) << 8;
                    let window = u16::from(low) | u16::from(high) << 8;
                    let lane = if top & top.wrapping_sub(1) == 0 { top.trailing_zeros() as usize & (LIGHT - 1) } else { smallest(clicks, top) };
                    let credit = credits.get_unchecked_mut(at);
                    *credit = if window != 0 { *campaign.get_unchecked(lane) } else { *credit };
                    let window_lanes = _mm256_set1_epi32(i32::from(window));
                    let low_first = low & !_mm256_mask_test_epi32_mask(low, low_earlier, window_lanes);
                    let high_first = high & !_mm256_mask_test_epi32_mask(high, high_earlier, window_lanes);
                    let day = _mm256_set1_epi32(*days.get_unchecked(at) as i32);
                    _mm256_storeu_si256(list.add(cells).cast(), _mm256_maskz_compress_epi32(low_first, _mm256_add_epi32(low_rows, day)));
                    cells += low_first.count_ones() as usize;
                    _mm256_storeu_si256(list.add(cells).cast(), _mm256_maskz_compress_epi32(high_first, _mm256_add_epi32(high_rows, day)));
                    cells += high_first.count_ones() as usize;
                }
            }
            for at in 0..cells {
                *assisted.get_unchecked_mut(*list.add(at) as usize) += 1;
            }
        }
    }

    /// `person` for any number of clicks. `NARROW`: a campaign's days in two 256-bit registers.
    fn heavy<const NARROW: bool>(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
        let (n, sessions) = (clicks.len(), starts.len());
        // The clicks in time order, the winner of a tie last: the smallest `click_id`.
        self.keys.clear();
        self.keys.extend(clicks.iter().enumerate().map(|(at, click)| u64::from(click.ts) << 32 | at as u64));
        self.keys.sort_unstable();
        let keys = &mut self.keys[..];
        let mut first = 0;
        for at in 1..=n {
            if at == n || keys[at] >> 32 != keys[first] >> 32 {
                if at - first > 1 {
                    keys[first..at].sort_unstable_by(|&a, &b| crate::id_order(&clicks[b as u32 as usize], &clicks[a as u32 as usize]));
                }
                first = at;
            }
        }
        self.ts.clear();
        self.ts.extend(keys.iter().map(|&key| (key >> 32) as u32));
        self.campaign.clear();
        self.campaign.extend(keys.iter().map(|&key| clicks[key as u32 as usize].campaign));

        // `prefix[x]`: the sessions before session `x` on each day.
        if self.prefix.len() < sessions + 1 {
            self.prefix.resize(sessions + 1, [0; DAYS]);
        }
        // Positions of this person's sessions are counted from `base` in `last`.
        if self.base > u32::MAX / 2 {
            self.last.fill(0);
            self.base = 0;
        }
        let base = self.base + 1;
        self.base = base + sessions as u32;
        let (ts, campaign, prefix, last) = (&self.ts[..], &self.campaign[..], &mut self.prefix[..], &mut self.last[..]);
        // SAFETY: `prefix` holds `sessions + 1` rows and every position below is at most
        // `sessions`; a campaign is below the counters' and `last`'s count (the caller's
        // contract), and its row of `assisted` is `DAYS` cells.
        unsafe {
            prefix_rows::<NARROW>(prefix.as_mut_ptr(), days);

            // Click `k` covers sessions `lo..hi`: from the first that starts at or after it
            // to the first that starts more than a week after it. Both only grow.
            let (mut lo, mut hi) = (0usize, 0usize);
            // The click before: its stretch and campaign, credited up to this click's.
            let (mut held_lo, mut held_hi, mut held) = (0usize, 0usize, 0u32);
            for k in 0..n {
                let (click, credited) = (ts[k], campaign[k]);
                while lo < sessions && starts[lo] < click {
                    lo += 1;
                }
                let end = click.saturating_add(WEEK as u32);
                while hi < sessions && starts[hi] <= end {
                    hi += 1;
                }
                credits[held_lo..lo.min(held_hi)].fill(held);
                (held_lo, held_hi, held) = (lo, hi, credited);
                // The campaign's sessions not counted yet: from the end of its last stretch.
                let seen = last.get_unchecked_mut(credited as usize);
                let from = (lo as u32 + base).max(*seen) - base;
                *seen = hi as u32 + base;
                let row = assisted.as_mut_ptr().add(credited as usize * DAYS);
                debug_assert!(credited as usize * DAYS + DAYS <= assisted.len() && from as usize <= hi);
                let (from, to) = (prefix.as_ptr().add(from as usize).cast::<u32>(), prefix.as_ptr().add(hi).cast::<u32>());
                if NARROW {
                    for half in [0, 8] {
                        let added = _mm256_sub_epi32(_mm256_loadu_si256(to.add(half).cast()), _mm256_loadu_si256(from.add(half).cast()));
                        _mm256_storeu_si256(row.add(half).cast(), _mm256_add_epi32(_mm256_loadu_si256(row.add(half).cast()), added));
                    }
                } else {
                    let added = _mm512_sub_epi32(_mm512_loadu_si512(to.cast()), _mm512_loadu_si512(from.cast()));
                    _mm512_storeu_si512(row.cast(), _mm512_add_epi32(_mm512_loadu_si512(row.cast()), added));
                }
            }
            credits[held_lo..held_hi].fill(held);
        }
    }
}

/// The times and the campaigns of clicks `from..from + 8` (of at most 16), a lane each, 0 in
/// the lanes past the last click: four loads of two clicks, the fields picked by shuffles.
/// The compiler made the plain copy into arrays a gather of 512-bit loads and permutes.
///
/// # Safety
/// AVX-512 F and VL compiled in; `from` is 0 or 8.
#[inline(always)]
unsafe fn click_lanes(clicks: &[Click], from: usize) -> (__m256i, __m256i) {
    const _: () = assert!(size_of::<Click>() == 16 && align_of::<Click>() >= 4);
    const TS: i32 = (core::mem::offset_of!(Click, ts) / 4) as i32;
    const CAMPAIGN: i32 = (core::mem::offset_of!(Click, campaign) / 4) as i32;
    // SAFETY: the masked loads read the clicks that are there; the pointers are formed by
    // `wrapping_add`.
    unsafe {
        // Of two registers of two clicks each: the four times, then the four campaigns.
        let pick = _mm256_setr_epi32(TS, TS + 4, TS + 8, TS + 12, CAMPAIGN, CAMPAIGN + 4, CAMPAIGN + 8, CAMPAIGN + 12);
        let live = ((1u64 << (2 * clicks.len())) - 1) >> (2 * from);
        let load = |pair: usize| _mm256_maskz_loadu_epi64((live >> (4 * pair)) as __mmask8 & 0xF, clicks.as_ptr().wrapping_add(from + 2 * pair).cast());
        let low = _mm256_permutex2var_epi32(load(0), pick, load(1));
        let high = _mm256_permutex2var_epi32(load(2), pick, load(3));
        (_mm256_permute2x128_si256::<0x20>(low, high), _mm256_permute2x128_si256::<0x31>(low, high))
    }
}

/// `LATER[j]`: in lane `i` (of 16), the bit of lane `j` if `i` is past it. A lane's earlier
/// lanes of its campaign are those `j` whose campaign it equals, each with its bit here.
static LATER: [[u32; LIGHT]; LIGHT] = {
    let mut table = [[0; LIGHT]; LIGHT];
    let mut j = 0;
    while j < LIGHT {
        let mut i = j + 1;
        while i < LIGHT {
            table[j][i] = 1 << j;
            i += 1;
        }
        j += 1;
    }
    table
};

/// `prefix[at]` for each session `at` and one more: the sessions before it on each day, 16
/// days a row, in one 512-bit register or (`NARROW`) two of 256.
///
/// # Safety
/// `days.len() + 1` rows at `prefix`; AVX-512 F (and VL for `NARROW`) compiled in.
#[inline(always)]
unsafe fn prefix_rows<const NARROW: bool>(prefix: *mut [u32; DAYS], days: &[u32]) {
    // SAFETY: the caller's contract.
    unsafe {
        if NARROW {
            let (mut low, mut high) = (_mm256_setzero_si256(), _mm256_setzero_si256());
            let one = _mm256_set1_epi32(1);
            for (at, &day) in days.iter().enumerate() {
                let row = prefix.add(at).cast::<u32>();
                _mm256_storeu_si256(row.cast(), low);
                _mm256_storeu_si256(row.add(8).cast(), high);
                let bit = 1u32 << (day as usize & (DAYS - 1));
                low = _mm256_mask_add_epi32(low, bit as __mmask8, low, one);
                high = _mm256_mask_add_epi32(high, (bit >> 8) as __mmask8, high, one);
            }
            let row = prefix.add(days.len()).cast::<u32>();
            _mm256_storeu_si256(row.cast(), low);
            _mm256_storeu_si256(row.add(8).cast(), high);
        } else {
            let mut before = _mm512_setzero_si512();
            let one = _mm512_set1_epi32(1);
            for (at, &day) in days.iter().enumerate() {
                _mm512_storeu_si512(prefix.add(at).cast(), before);
                before = _mm512_mask_add_epi32(before, (1u32 << (day as usize & (DAYS - 1))) as __mmask16, before, one);
            }
            _mm512_storeu_si512(prefix.add(days.len()).cast(), before);
        }
    }
}

/// Of the 16 starts at `st`: how many are before `ts`, and how many at or before `end`.
///
/// # Safety
/// 16 values at `st`; AVX-512 F (and VL for `NARROW`) compiled in.
#[inline(always)]
unsafe fn count_starts<const NARROW: bool>(st: *const u32, ts: u32, end: u32) -> (usize, usize) {
    // SAFETY: the caller's contract.
    unsafe {
        if NARROW {
            let (low, high) = (_mm256_loadu_si256(st.cast()), _mm256_loadu_si256(st.add(8).cast()));
            let (ts, end) = (_mm256_set1_epi32(ts as i32), _mm256_set1_epi32(end as i32));
            (
                (_mm256_cmplt_epu32_mask(low, ts).count_ones() + _mm256_cmplt_epu32_mask(high, ts).count_ones()) as usize,
                (_mm256_cmple_epu32_mask(low, end).count_ones() + _mm256_cmple_epu32_mask(high, end).count_ones()) as usize,
            )
        } else {
            let all = _mm512_loadu_si512(st.cast());
            (_mm512_cmplt_epu32_mask(all, _mm512_set1_epi32(ts as i32)).count_ones() as usize, _mm512_cmple_epu32_mask(all, _mm512_set1_epi32(end as i32)).count_ones() as usize)
        }
    }
}

/// Calls `f(from, to)` for every run of set bits `from..to` of `words` (bit `b` of word `w` is
/// `64 * w + b`), ascending. A run may cross from a word into the next.
#[inline(always)]
fn each_run(words: &[u64], mut f: impl FnMut(usize, usize)) {
    // Where a run that reached its word's last bit began.
    let mut open = usize::MAX;
    for (at, &word) in words.iter().enumerate() {
        let (base, mut rest) = (64 * at, word);
        if open != usize::MAX {
            let more = (!rest).trailing_zeros() as usize;
            if more == 64 {
                continue;
            }
            f(open, base + more);
            open = usize::MAX;
            rest &= !0u64 << more;
        }
        while rest != 0 {
            let from = rest.trailing_zeros() as usize;
            let to = from + (!(rest >> from)).trailing_zeros() as usize;
            if to == 64 {
                open = base + from;
                break;
            }
            f(base + from, base + to);
            rest &= !0u64 << to;
        }
    }
    if open != usize::MAX {
        f(open, 64 * words.len());
    }
}

/// Among the lanes `lanes` (several clicks of one `ts`), the one with the smallest `click_id`.
#[cold]
#[inline(never)]
fn smallest(clicks: &[Click], lanes: u16) -> usize {
    let mut rest = lanes;
    let mut best = rest.trailing_zeros() as usize;
    rest &= rest - 1;
    while rest != 0 {
        let lane = rest.trailing_zeros() as usize;
        rest &= rest - 1;
        if crate::id_order(&clicks[lane], &clicks[best]) == std::cmp::Ordering::Less {
            best = lane;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use types::{Ctx, id};
    use types::time::{DAY, parse_f1};

    use super::*;
    use crate::attr_interval;
    use crate::partitioned::sort_clicks_for_test;

    /// Two clicks at one instant, for two campaigns, under IDs whose order is open: IDs that
    /// compare equal, or 9 bytes and an ID kept whole that begins with them, either way
    /// round. Reported wherever the tie is broken (the flag only ever goes up outside this
    /// test, so each check is of the call before it).
    #[test]
    fn reports_a_tie_the_ids_cannot_break() {
        let twins = |n: usize, ids: [&[u8]; 2]| -> Vec<Click> {
            let mut clicks = vec![Click { ts: 100, campaign: 1, id: id::pack(ids[0]) }, Click { ts: 100, campaign: 2, id: id::pack(ids[1]) }];
            clicks.extend((2..n).map(|at| Click { ts: at as u32, campaign: 3, id: id::pack(format!("kx{at}").as_bytes()) }));
            clicks
        };
        let (starts, days) = ([200u32], [0u32]);
        let mut attr = Attr::new(8);
        let mut assisted = vec![0u32; 8 * DAYS];
        let (nine, whole): (&[u8], &[u8]) = (b"k00000000", b"k0000000000000000000000000000000A");
        for ids in [[&b"k1"[..], b"k1"], [nine, whole], [whole, nine]] {
            for n in [2, LIGHT, LIGHT + 4] {
                crate::reset_tie();
                let mut credits = [7u32];
                attr.person(&twins(n, ids), &starts, &days, &mut credits, &mut assisted);
                assert!(crate::ambiguous_tie(), "{n} clicks");
                crate::reset_tie();
                attr.person_narrow(&twins(n, ids), &starts, &days, &mut credits, &mut assisted);
                assert!(crate::ambiguous_tie(), "{n} clicks, narrow");
                crate::reset_tie();
                sort_clicks_for_test(&mut twins(n, ids));
                assert!(crate::ambiguous_tie(), "{n} clicks sorted");
            }
        }
    }

    /// Random persons through both attributions: the same credits, the same assists.
    #[test]
    fn agrees_with_attr_interval() {
        let campaigns: Vec<(Vec<u8>, Vec<u8>)> = (0..40).map(|at| (format!("c_{at}").into_bytes(), b"search".to_vec())).collect();
        let rows: Vec<(&[u8], &[u8])> = campaigns.iter().map(|(id, channel)| (&id[..], &channel[..])).collect();
        let ctx = Ctx::new(parse_f1(b"2026-10-05T00:00:00Z").unwrap(), parse_f1(b"2026-10-19T00:00:00Z").unwrap(), &[], &rows).unwrap();
        let (real, organic) = (ctx.organic() as usize, ctx.organic());
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |below: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % below
        };
        // Both ways of a long run of clicks: unsorted through the table, and sorted.
        let (mut wide, mut by_sort) = (Attr::new(ctx.campaigns.len()), Attr::new(ctx.campaigns.len()));
        (wide.sorted, by_sort.sorted) = (false, true);
        let mut by_sorted = vec![0u32; ctx.campaigns.len() * DAYS];
        // And both in 256-bit registers, and the table without jumps both ways.
        let (mut narrow, mut narrow_sort) = (Attr::new(ctx.campaigns.len()), Attr::new(ctx.campaigns.len()));
        (narrow.sorted, narrow_sort.sorted) = (false, true);
        let (mut by_narrow, mut by_narrow_sorted) = (vec![0u32; ctx.campaigns.len() * DAYS], vec![0u32; ctx.campaigns.len() * DAYS]);
        let (mut flat, mut narrow_flat) = (Attr::new(ctx.campaigns.len()), Attr::new(ctx.campaigns.len()));
        (flat.flat, narrow_flat.flat) = (true, true);
        let (mut by_flat, mut by_narrow_flat) = (vec![0u32; ctx.campaigns.len() * DAYS], vec![0u32; ctx.campaigns.len() * DAYS]);
        let mut interval = attr_interval::Attr::new(&ctx);
        let cells = ctx.campaigns.len() * ctx.n_days;
        let (mut by_wide, mut by_interval) = (vec![0u32; ctx.campaigns.len() * DAYS], vec![0u32; cells]);
        for person in 0..4000 {
            // Few clicks or many, few campaigns or many, and times that often collide: on a
            // session's start, a week before it to the millisecond, one more, and each other.
            let clicks_n = match person % 5 {
                0 => next(4),
                1 => 1 + next(16),
                2 => 16 + next(3),
                3 => 17 + next(40),
                _ => next(300),
            } as usize;
            let sessions_n = 1 + next(if person % 7 == 0 { 400 } else { 12 }) as usize;
            let spread = if person % 3 == 0 { 14 * DAY as u64 } else { 2 * DAY as u64 };
            let from = ctx.event_lo as u64 + next(14 * DAY as u64 - spread + 1);
            let mut starts: Vec<u32> = (0..sessions_n).map(|_| (from + next(spread)) as u32).collect();
            starts.sort_unstable();
            starts.dedup();
            let days: Vec<u32> = starts.iter().map(|&start| (ctx.day_of(start) - ctx.day0) as u32).collect();
            let few = 1 + next(real as u64);
            let mut clicks: Vec<Click> = (0..clicks_n)
                .map(|at| {
                    let start = u64::from(starts[next(starts.len() as u64) as usize]);
                    let ts = match next(8) {
                        0 => start,
                        1 => start + 1,
                        2 => start.saturating_sub(WEEK as u64),
                        3 => start.saturating_sub(WEEK as u64 + 1),
                        4 => start.saturating_sub(next(WEEK as u64)),
                        _ => next(u64::from(ctx.ts_end)),
                    };
                    // Twins now and then: the same instant, another campaign and `click_id`.
                    // Every click its own `click_id`, in no order.
                    Click { ts: ts.min(u64::from(ctx.ts_end) - 1) as u32, campaign: next(few) as u32, id: id::pack(format!("k{}", next(50) * 1000 + at as u64).as_bytes()) }
                })
                .collect();
            let mut got = vec![organic; starts.len()];
            wide.person(&clicks, &starts, &days, &mut got, &mut by_wide);
            let mut got_sorted = vec![organic; starts.len()];
            by_sort.person(&clicks, &starts, &days, &mut got_sorted, &mut by_sorted);
            assert_eq!(got, got_sorted, "person {person}: {clicks_n} clicks, {} sessions, table against sort", starts.len());
            for (attr, assisted, what) in [(&mut narrow, &mut by_narrow, "narrow"), (&mut narrow_sort, &mut by_narrow_sorted, "narrow sort"), (&mut narrow_flat, &mut by_narrow_flat, "narrow flat")] {
                let mut got_narrow = vec![organic; starts.len()];
                attr.person_narrow(&clicks, &starts, &days, &mut got_narrow, assisted);
                assert_eq!(got, got_narrow, "person {person}: {clicks_n} clicks, {} sessions, {what}", starts.len());
            }
            let mut got_flat = vec![organic; starts.len()];
            flat.person(&clicks, &starts, &days, &mut got_flat, &mut by_flat);
            assert_eq!(got, got_flat, "person {person}: {clicks_n} clicks, {} sessions, flat", starts.len());
            let sorted = sort_clicks_for_test(&mut clicks);
            let want: Vec<u32> = starts.iter().zip(&days).map(|(&start, &day)| interval.session(&sorted, start, day as usize, organic, &mut by_interval)).collect();
            interval.end_person(&mut by_interval);
            assert_eq!(got, want, "person {person}: {clicks_n} clicks, {} sessions", starts.len());
        }
        for campaign in 0..ctx.campaigns.len() {
            for day in 0..ctx.n_days {
                assert_eq!(by_wide[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "campaign {campaign}, day {day}");
                assert_eq!(by_sorted[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "sorted: campaign {campaign}, day {day}");
                assert_eq!(by_narrow[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "narrow: campaign {campaign}, day {day}");
                assert_eq!(by_narrow_sorted[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "narrow sorted: campaign {campaign}, day {day}");
                assert_eq!(by_flat[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "flat: campaign {campaign}, day {day}");
                assert_eq!(by_narrow_flat[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "narrow flat: campaign {campaign}, day {day}");
            }
            assert!(by_wide[campaign * DAYS + ctx.n_days..(campaign + 1) * DAYS].iter().all(|&count| count == 0));
        }
        assert!(by_wide.iter().any(|&count| count > 0));
    }
}
