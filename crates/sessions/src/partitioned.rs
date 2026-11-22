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

/// `bucket_dense`'s buffers, one set per worker.
#[derive(Default)]
pub struct Dense {
    /// Per person of the bucket, and one more: before the scatter the first position of its
    /// events in `ts`, after it one past the last. The same for the clicks.
    ends: Vec<u32>,
    click_ends: Vec<u32>,
    /// The events' times, a person's together; sorted person by person as they are walked.
    ts: Vec<u32>,
    clicks: Vec<Click>,
    /// The purchases as `(key - base << 32, pay)`, ascending, then a sentinel.
    side: Vec<(u64, u32)>,
}

/// `crate::bucket_dense`: the bucket's persons are the numbers `base..base + span`.
///
/// Nothing is looked up and no key is sorted as a whole. A counting scatter by `person - base`
/// puts each person's times together, straight from the lists (four bytes an event: the
/// person is the position); a person's times are sorted alone when its turn comes; the clicks
/// are grouped the same way. The sessions, the rows and the credits are then the calls
/// `bucket_wide` makes.
#[cfg(feature = "pipe-positional")]
pub fn bucket_dense(
    ctx: &Ctx,
    base: u32,
    span: usize,
    events: &dyn Fn(&mut dyn FnMut(&[u64])),
    pays: &dyn Fn(&mut dyn FnMut(&[crate::DensePay])),
    clicks: &dyn Fn(&mut dyn FnMut(&[ClickRec])),
    agg: &mut Agg,
) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
    let Agg { user_daily, bucket: scratch, attr, wide, day, counters, dense, .. } = agg;
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd")))]
    let Agg { user_daily, bucket: scratch, attr, day, counters, dense, .. } = agg;
    let Dense { ends, click_ends, ts, clicks: grouped, side } = dense;
    let offset = u64::from(base) << 32;
    // A key's person as its place among the bucket's; checked where it is first met, in the
    // counting passes, so that the scatters may trust it.
    let local = |key: u64| (key.wrapping_sub(offset) >> 32) as usize;

    // The events: counted per person, then each time to its person's stretch.
    ends.clear();
    ends.resize(span + 1, 0);
    events(&mut |piece| {
        for &key in piece {
            ends[local(key) + 1] += 1;
        }
    });
    let mut n = 0u32;
    for end in ends.iter_mut() {
        n += *end;
        *end = n;
    }
    let n = n as usize;
    if n == 0 {
        return;
    }
    ts.clear();
    ts.reserve(n);
    {
        let (out, next) = (ts.as_mut_ptr(), ends.as_mut_ptr());
        events(&mut |piece| {
            // Eight keys a round, the loads before the stores that depend on them
            // (`sort_grouped`).
            let mut chunks = piece.chunks_exact(8);
            for chunk in &mut chunks {
                let batch: [u64; 8] = chunk.try_into().expect("chunks of 8");
                let mut at = [0u32; 8];
                for (at, key) in at.iter_mut().zip(batch) {
                    // SAFETY: the count pass indexed the same persons, below `span`.
                    unsafe {
                        let slot = next.add(local(key));
                        *at = *slot;
                        *slot = *at + 1;
                    }
                }
                for (at, key) in at.into_iter().zip(batch) {
                    // SAFETY: the running sums make the positions a permutation of `0..n`,
                    // inside the reserved room.
                    unsafe { out.add(at as usize).write(key as u32) };
                }
            }
            for &key in chunks.remainder() {
                // SAFETY: as above.
                unsafe {
                    let slot = next.add(local(key));
                    out.add(*slot as usize).write(key as u32);
                    *slot += 1;
                }
            }
        });
        // SAFETY: every position in `0..n` was written once above.
        unsafe { ts.set_len(n) };
    }

    // The purchases, in the order the walk meets them.
    side.clear();
    pays(&mut |piece| side.extend(piece.iter().map(|pay| (pay.key.wrapping_sub(offset), pay.pay))));
    side.sort_unstable_by_key(|&(key, _)| key);
    side.push((u64::MAX, 0));

    // The clicks, grouped the same way; those of a person without events stay unread.
    click_ends.clear();
    click_ends.resize(span + 1, 0);
    clicks(&mut |piece| {
        for click in piece {
            click_ends[(click.user as u32).wrapping_sub(base) as usize + 1] += 1;
        }
    });
    let mut kept = 0u32;
    for end in click_ends.iter_mut() {
        kept += *end;
        *end = kept;
    }
    grouped.clear();
    grouped.reserve(kept as usize);
    {
        let (out, next) = (grouped.as_mut_ptr(), click_ends.as_mut_ptr());
        clicks(&mut |piece| {
            for click in piece {
                // SAFETY: the count pass indexed the same persons; the positions are a
                // permutation of `0..kept`, inside the reserved room.
                unsafe {
                    let slot = next.add((click.user as u32).wrapping_sub(base) as usize);
                    out.add(*slot as usize).write(Click { ts: click.ts, campaign: click.campaign, id: click.click });
                    *slot += 1;
                }
            }
        });
        // SAFETY: every position in `0..kept` was written once above.
        unsafe { grouped.set_len(kept as usize) };
    }

    let Scratch { starts, days, revenue, credits, order, sorted, .. } = scratch;
    let organic = ctx.organic();
    let gap = GAP as u32;
    // After the scatters `ends[p]` is one past person `p`'s last event, so its first is the
    // end of the person before.
    let (mut first, mut first_click, mut next_pay) = (0usize, 0usize, 0usize);
    for at in 0..span {
        let (end, end_click) = (ends[at] as usize, click_ends[at] as usize);
        let (from, from_click) = (first, first_click);
        (first, first_click) = (end, end_click);
        if end == from {
            continue;
        }
        let person = Id::from(base) + at as Id;
        let times = &mut ts[from..end];
        times.sort_unstable();
        let high = (at as u64) << 32;
        starts.clear();
        days.clear();
        revenue.clear();
        let mut event = 0;
        while event < times.len() {
            let (begin, start) = (event, times[event]);
            event += 1;
            while event < times.len() && times[event] - times[event - 1] <= gap {
                event += 1;
            }
            let d = (ctx.day_of(start) - ctx.day0) as usize;
            day.session(d, event - begin, person, ctx.day0, user_daily);
            // The session's purchases: those of this person up to its last event.
            let (last, mut paid) = (high | u64::from(times[event - 1]), 0u64);
            loop {
                // SAFETY: the sentinel is above every key, so the walk stops inside `side`.
                let (key, pay) = *unsafe { side.get_unchecked(next_pay) };
                if key > last {
                    break;
                }
                let price = pay & types::record::PAY_PRICE_MASK;
                day.purchase(price, (pay >> types::record::PAY_CATEGORY_SHIFT) as u8);
                paid += u64::from(price);
                next_pay += 1;
            }
            revenue.push(paid);
            starts.push(start);
            days.push(d as u32);
        }
        day.flush(person, ctx.day0, user_daily);
        credits.clear();
        credits.resize(starts.len(), organic);
        let clicks = &mut grouped[from_click..end_click];
        if !clicks.is_empty() {
            #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
            let done = match wide.as_mut() {
                Some(wide) => {
                    wide.person(clicks, starts, days, credits, counters.wide());
                    true
                }
                None => false,
            };
            #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd")))]
            let done = false;
            if !done {
                // A window of more than 16 days: session by session, over the sorted clicks.
                let clicks = sort_clicks(clicks, order, sorted);
                for ((&start, &d), credit) in starts.iter().zip(days.iter()).zip(credits.iter_mut()) {
                    *credit = attr.session(clicks, start, organic, counters.assists(d as usize));
                }
                attr.end_person();
            }
        }
        for ((&campaign, &d), &paid) in credits.iter().zip(days.iter()).zip(revenue.iter()) {
            counters.credit(campaign, d as usize, paid);
        }
    }
    debug_assert_eq!(first, n);
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
