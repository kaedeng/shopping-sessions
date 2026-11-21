//! One person bucket at a time: sort, sessionize, attribute, aggregate (decisions: Sessionize,
//! Attribution, Aggregation).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's):
//!
//! Each slot is one module behind its feature, its own contract at the top of its file:
//! `sess-partitioned` (the bucket pipeline), `sess-purchase-side` (where a purchase's price
//! and category ride through the sort), `sort-std` (the key sort), `attr-sweep` (credit and
//! assists), `agg-dense` (both tables' counters). `implementation.md` has the choices inside them.

#[cfg(feature = "agg-dense")]
mod agg_dense;
#[cfg(feature = "attr-sweep")]
mod attr_sweep;
#[cfg(feature = "sess-partitioned")]
mod partitioned;
#[cfg(feature = "sess-purchase-side")]
mod purchase_side;
#[cfg(feature = "sort-std")]
mod sort_std;
#[cfg(test)]
mod tests;

use types::record::{ClickRec, PersonEvent};
use types::rows::{CampaignDaily, UserDaily};
use types::{Ctx, Id};

/// One click as the attribution reads it; a person's clicks sit together in the bucket.
#[derive(Clone, Copy, Default)]
struct Click {
    ts: u32,
    campaign: u32,
    id: Id,
}

/// One worker's results and reusable buffers.
pub struct Agg {
    /// This worker's `user_daily` rows, one per person and day with a session.
    pub user_daily: UserDaily,
    bucket: partitioned::Scratch,
    rows: purchase_side::Rows,
    attr: attr_sweep::Attr,
    day: agg_dense::Day,
    counters: agg_dense::Counters,
}

impl Agg {
    pub fn new(ctx: &Ctx) -> Agg {
        Agg {
            user_daily: UserDaily::default(),
            bucket: Default::default(),
            rows: Default::default(),
            attr: attr_sweep::Attr::new(ctx),
            day: Default::default(),
            counters: agg_dense::Counters::new(ctx),
        }
    }
}

/// Processes one person bucket: `events` and `clicks` are the workers' vectors of that bucket,
/// so every row of a person is in them. Appends the bucket's `user_daily` rows to `agg` and
/// adds its sessions to `agg`'s campaign counters. Called concurrently, one `agg` per worker.
pub fn bucket<'a>(
    ctx: &Ctx,
    events: impl Iterator<Item = &'a [PersonEvent]> + Clone,
    clicks: impl Iterator<Item = &'a [ClickRec]> + Clone,
    agg: &mut Agg,
) {
    partitioned::bucket(ctx, events, clicks, agg)
}

/// Sums the workers' campaign counters into the `campaign_daily` rows: one per (campaign, day)
/// with at least one credited or assisted session.
pub fn campaign_daily(ctx: &Ctx, aggs: &[Agg]) -> CampaignDaily {
    agg_dense::campaign_daily(ctx, aggs.iter().map(|agg| &agg.counters))
}
