//! Slot `agg-dense`: both tables' counters (decision: Aggregation). `user_daily` streams out a
//! person-day at a time, since a person's session days only grow; `campaign_daily` comes from
//! each worker's dense cells, `[campaigns + organic][n_days]` laid out day-major, summed at
//! the end.
//!
//! Contract, kept by every alternative:
//! - `Day`: for each of a person's sessions in order, `session(day, events, person, day0, out)`
//!   (it writes the person's previous day when `day` differs), then `purchase(price, category)`
//!   for each of the session's purchases; `flush(person, day0, out)` after the person's last;
//! - `Counters::new(ctx)`; `credit(campaign, day, revenue)` once per session, and for its
//!   assists `assists(day)`, called once per assisting campaign; `day` indexes `0..n_days`;
//! - `campaign_daily(ctx, every worker's counters)`.

use types::rows::{CampaignDaily, NO_CATEGORY, UserDaily};
use types::{Ctx, Id};

/// The person-day being summed.
pub struct Day {
    day: usize,
    /// 0 when nothing is summed yet.
    sessions: i64,
    events: i64,
    revenue: i64,
    /// Summed price per category index, and the categories with a sum.
    by_category: Box<[i64; 256]>,
    touched: Vec<u8>,
}

impl Default for Day {
    fn default() -> Day {
        Day { day: 0, sessions: 0, events: 0, revenue: 0, by_category: Box::new([0; 256]), touched: Vec::new() }
    }
}

impl Day {
    #[inline(always)]
    pub fn session(&mut self, day: usize, events: usize, person: Id, day0: i32, out: &mut UserDaily) {
        if day != self.day {
            self.flush(person, day0, out);
            self.day = day;
        }
        self.sessions += 1;
        self.events += events as i64;
    }

    #[inline(always)]
    pub fn purchase(&mut self, price: u32, category: u8) {
        let sum = &mut self.by_category[category as usize];
        if *sum == 0 {
            self.touched.push(category);
        }
        *sum += i64::from(price);
        self.revenue += i64::from(price);
    }

    pub fn flush(&mut self, person: Id, day0: i32, out: &mut UserDaily) {
        if self.sessions == 0 {
            return;
        }
        let (mut top, mut best) = (NO_CATEGORY, 0);
        for &category in &self.touched {
            let sum = std::mem::take(&mut self.by_category[category as usize]);
            // Categories are numbered in byte order, so a tie goes to the smaller index.
            if sum > best || (sum == best && category < top) {
                (top, best) = (category, sum);
            }
        }
        self.touched.clear();
        out.person.push(person);
        out.day.push(day0 + self.day as i32);
        out.sessions.push(self.sessions);
        out.events.push(self.events);
        out.revenue_cents.push(self.revenue);
        out.top_category.push(top);
        (self.sessions, self.events, self.revenue) = (0, 0, 0);
    }
}

/// One cell's credited sessions; 16 bytes, so a credit touches one cache line.
#[derive(Clone, Copy, Default)]
struct Credited {
    sessions: u32,
    purchasing: u32,
    revenue: u64,
}

pub struct Counters {
    campaigns: usize,
    /// `[day * campaigns + campaign]`: a session's assists all fall in one day's row. Assists
    /// apart from credits, since a session writes several of them.
    credited: Vec<Credited>,
    assisted: Vec<u32>,
    /// More assists, `[campaign * 16 + day]`: `attr-wide`'s, which adds a campaign's days at
    /// once. Empty for a window of more than 16 days, where that slot does not run.
    wide: Vec<u32>,
}

/// Days a campaign has in `Counters::wide`.
const WIDE_DAYS: usize = 16;

impl Counters {
    pub fn new(ctx: &Ctx) -> Counters {
        let cells = ctx.campaigns.len() * ctx.n_days;
        let wide = if ctx.n_days <= WIDE_DAYS { ctx.campaigns.len() * WIDE_DAYS } else { 0 };
        Counters { campaigns: ctx.campaigns.len(), credited: vec![Credited::default(); cells], assisted: vec![0; cells], wide: vec![0; wide] }
    }

    /// The table `attr-wide` adds to, `[campaign * 16 + day]`.
    #[cfg(feature = "pipe-positional")]
    #[inline(always)]
    pub(crate) fn wide(&mut self) -> &mut [u32] {
        &mut self.wide
    }

    #[inline(always)]
    pub fn credit(&mut self, campaign: u32, day: usize, revenue: u64) {
        let cell = &mut self.credited[day * self.campaigns + campaign as usize];
        cell.sessions += 1;
        cell.purchasing += u32::from(revenue > 0);
        cell.revenue += revenue;
    }

    /// The assists table, `[day * campaigns + campaign]`, for `attr-interval` to add to.
    #[cfg(feature = "pipe-positional")]
    #[inline(always)]
    pub(crate) fn assisted(&mut self) -> &mut [u32] {
        &mut self.assisted
    }

    /// Counts one assist of `campaign` on `day` per call.
    #[inline(always)]
    pub fn assists(&mut self, day: usize) -> impl FnMut(u32) + '_ {
        let row = &mut self.assisted[day * self.campaigns..][..self.campaigns];
        move |campaign| row[campaign as usize] += 1
    }
}

pub fn campaign_daily<'a>(ctx: &Ctx, workers: impl Iterator<Item = &'a Counters>) -> CampaignDaily {
    campaign_daily_part(ctx, workers, 0, 1)
}

/// Part `part` of `parts` of `campaign_daily`: the rows of an even share of the cells, so the
/// parts together are the table. A part is summed and written by one worker while the others
/// do theirs: summed whole by one thread, the table was a millisecond at the end of a run of
/// twenty at 1M events (4.5 MB of counters read cold, a megabyte of sums on fresh pages).
pub fn campaign_daily_part<'a>(ctx: &Ctx, workers: impl Iterator<Item = &'a Counters>, part: usize, parts: usize) -> CampaignDaily {
    let cells = ctx.campaigns.len() * ctx.n_days;
    let (from, to) = (cells * part / parts, cells * (part + 1) / parts);
    let mut sums = vec![[0i64; 4]; to - from];
    for counters in workers {
        for ((sum, credited), &assisted) in sums.iter_mut().zip(&counters.credited[from..to]).zip(&counters.assisted[from..to]) {
            sum[0] += i64::from(credited.sessions);
            sum[1] += i64::from(credited.purchasing);
            sum[2] += credited.revenue as i64;
            sum[3] += i64::from(assisted);
        }
        // The other table's assists, cell by cell: a cell is `day * campaigns + campaign`.
        if !counters.wide.is_empty() {
            for (sum, cell) in sums.iter_mut().zip(from..to) {
                let (day, campaign) = (cell / ctx.campaigns.len(), cell % ctx.campaigns.len());
                sum[3] += i64::from(counters.wide[campaign * WIDE_DAYS + day]);
            }
        }
    }
    let mut out = CampaignDaily::default();
    let rows = sums.iter().filter(|sum| sum[0] > 0 || sum[3] > 0).count();
    out.campaign.reserve_exact(rows);
    out.day.reserve_exact(rows);
    out.sessions.reserve_exact(rows);
    out.purchasing_sessions.reserve_exact(rows);
    out.revenue_cents.reserve_exact(rows);
    out.assisted_sessions.reserve_exact(rows);
    for (cell, &[sessions, purchasing, revenue, assisted]) in (from..to).zip(&sums) {
        if sessions > 0 || assisted > 0 {
            out.campaign.push((cell % ctx.campaigns.len()) as u32);
            out.day.push(ctx.day0 + (cell / ctx.campaigns.len()) as i32);
            out.sessions.push(sessions);
            out.purchasing_sessions.push(purchasing);
            out.revenue_cents.push(revenue);
            out.assisted_sessions.push(assisted);
        }
    }
    out
}
