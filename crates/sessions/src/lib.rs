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
#[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
mod attr_wide;
#[cfg(feature = "attr-sweep")]
mod attr_sweep;
#[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq", target_feature = "avx512vl", target_feature = "avx512cd", target_feature = "bmi2"))]
mod dense_walk;
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
#[cfg(feature = "sess-partitioned")]
mod sort_times;
#[cfg(test)]
mod tests;

use types::record::{ClickN, ClickRec, PersonEvent};
use types::rows::{CampaignDaily, UserDaily};
use types::{Ctx, Id};

/// One click as the attribution reads it; a person's clicks sit together in the bucket.
#[derive(Clone, Copy, Default)]
pub struct Click {
    pub ts: u32,
    pub campaign: u32,
    pub id: Id,
}

/// Set once a tie between two clicks could not be told apart: see `ambiguous_tie`.
static AMBIGUOUS_TIE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The byte order of the IDs of two clicks of one person and one `ts`, where the smaller
/// `click_id` decides. Every place such a tie is broken asks here (the classic click sort,
/// whose order `attr-sweep` and `attr-interval` read, and `attr-wide`'s two kernels), and
/// `ambiguous_tie` says whether any answer was one the IDs as kept cannot give.
#[inline(always)]
pub(crate) fn id_order(a: &Click, b: &Click) -> std::cmp::Ordering {
    if (a.id | b.id) & types::id::SLOW == 0 && a.id != b.id {
        return a.id.cmp(&b.id);
    }
    open_order(a, b)
}

/// `id_order` for two IDs that compare equal, or with a slow-path ID among them. A long
/// `click_id` may be kept by its first 9 bytes (`ingest::opaque::click_id_at`), so between
/// two campaigns the order is open in two cases: the IDs compare equal, or one is 9 bytes
/// that an ID kept whole begins with, which `less` puts first whatever followed them.
/// Copies of one click (the same campaign) are no matter.
#[cold]
#[inline(never)]
fn open_order(a: &Click, b: &Click) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    use types::id::{begins, less};
    let order = if less(a.id, b.id) {
        Ordering::Less
    } else if less(b.id, a.id) {
        Ordering::Greater
    } else {
        Ordering::Equal
    };
    if a.campaign != b.campaign && (order == Ordering::Equal || begins(a.id, b.id) || begins(b.id, a.id)) {
        AMBIGUOUS_TIE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    order
}

/// Whether any tie so far was between two clicks of different campaigns whose IDs as kept
/// leave their order open (`open_order`): the credit of some session then rests on an order
/// the IDs as kept cannot give (a long `click_id` kept by its first bytes), and the caller
/// should run again with the IDs whole.
pub fn ambiguous_tie() -> bool {
    AMBIGUOUS_TIE.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn reset_tie() {
    AMBIGUOUS_TIE.store(false, std::sync::atomic::Ordering::Relaxed);
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
    /// The fast lane's (`attr-wide`), where the build has it and the window is at most 16
    /// days; `SS_ATTR=wide|interval`.
    #[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
    wide: Option<attr_wide::Attr>,
    day: agg_dense::Day,
    counters: agg_dense::Counters,
    /// `bucket_dense`'s buffers.
    dense: partitioned::Dense,
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
            #[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
            wide: (ctx.n_days <= attr_wide::DAYS && types::env::pick("SS_ATTR", &["wide", "interval"]) == 0).then(|| attr_wide::Attr::new(ctx.campaigns.len())),
            day: Default::default(),
            counters: agg_dense::Counters::new(ctx),
            dense: Default::default(),
        }
    }
}

#[cfg(feature = "pipe-positional")]
impl Agg {
    /// Credits one person's sessions and counts their assists (`attr-interval`): session `i`
    /// starts at `starts[i]` on day `days[i]` (an index into the days), in time order, and
    /// `credits[i]` becomes its campaign. `clicks` are the person's, in any order.
    pub fn attribute(&mut self, clicks: &mut [Click], starts: &[u32], days: &[u32], organic: u32, credits: &mut [u32]) {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
        if let Some(wide) = &mut self.wide {
            credits.fill(organic);
            return wide.person(clicks, starts, days, credits, self.counters.wide());
        }
        let Agg { bucket: scratch, stretch: attr, counters, .. } = self;
        let clicks = partitioned::sort_person_clicks(scratch, clicks);
        for ((&start, &day), credit) in starts.iter().zip(days).zip(credits.iter_mut()) {
            *credit = attr.session(clicks, start, day as usize, organic, counters.assisted());
        }
        attr.end_person(counters.assisted());
    }

    /// One session credited to `campaign` on day `day`, with its revenue.
    #[inline(always)]
    pub fn credit(&mut self, campaign: u32, day: usize, revenue: u64) {
        self.counters.credit(campaign, day, revenue);
    }

    /// Credited sessions counted apart: a table `[campaign * 16 + day]` to add one to per
    /// session, with a row past the campaigns (`Ctx::campaigns`' length) for sessions that
    /// are not to be counted; each such session with revenue then goes to `paid`. `None` for
    /// a window of more than 16 days: `credit` then.
    #[inline(always)]
    pub fn counted(&mut self) -> Option<&mut [u32]> {
        let counted = self.counters.counted();
        (!counted.is_empty()).then_some(counted)
    }

    /// The revenue of a session counted through `counted`.
    #[inline(always)]
    pub fn paid(&mut self, campaign: u32, day: usize, revenue: u64) {
        self.counters.paid(campaign, day, revenue);
    }
}

/// A gap between two of a person's events over this many ms starts a session.
#[cfg(feature = "pipe-positional")]
pub const SESSION_GAP: u32 = partitioned::GAP as u32;

/// Processes one person bucket: `events` and `clicks` are the workers' vectors of that bucket,
/// so every row of a person is in them. Appends the bucket's `user_daily` rows to `agg` and
/// adds its sessions to `agg`'s campaign counters. Called concurrently, one `agg` per worker.
pub fn bucket<'a>(
    ctx: &Ctx,
    events: impl Iterator<Item = &'a [PersonEvent]> + Clone,
    clicks: impl Iterator<Item = &'a [ClickRec]> + Clone,
    agg: &mut Agg,
) {
    // `SS_CLASSIC_ATTR=wide|sweep`: a person's sessions credited together by `attr-wide` where
    // the build and the window allow it, or session by session by `attr-sweep`.
    #[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512cd"))]
    {
        static WIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *WIDE.get_or_init(|| types::env::pick("SS_CLASSIC_ATTR", &["wide", "sweep"]) == 0) && agg.wide.is_some() {
            partitioned::bucket_wide(ctx, events, clicks, agg);
            return;
        }
    }
    partitioned::bucket::<false>(ctx, events, clicks, agg)
}

/// A purchase as `bucket_dense` reads it: its event's key (`person << 32 | ts`) and its `pay`
/// (`PersonEvent::pay`). 16 bytes.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct DensePay {
    pub key: u64,
    pub pay: u32,
    pub pad: u32,
}

/// One bucket of persons that are numbers: every person of the bucket is in
/// `base..base + span`, so what `bucket` keeps per person in a hash table is an array here,
/// read at `person - base`.
///
/// - `events` calls its argument with the bucket's surviving events, a piece at a time, each
///   the key `person << 32 | ts`; it is called twice, with the same pieces.
/// - `pays` does the same with the purchases among those events, once.
/// - `clicks` does the same with the bucket's clicks, twice; a click's `user` is its person.
/// - `placed` does the same with the bucket's clicks that are `ClickN`s, twice: those the
///   parse placed itself, a click's person being its place among the bucket's.
///
/// The rows are `bucket`'s, with the person's number as the row's `person`.
#[cfg(feature = "pipe-positional")]
pub fn bucket_dense(
    ctx: &Ctx,
    base: u32,
    span: usize,
    events: &dyn Fn(&mut dyn FnMut(&[u64])),
    pays: &dyn Fn(&mut dyn FnMut(&[DensePay])),
    clicks: &dyn Fn(&mut dyn FnMut(&[ClickRec])),
    placed: &dyn Fn(&mut dyn FnMut(&[ClickN])),
    agg: &mut Agg,
) {
    partitioned::bucket_dense(ctx, base, span, events, pays, clicks, placed, agg)
}

/// `bucket` with the assists counted per stretch of a person's sessions (`attr-interval`): for
/// the positional pipeline's classic lane. Its persons include a few with tens of thousands of
/// events and as many clicks, and counted per session and campaign their assists made a
/// bucket of one such person the longest task of the lane (8 ms of a 290 ms run at 20M).
#[cfg(feature = "pipe-positional")]
pub fn bucket_stretch<'a>(
    ctx: &Ctx,
    events: impl Iterator<Item = &'a [PersonEvent]> + Clone,
    clicks: impl Iterator<Item = &'a [ClickRec]> + Clone,
    agg: &mut Agg,
) {
    partitioned::bucket::<true>(ctx, events, clicks, agg)
}

/// Sums the workers' campaign counters into the `campaign_daily` rows: one per (campaign, day)
/// with at least one credited or assisted session.
pub fn campaign_daily(ctx: &Ctx, aggs: &[Agg]) -> CampaignDaily {
    agg_dense::campaign_daily(ctx, aggs.iter().map(|agg| &agg.counters))
}

/// Part `part` of `parts` of `campaign_daily`: the parts together are the table, and each
/// reads every worker's counters, so they may be made side by side.
pub fn campaign_daily_part(ctx: &Ctx, aggs: &[Agg], part: usize, parts: usize) -> CampaignDaily {
    agg_dense::campaign_daily_part(ctx, aggs.iter().map(|agg| &agg.counters), part, parts)
}
