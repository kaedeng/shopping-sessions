//! Slot `sess-partitioned`: one person bucket end to end (decision: Sessionize). Numbers the
//! bucket's persons in a small table, gathers each event as the key `local << ts_bits | ts`,
//! groups the clicks by person with a counting scatter, sorts the keys, then walks every
//! person's sessions in `ts` order through attribution and aggregation.
//!
//! Contract: `bucket(ctx, events, clicks, agg)` exactly as `crate::bucket`; `Scratch` is its
//! per-worker state inside `Agg`.

use std::cmp::Ordering;

use types::hash::person_hash;
use types::id::{self, Id};
use types::record::{ClickRec, PersonEvent};
use types::Ctx;

use crate::{Agg, Click};

/// A gap over this starts a session; exactly this stays.
pub(crate) const GAP: u64 = 1_800_000;
/// No local number: a click whose person has no event in the bucket.
const NONE: u32 = u32::MAX;
/// Clicks of one person above this are sorted by index (`sort_clicks`).
const LONG_RUN: usize = 16;

// A new person is also a gap over GAP in the keys, since `ts >= event_lo` (window start, 7 days
// after the zero of `ts`): one compare per row finds both.
const _: () = assert!(types::time::WEEK as u64 > GAP);

#[derive(Default)]
pub struct Scratch {
    /// Open addressing on the person-hash bits below the bucket bits, linear probing, at most
    /// half full: `(person, local)`, person 0 (the empty ID, never a person) for free.
    slots: Vec<(Id, u32)>,
    shift: u32,
    bucket_bits: u32,
    /// Local number -> person.
    persons: Vec<Id>,
    /// Each click's local number in gather order, or `NONE`.
    click_local: Vec<u32>,
    /// Per local number, one past its last click in `clicks` once gathered.
    click_end: Vec<u32>,
    clicks: Vec<Click>,
    /// A long click run sorted: `ts << 32 | position`, then the clicks in that order.
    order: Vec<u64>,
    sorted: Vec<Click>,
    /// `bucket_wide`: the person in hand's sessions, one entry each.
    starts: Vec<u32>,
    days: Vec<u32>,
    revenue: Vec<u64>,
    credits: Vec<u32>,
}

impl Scratch {
    fn reset(&mut self, rows: usize, bucket_bits: u32) {
        self.bucket_bits = bucket_bits;
        self.persons.clear();
        // About 30 rows per person at full size: 8 gives room without a grow in most buckets.
        self.size((rows / 8).next_power_of_two().max(64));
    }

    fn size(&mut self, slots: usize) {
        self.slots.clear();
        self.slots.resize(slots, (0, 0));
        self.shift = 64 - slots.trailing_zeros();
    }

    #[inline(always)]
    fn home(&self, person: Id) -> usize {
        ((person_hash(person) << self.bucket_bits) >> self.shift) as usize
    }

    #[inline(always)]
    fn intern(&mut self, person: Id) -> u32 {
        debug_assert!(person != 0);
        let mask = self.slots.len() - 1;
        let mut at = self.home(person);
        loop {
            // SAFETY: `at <= mask < slots.len()`.
            let (key, local) = unsafe { *self.slots.get_unchecked(at) };
            if key == person {
                return local;
            }
            if key == 0 {
                return self.insert(at, person);
            }
            at = (at + 1) & mask;
        }
    }

    fn insert(&mut self, at: usize, person: Id) -> u32 {
        let local = self.persons.len() as u32;
        self.slots[at] = (person, local);
        self.persons.push(person);
        if 2 * self.persons.len() > self.slots.len() {
            self.grow();
        }
        local
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        self.size(2 * self.slots.len());
        let mask = self.slots.len() - 1;
        for (local, &person) in self.persons.iter().enumerate() {
            let mut at = self.home(person);
            while self.slots[at].0 != 0 {
                at = (at + 1) & mask;
            }
            self.slots[at] = (person, local as u32);
        }
    }

    #[inline(always)]
    fn find(&self, person: Id) -> u32 {
        debug_assert!(person != 0);
        let mask = self.slots.len() - 1;
        let mut at = self.home(person);
        loop {
            // SAFETY: `at <= mask < slots.len()`.
            let (key, local) = unsafe { *self.slots.get_unchecked(at) };
            if key == person {
                return local;
            }
            if key == 0 {
                return NONE;
            }
            at = (at + 1) & mask;
        }
    }

    /// Groups the clicks by local number, dropping those of persons without an event: counts
    /// per person, then a scatter over a second read of the pieces.
    fn gather_clicks<'a>(&mut self, clicks: impl Iterator<Item = &'a [ClickRec]> + Clone) {
        let total: usize = clicks.clone().map(<[_]>::len).sum();
        let persons = self.persons.len();
        self.click_local.clear();
        self.click_local.reserve(total);
        self.click_end.clear();
        self.click_end.resize(persons + 1, 0);
        for piece in clicks.clone() {
            for click in piece {
                let local = self.find(click.user);
                if local != NONE {
                    // SAFETY: `local < persons`.
                    unsafe { *self.click_end.get_unchecked_mut(local as usize + 1) += 1 };
                }
                self.click_local.push(local);
            }
        }
        for local in 1..=persons {
            self.click_end[local] += self.click_end[local - 1];
        }
        // `click_end[local]` is now the person's first position, and the scatter moves it to
        // one past the last.
        let kept = self.click_end[persons] as usize;
        self.clicks.clear();
        self.clicks.reserve(kept);
        let (out, mut next) = (self.clicks.as_mut_ptr(), self.click_local.iter());
        for piece in clicks {
            for click in piece {
                // SAFETY: `click_local` holds one entry per click, read in the same order.
                let local = *unsafe { next.next().unwrap_unchecked() };
                if local != NONE {
                    // SAFETY: `local < persons`; the counts make the positions a permutation
                    // of `0..kept`, inside the reserved room.
                    unsafe {
                        let at = self.click_end.get_unchecked_mut(local as usize);
                        out.add(*at as usize).write(Click { ts: click.ts, campaign: click.campaign, id: click.click });
                        *at += 1;
                    }
                }
            }
        }
        // SAFETY: every position in `0..kept` was written once above.
        unsafe { self.clicks.set_len(kept) };
    }
}

/// By `ts`, and among equal `ts` the smallest `click_id` last: the credit is the window's last.
fn click_order(a: &Click, b: &Click) -> Ordering {
    a.ts.cmp(&b.ts).then_with(|| {
        if id::less(a.id, b.id) {
            Ordering::Greater
        } else if id::less(b.id, a.id) {
            Ordering::Less
        } else {
            crate::note_tie(a, b);
            Ordering::Equal
        }
    })
}

/// One person's clicks in `click_order`. A long run sorts `ts << 32 | position` as plain `u64`
/// and gathers once, which beats moving 16-byte clicks through the sort; equal `ts` are rare
/// and get their `click_id` order after.
fn sort_clicks<'s>(clicks: &'s mut [Click], order: &mut Vec<u64>, sorted: &'s mut Vec<Click>) -> &'s [Click] {
    if clicks.len() <= LONG_RUN {
        clicks.sort_unstable_by(click_order);
        return clicks;
    }
    order.clear();
    order.extend(clicks.iter().enumerate().map(|(at, click)| u64::from(click.ts) << 32 | at as u64));
    order.sort_unstable();
    sorted.clear();
    sorted.extend(order.iter().map(|&key| clicks[key as u32 as usize]));
    let mut first = 0;
    for at in 1..=sorted.len() {
        if at == sorted.len() || sorted[at].ts != sorted[first].ts {
            if at - first > 1 {
                sorted[first..at].sort_unstable_by(click_order);
            }
            first = at;
        }
    }
    sorted
}

/// A person's clicks in `click_order`, for tests of other attributions.
#[cfg(test)]
pub(crate) fn sort_clicks_for_test(clicks: &mut [Click]) -> Vec<Click> {
    clicks.sort_unstable_by(click_order);
    clicks.to_vec()
}

/// One person's clicks in `click_order`, sorted in place or into the scratch (`sort_clicks`).
#[cfg(feature = "pipe-positional")]
pub(crate) fn sort_person_clicks<'s>(scratch: &'s mut Scratch, clicks: &'s mut [Click]) -> &'s [Click] {
    sort_clicks(clicks, &mut scratch.order, &mut scratch.sorted)
}

/// `bucket` with a person's sessions listed first and credited together by `attr-wide`: no
/// click is sorted, a person with at most 16 clicks is one compare a session, and the assists
/// of one with more are counted per campaign, not per session and campaign. Returns `false`,
/// with nothing done, where that attribution is not there (a window of more than 16 days).
///
/// The rows, the purchases and the credits are the calls `bucket` makes, a person's credits
/// after its rows instead of between them.
#[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
pub fn bucket_wide<'a>(
    ctx: &Ctx,
    events: impl Iterator<Item = &'a [PersonEvent]> + Clone,
    clicks: impl Iterator<Item = &'a [ClickRec]> + Clone,
    agg: &mut Agg,
) -> bool {
    let Agg { user_daily, bucket: scratch, rows, wide, day, counters, .. } = agg;
    let Some(wide) = wide else { return false };
    let n: usize = events.clone().map(<[_]>::len).sum();
    let ts_bits = ctx.ts_bits;
    scratch.reset(n, ctx.bucket_bits);
    rows.clear(n);
    for piece in events {
        rows.extend(piece.iter().map(|event| {
            debug_assert!(event.ts >= ctx.event_lo && event.ts < ctx.ts_end);
            (u64::from(scratch.intern(event.person)) << ts_bits | u64::from(event.ts), event.pay)
        }));
    }
    let persons = scratch.persons.len();
    rows.sort(ts_bits + (usize::BITS - persons.leading_zeros()), ts_bits);
    scratch.gather_clicks(clicks);

    let Scratch { persons: names, click_end, clicks: grouped, starts, days, revenue, credits, .. } = scratch;
    let ts_mask = (1u64 << ts_bits) - 1;
    let organic = ctx.organic();
    let (mut at, mut first_click) = (0, 0);
    for local in 0..persons {
        let person = names[local];
        let end_click = click_end[local] as usize;
        let clicks = &grouped[first_click..end_click];
        first_click = end_click;
        starts.clear();
        days.clear();
        revenue.clear();
        loop {
            let first = at;
            let start = (rows.key(at) & ts_mask) as u32;
            at += 1;
            // The sentinel key ends the last session of the bucket.
            while rows.key(at) - rows.key(at - 1) <= GAP {
                at += 1;
            }
            let d = (ctx.day_of(start) - ctx.day0) as usize;
            day.session(d, at - first, person, ctx.day0, user_daily);
            revenue.push(rows.purchases(first..at, |price, category| day.purchase(price, category)));
            starts.push(start);
            days.push(d as u32);
            if rows.key(at) >> ts_bits != local as u64 {
                break;
            }
        }
        day.flush(person, ctx.day0, user_daily);
        credits.clear();
        credits.resize(starts.len(), organic);
        wide.person(clicks, starts, days, credits, counters.wide());
        for ((&campaign, &d), &paid) in credits.iter().zip(days.iter()).zip(revenue.iter()) {
            counters.credit(campaign, d as usize, paid);
        }
    }
    debug_assert_eq!(at, n);
    true
}

/// `STRETCH` picks the attribution: `attr-interval` (assists counted per stretch of sessions)
/// where the build has it, else the bucket's own, `attr-sweep` (per session and campaign).
pub fn bucket<'a, const STRETCH: bool>(
    ctx: &Ctx,
    events: impl Iterator<Item = &'a [PersonEvent]> + Clone,
    clicks: impl Iterator<Item = &'a [ClickRec]> + Clone,
    agg: &mut Agg,
) {
    #[cfg(feature = "pipe-positional")]
    let Agg { user_daily, bucket: scratch, rows, attr, stretch, day, counters, .. } = agg;
    #[cfg(not(feature = "pipe-positional"))]
    let Agg { user_daily, bucket: scratch, rows, attr, day, counters } = agg;
    let n: usize = events.clone().map(<[_]>::len).sum();
    let ts_bits = ctx.ts_bits;
    scratch.reset(n, ctx.bucket_bits);
    rows.clear(n);
    for piece in events {
        rows.extend(piece.iter().map(|event| {
            debug_assert!(event.ts >= ctx.event_lo && event.ts < ctx.ts_end);
            (u64::from(scratch.intern(event.person)) << ts_bits | u64::from(event.ts), event.pay)
        }));
    }
    let persons = scratch.persons.len();
    rows.sort(ts_bits + (usize::BITS - persons.leading_zeros()), ts_bits);
    scratch.gather_clicks(clicks);

    let ts_mask = (1u64 << ts_bits) - 1;
    let organic = ctx.organic();
    // Every local number has a row and the keys are sorted, so persons come in local order.
    let (mut at, mut first_click) = (0, 0);
    for local in 0..persons {
        let person = scratch.persons[local];
        let end_click = scratch.click_end[local] as usize;
        let clicks = sort_clicks(&mut scratch.clicks[first_click..end_click], &mut scratch.order, &mut scratch.sorted);
        first_click = end_click;
        loop {
            let first = at;
            let start = (rows.key(at) & ts_mask) as u32;
            at += 1;
            // The sentinel key ends the last session of the bucket.
            while rows.key(at) - rows.key(at - 1) <= GAP {
                at += 1;
            }
            let d = (ctx.day_of(start) - ctx.day0) as usize;
            day.session(d, at - first, person, ctx.day0, user_daily);
            let revenue = rows.purchases(first..at, |price, category| day.purchase(price, category));
            #[cfg(feature = "pipe-positional")]
            let credit =
                if STRETCH { stretch.session(clicks, start, d, organic, counters.assisted()) } else { attr.session(clicks, start, organic, counters.assists(d)) };
            #[cfg(not(feature = "pipe-positional"))]
            let credit = attr.session(clicks, start, organic, counters.assists(d));
            counters.credit(credit, d, revenue);
            if rows.key(at) >> ts_bits != local as u64 {
                break;
            }
        }
        #[cfg(feature = "pipe-positional")]
        if STRETCH {
            stretch.end_person(counters.assisted());
        } else {
            attr.end_person();
        }
        #[cfg(not(feature = "pipe-positional"))]
        attr.end_person();
        day.flush(person, ctx.day0, user_daily);
    }
    debug_assert_eq!(at, n);
}
