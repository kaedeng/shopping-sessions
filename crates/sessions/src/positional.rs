//! Slot `pipe-positional`: one person whose events already sit in time order (the positional
//! pipeline, `src/positional.rs`): no table, no keys, no sort. The sessions, the purchases and
//! the credit are the same calls, in the same order, as `partitioned::bucket` makes for a
//! person; the assists are counted per stretch (`attr-interval`), to the same totals.

use types::record::{PAY_CATEGORY_SHIFT, PAY_PRICE_MASK};
use types::{Ctx, Id};

use crate::partitioned::{GAP, sort_person_clicks};
use crate::{Agg, Click};

/// Sessionizes, attributes and aggregates one person.
///
/// - `times`: the person's stretch of the event table, one slot per event number from `first`
///   on: a `ts`, or 0 where the number has no surviving event. The first slot is an event, and
///   the events are in non-decreasing `ts` order.
/// - `pays`: every surviving purchase as `(event number, pay)`, ascending; `next_pay` is the
///   first at or after `first`, and is left at the first past the stretch.
/// - `clicks`: the person's clicks, in any order (sorted here).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn person(
    ctx: &Ctx,
    name: Id,
    times: &[u32],
    first: u32,
    pays: &[(u32, u32)],
    next_pay: &mut usize,
    clicks: &mut [Click],
    agg: &mut Agg,
) {
    let Agg { user_daily, bucket: scratch, stretch: attr, day, counters, .. } = agg;
    let clicks = sort_person_clicks(scratch, clicks);
    let organic = ctx.organic();
    let (n, mut at) = (times.len(), 0);
    debug_assert!(n > 0 && times[0] != 0);
    while at < n {
        let start = times[at];
        let (mut last, mut events) = (start, 1usize);
        at += 1;
        // Stops on the next session's first event, or past the stretch.
        while at < n {
            let ts = times[at];
            if ts != 0 {
                debug_assert!(ts >= last);
                if u64::from(ts - last) > GAP {
                    break;
                }
                (last, events) = (ts, events + 1);
            }
            at += 1;
        }
        let d = (ctx.day_of(start) - ctx.day0) as usize;
        day.session(d, events, name, ctx.day0, user_daily);
        // Event numbers below `end` belong to this session or an earlier one.
        let (end, mut revenue) = (first + at as u32, 0u64);
        while let Some(&(number, pay)) = pays.get(*next_pay)
            && number < end
        {
            let price = pay & PAY_PRICE_MASK;
            day.purchase(price, (pay >> PAY_CATEGORY_SHIFT) as u8);
            revenue += u64::from(price);
            *next_pay += 1;
        }
        let credit = attr.session(clicks, start, d, organic, counters.assisted());
        counters.credit(credit, d, revenue);
    }
    attr.end_person(counters.assisted());
    day.flush(name, ctx.day0, user_daily);
}

/// The same for a person whose events were gathered into `(ts, pay)` pairs, ascending by `ts`:
/// a person with events outside the table.
pub fn person_pairs(ctx: &Ctx, name: Id, events: &[(u32, u32)], clicks: &mut [Click], agg: &mut Agg) {
    let Agg { user_daily, bucket: scratch, stretch: attr, day, counters, .. } = agg;
    let clicks = sort_person_clicks(scratch, clicks);
    let organic = ctx.organic();
    let (n, mut at) = (events.len(), 0);
    while at < n {
        let (first, start) = (at, events[at].0);
        at += 1;
        while at < n && u64::from(events[at].0 - events[at - 1].0) <= GAP {
            at += 1;
        }
        let d = (ctx.day_of(start) - ctx.day0) as usize;
        day.session(d, at - first, name, ctx.day0, user_daily);
        let mut revenue = 0u64;
        for &(_, pay) in &events[first..at] {
            if pay != 0 {
                let price = pay & PAY_PRICE_MASK;
                day.purchase(price, (pay >> PAY_CATEGORY_SHIFT) as u8);
                revenue += u64::from(price);
            }
        }
        let credit = attr.session(clicks, start, d, organic, counters.assisted());
        counters.credit(credit, d, revenue);
    }
    attr.end_person(counters.assisted());
    day.flush(name, ctx.day0, user_daily);
}
