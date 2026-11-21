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
}

impl Counters {
    pub fn new(ctx: &Ctx) -> Counters {
        let cells = ctx.campaigns.len() * ctx.n_days;
        Counters { campaigns: ctx.campaigns.len(), credited: vec![Credited::default(); cells], assisted: vec![0; cells] }
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
    let cells = ctx.campaigns.len() * ctx.n_days;
    let mut sums = vec![[0i64; 4]; cells];
    for counters in workers {
        for ((sum, credited), &assisted) in sums.iter_mut().zip(&counters.credited).zip(&counters.assisted) {
            sum[0] += i64::from(credited.sessions);
            sum[1] += i64::from(credited.purchasing);
            sum[2] += credited.revenue as i64;
            sum[3] += i64::from(assisted);
        }
    }
    let mut out = CampaignDaily::default();
    for (cell, &[sessions, purchasing, revenue, assisted]) in sums.iter().enumerate() {
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
