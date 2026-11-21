//! Slot `attr-interval`: `attr-sweep` with each assist counted once per stretch, not once per
//! session. A campaign that stays in the window over several of a person's sessions on one day
//! assists each of them; instead of an increment per campaign per session, the campaign
//! remembers how many of the day's sessions had passed when it came in, and the difference is
//! added when it leaves the window, when the day changes and when the person ends. The counts
//! are the same as `attr-sweep`'s; at 20M events that one makes 36.9M increments, nearly all for
//! about 20,000 persons with many clicks (a quarter of the sessions stage on the x86 box).
//!
//! Used by the positional pipeline (`positional::person`); the classic bucket keeps `attr-sweep`.
//!
//! - `session(clicks, start, day, organic, assisted)` for each of a person's sessions, `start`
//!   strictly increasing and `day` never decreasing, with the same `clicks` each time: the
//!   person's, sorted by `ts`, the smallest `click_id` last among equal `ts`. Returns the
//!   credited campaign. `assisted` is the dense table `[day * campaigns + campaign]`;
//! - `end_person(assisted)` after a person's last session.

use types::Ctx;
use types::time::WEEK;

use crate::Click;

pub struct Attr {
    campaigns: usize,
    /// Clicks in the window, per campaign.
    count: Vec<u32>,
    /// The campaigns with a count above zero, and each one's position in it.
    active: Vec<u32>,
    at: Vec<u32>,
    /// Per active campaign: the day's sessions before it came into the window (0 for one that
    /// was in the window when the day began).
    since: Vec<u32>,
    /// The window is `clicks[lo..hi]`.
    lo: usize,
    hi: usize,
    /// The day of the last session, and the person's sessions on it so far.
    day: usize,
    sessions: u32,
}

impl Attr {
    pub fn new(ctx: &Ctx) -> Attr {
        let campaigns = ctx.campaigns.len();
        Attr {
            campaigns,
            count: vec![0; campaigns],
            active: Vec::with_capacity(campaigns),
            at: vec![0; campaigns],
            since: vec![0; campaigns],
            lo: 0,
            hi: 0,
            day: 0,
            sessions: 0,
        }
    }

    #[inline(always)]
    pub fn session(&mut self, clicks: &[Click], start: u32, day: usize, organic: u32, assisted: &mut [u32]) -> u32 {
        if day != self.day {
            self.close_day(assisted);
            self.day = day;
        }
        let Attr { campaigns, count, active, at, since, lo, hi, sessions, .. } = self;
        let (count, at, since, (mut lo, mut hi)) = (&mut count[..], &mut at[..], &mut since[..], (*lo, *hi));
        // The day's sessions before this one: a campaign entering now assists from this one on.
        let before = *sessions;
        while hi < clicks.len() && clicks[hi].ts <= start {
            let campaign = clicks[hi].campaign as usize;
            count[campaign] += 1;
            if count[campaign] == 1 {
                at[campaign] = active.len() as u32;
                active.push(campaign as u32);
                since[campaign] = before;
            }
            hi += 1;
        }
        // `start >= event_lo = WEEK`, so this cannot wrap.
        let from = start - WEEK as u32;
        while lo < hi && clicks[lo].ts < from {
            let campaign = clicks[lo].campaign as usize;
            count[campaign] -= 1;
            if count[campaign] == 0 {
                // Out before this session: it assisted the day's sessions since it came in.
                assisted[day * *campaigns + campaign] += before - since[campaign];
                let place = at[campaign] as usize;
                active.swap_remove(place);
                if let Some(&moved) = active.get(place) {
                    at[moved as usize] = place as u32;
                }
            }
            lo += 1;
        }
        (self.lo, self.hi, self.sessions) = (lo, hi, before + 1);
        // Every click of the last `ts` is in the window, and the sort put the winner last.
        if lo < hi { clicks[hi - 1].campaign } else { organic }
    }

    /// Adds what every campaign still in the window assisted on the day that ends.
    #[inline(always)]
    fn close_day(&mut self, assisted: &mut [u32]) {
        let row = self.day * self.campaigns;
        for &campaign in &self.active {
            assisted[row + campaign as usize] += self.sessions - self.since[campaign as usize];
            self.since[campaign as usize] = 0;
        }
        self.sessions = 0;
    }

    #[inline(always)]
    pub fn end_person(&mut self, assisted: &mut [u32]) {
        self.close_day(assisted);
        for &campaign in &self.active {
            self.count[campaign as usize] = 0;
        }
        self.active.clear();
        (self.lo, self.hi) = (0, 0);
    }
}
