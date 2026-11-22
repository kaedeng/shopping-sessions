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
}

impl Attr {
    pub(crate) fn new(campaigns: usize) -> Attr {
        Attr { last: vec![0; campaigns], ..Attr::default() }
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
        if clicks.len() <= LIGHT { self.light(clicks, starts, days, credits, assisted) } else { self.heavy(clicks, starts, days, credits, assisted) }
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
                if window == 0 {
                    continue;
                }
                // The credit: the latest of them, and among equals the smallest `click_id`.
                let latest = _mm512_mask_reduce_max_epu32(window, times);
                let top = _mm512_mask_cmpeq_epu32_mask(window, times, _mm512_set1_epi32(latest as i32));
                let lane = if top & (top - 1) == 0 { top.trailing_zeros() as usize } else { smallest(clicks, top) };
                *credits.get_unchecked_mut(at) = *campaign.get_unchecked(lane);
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
        let mut wide = Attr::new(ctx.campaigns.len());
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
            let sorted = sort_clicks_for_test(&mut clicks);
            let want: Vec<u32> = starts.iter().zip(&days).map(|(&start, &day)| interval.session(&sorted, start, day as usize, organic, &mut by_interval)).collect();
            interval.end_person(&mut by_interval);
            assert_eq!(got, want, "person {person}: {clicks_n} clicks, {} sessions", starts.len());
        }
        for campaign in 0..ctx.campaigns.len() {
            for day in 0..ctx.n_days {
                assert_eq!(by_wide[campaign * DAYS + day], by_interval[day * ctx.campaigns.len() + campaign], "campaign {campaign}, day {day}");
            }
            assert!(by_wide[campaign * DAYS + ctx.n_days..(campaign + 1) * DAYS].iter().all(|&count| count == 0));
        }
        assert!(by_wide.iter().any(|&count| count > 0));
    }
}
