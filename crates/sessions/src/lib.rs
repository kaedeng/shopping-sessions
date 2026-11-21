//! One person bucket at a time: sort, sessionize, attribute, aggregate (decisions: Sessionize,
//! Attribution, Aggregation).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's):
//!
//! Each slot is one module behind its feature, its own contract at the top of its file:
//! `sess-partitioned` (the bucket pipeline), `sess-purchase-side` (where a purchase's price
//! and category ride through the sort), `sort-radix`, `sort-grouped` and `sort-std` (the key sort, `SS_SORT`),
//! `attr-sweep` (credit and assists), `agg-dense` (both tables' counters). `implementation.md` has
//! the choices inside them.

#[cfg(feature = "agg-dense")]
mod agg_dense;
#[cfg(feature = "pipe-positional")]
mod attr_interval;
#[cfg(feature = "attr-sweep")]
mod attr_sweep;
#[cfg(feature = "sess-partitioned")]
mod partitioned;
#[cfg(feature = "pipe-positional")]
pub mod positional;
#[cfg(feature = "sess-purchase-side")]
mod purchase_side;
#[cfg(feature = "sort-grouped")]
mod sort_grouped;
#[cfg(feature = "sort-radix")]
mod sort_radix;
#[cfg(feature = "sort-std")]
mod sort_std;
#[cfg(test)]
mod tests;

use types::record::{ClickRec, PersonEvent};
use types::rows::{CampaignDaily, UserDaily};
use types::{Ctx, Id};

/// One click as the attribution reads it; a person's clicks sit together in the bucket.
#[derive(Clone, Copy, Default)]
pub struct Click {
    pub ts: u32,
    pub campaign: u32,
    pub id: Id,
}

#[cfg(not(any(feature = "sort-radix", feature = "sort-grouped", feature = "sort-std")))]
compile_error!("sessions needs a key sort: enable `sort-radix`, `sort-grouped` or `sort-std`");

/// A key sort's spare buffers, one set per worker: any length on entry, left in any state.
#[derive(Default)]
pub(crate) struct SortScratch {
    keys: Vec<u64>,
    /// `sort-grouped`: where each group's keys start. `sort-radix`: every digit's counts.
    starts: Vec<u32>,
}

/// The sort slot's contract: `sort(keys, key_bits, low_bits, scratch)` leaves `keys` ascending.
/// Every key is below `1 << key_bits`, and its bits from `low_bits` up are a dense group number
/// (the bucket-local person), which an implementation may use. `keys` is a `Vec` so that one
/// may swap it with a scratch buffer instead of copying back.
type SortFn = fn(&mut Vec<u64>, u32, u32, &mut SortScratch);

/// The key sort `SS_SORT` picks among those compiled in, read once.
fn key_sort() -> SortFn {
    static PICK: std::sync::OnceLock<SortFn> = std::sync::OnceLock::new();
    *PICK.get_or_init(|| {
        let options: &[(&str, SortFn)] = &[
            #[cfg(feature = "sort-grouped")]
            ("grouped", sort_grouped::sort),
            #[cfg(feature = "sort-std")]
            ("std", sort_std::sort),
            #[cfg(feature = "sort-radix")]
            ("radix", sort_radix::sort),
        ];
        let names: Vec<&str> = options.iter().map(|option| option.0).collect();
        options[types::env::pick("SS_SORT", &names)].1
    })
}

/// One worker's results and reusable buffers.
pub struct Agg {
    /// This worker's `user_daily` rows, one per person and day with a session.
    pub user_daily: UserDaily,
    bucket: partitioned::Scratch,
    rows: purchase_side::Rows,
    attr: attr_sweep::Attr,
    /// The positional pipeline's attribution (`attr-interval`).
    #[cfg(feature = "pipe-positional")]
    stretch: attr_interval::Attr,
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
            #[cfg(feature = "pipe-positional")]
            stretch: attr_interval::Attr::new(ctx),
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
