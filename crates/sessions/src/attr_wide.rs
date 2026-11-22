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
    /// `heavy` sorts the clicks (`SS_ATTR_HEAVY=sort`); `heavy_table` does not (the pick).
    sorted: bool,
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
/// No click yet, in `Attr::best_at`.
const NO_CLICK: u32 = u32::MAX;

impl Attr {
    pub(crate) fn new(campaigns: usize) -> Attr {
        Attr { last: vec![0; campaigns], seen: vec![0; campaigns], sorted: types::env::pick("SS_ATTR_HEAVY", &["table", "sort"]) == 1, ..Attr::default() }
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
            self.heavy(clicks, starts, days, credits, assisted)
        } else {
            self.heavy_table(clicks, starts, days, credits, assisted)
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
    fn heavy_table(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
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
        let mut cap = 0usize;
        if wide {
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
            let mut before = _mm512_setzero_si512();
            let one = _mm512_set1_epi32(1);
            for at in 0..sessions {
                _mm512_storeu_si512(prefix.add(at).cast(), before);
                before = _mm512_mask_add_epi32(before, (1u32 << (days[at] as usize & (DAYS - 1))) as __mmask16, before, one);
            }
            _mm512_storeu_si512(prefix.add(sessions).cast(), before);

            let all = _mm512_loadu_si512(st.cast());
            let mut marked = 0usize;
            for (k, click) in clicks.iter().enumerate() {
                let ts = click.ts;
                let end = ts.saturating_add(week).min(u32::MAX - 1);
                let (lo, hi) = if wide {
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
                    (
                        _mm512_cmplt_epu32_mask(all, _mm512_set1_epi32(ts as i32)).count_ones() as usize,
                        _mm512_cmple_epu32_mask(all, _mm512_set1_epi32(end as i32)).count_ones() as usize,
                    )
                };
                // The latest click of this `lo`.
                let (held_ts, held) = (*best_ts.add(lo), *best_at.add(lo));
                let take = (held == NO_CLICK) | (ts > held_ts);
                *best_ts.add(lo) = if take { ts } else { held_ts };
                *best_at.add(lo) = if take { k as u32 } else { held };
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
                latest_ts = if own { *best_ts.add(at) } else { latest_ts };
                latest = if own { here } else { latest };
                let any = latest != NO_CLICK;
                let campaign = clicks.get_unchecked(if any { latest as usize } else { 0 }).campaign;
                let credit = credits.get_unchecked_mut(at);
                *credit = if any & (starts.get_unchecked(at).wrapping_sub(latest_ts) <= week) { campaign } else { *credit };
            }

            // The assists: each campaign's runs of sessions, a day's count each.
            for &campaign in std::slice::from_raw_parts(touched, marked) {
                let row = std::slice::from_raw_parts_mut(cmask.add(campaign as usize * stride), words);
                let cells = assisted.as_mut_ptr().add(campaign as usize * DAYS).cast::<__m512i>();
                debug_assert!(campaign as usize * DAYS + DAYS <= assisted.len());
                let mut sum = _mm512_loadu_si512(cells);
                each_run(row, |from, to| {
                    sum = _mm512_add_epi32(sum, _mm512_sub_epi32(_mm512_loadu_si512(prefix.add(to).cast()), _mm512_loadu_si512(prefix.add(from).cast())));
                });
                _mm512_storeu_si512(cells, sum);
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

    /// `person` for any number of clicks.
    fn heavy(&mut self, clicks: &[Click], starts: &[u32], days: &[u32], credits: &mut [u32], assisted: &mut [u32]) {
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
            let mut before = _mm512_setzero_si512();
            let one = _mm512_set1_epi32(1);
            for at in 0..sessions {
                _mm512_storeu_si512(prefix.as_mut_ptr().add(at).cast(), before);
                before = _mm512_mask_add_epi32(before, (1u32 << (days[at] as usize & (DAYS - 1))) as __mmask16, before, one);
            }
            _mm512_storeu_si512(prefix.as_mut_ptr().add(sessions).cast(), before);

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
                let row = assisted.as_mut_ptr().add(credited as usize * DAYS).cast::<__m512i>();
                debug_assert!(credited as usize * DAYS + DAYS <= assisted.len() && from as usize <= hi);
                let added = _mm512_sub_epi32(_mm512_loadu_si512(prefix.as_ptr().add(hi).cast()), _mm512_loadu_si512(prefix.as_ptr().add(from as usize).cast()));
                _mm512_storeu_si512(row, _mm512_add_epi32(_mm512_loadu_si512(row), added));
            }
            credits[held_lo..held_hi].fill(held);
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
            let sorted = sort_clicks_for_test(&mut clicks);
            let want: Vec<u32> = starts.iter().zip(&days).map(|(&start, &day)| interval.session(&sorted, start, day as usize, organic, &mut by_interval)).collect();
            interval.end_person(&mut by_interval);
            assert_eq!(got, want, "person {person}: {clicks_n} clicks, {} sessions", starts.len());
        }
        for campaign in 0..ctx.campaigns.len() {
            for day in 0..ctx.n_days {
                assert_eq!(by_wide[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "campaign {campaign}, day {day}");
                assert_eq!(by_sorted[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "sorted: campaign {campaign}, day {day}");
            }
            assert!(by_wide[campaign * DAYS + ctx.n_days..(campaign + 1) * DAYS].iter().all(|&count| count == 0));
        }
        assert!(by_wide.iter().any(|&count| count > 0));
    }
}
