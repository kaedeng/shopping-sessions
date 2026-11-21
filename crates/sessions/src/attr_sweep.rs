//! Slot `attr-sweep`: credit and assists for one person's sessions (decision: Attribution).
//! Two pointers keep the window `[start - 7 days, start]`, both ends inclusive, over the
//! person's clicks; a count per campaign and a sparse set (Briggs and Torczon) of the
//! campaigns whose count is above zero give the assists, updated as clicks enter and leave.
//!
//! Contract, kept by every alternative (`attr-stamp`):
//! - `Attr::new(ctx)`;
//! - `session(clicks, start, organic, assist)` for each of a person's sessions, `start`
//!   strictly increasing, with the same `clicks` each time: the person's, sorted by `ts`, the
//!   smallest `click_id` last among equal `ts`. Calls `assist(campaign)` once for every
//!   campaign with a click in the window, and returns the credited campaign: the window's
//!   last click's, or `organic` for an empty window;
//! - `end_person()` after a person's last session.

use types::Ctx;
use types::time::WEEK;

use crate::Click;

pub struct Attr {
    /// Clicks in the window, per campaign.
    count: Vec<u32>,
    /// The campaigns with a count above zero, and each one's position in it.
    active: Vec<u32>,
    at: Vec<u32>,
    /// The window is `clicks[lo..hi]`.
    lo: usize,
    hi: usize,
}

impl Attr {
    pub fn new(ctx: &Ctx) -> Attr {
        let campaigns = ctx.campaigns.len();
        Attr { count: vec![0; campaigns], active: Vec::with_capacity(campaigns), at: vec![0; campaigns], lo: 0, hi: 0 }
    }

    #[inline(always)]
    pub fn session(&mut self, clicks: &[Click], start: u32, organic: u32, mut assist: impl FnMut(u32)) -> u32 {
        // Locals, so the window and the arrays stay in registers across the pushes.
        let Attr { count, active, at, lo, hi } = self;
        let (count, at, (mut lo, mut hi)) = (&mut count[..], &mut at[..], (*lo, *hi));
        while hi < clicks.len() && clicks[hi].ts <= start {
            let campaign = clicks[hi].campaign as usize;
            count[campaign] += 1;
            if count[campaign] == 1 {
                at[campaign] = active.len() as u32;
                active.push(campaign as u32);
            }
            hi += 1;
        }
        // `start >= event_lo = WEEK`, so this cannot wrap.
        let from = start - WEEK as u32;
        while lo < hi && clicks[lo].ts < from {
            let campaign = clicks[lo].campaign as usize;
            count[campaign] -= 1;
            if count[campaign] == 0 {
                let place = at[campaign] as usize;
                active.swap_remove(place);
                if let Some(&moved) = active.get(place) {
                    at[moved as usize] = place as u32;
                }
            }
            lo += 1;
        }
        (self.lo, self.hi) = (lo, hi);
        for &campaign in &self.active {
            assist(campaign);
        }
        // Every click of the last `ts` is in the window, and the sort put the winner last.
        if lo < hi { clicks[hi - 1].campaign } else { organic }
    }

    #[inline(always)]
    pub fn end_person(&mut self) {
        for &campaign in &self.active {
            self.count[campaign as usize] = 0;
        }
        self.active.clear();
        (self.lo, self.hi) = (0, 0);
    }
}
