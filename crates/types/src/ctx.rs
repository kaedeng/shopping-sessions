//! Everything read before the parse (decision "Inputs first"), and the run's shape.

use crate::env;
use crate::id::{self, Id};
use crate::rows::NO_CATEGORY;
use crate::table::IdMap;
use crate::time::{DAY, SECOND, WEEK};

pub struct Ctx {
    /// Epoch milliseconds. An event's `ts` must be in `[window_start, window_end)`.
    pub window_start: i64,
    pub window_end: i64,
    /// `window_start` - 7 days: the first valid click instant, and the zero of every `u32` ts.
    pub origin: i64,
    /// `window_start - origin`: the smallest valid event ts.
    pub event_lo: u32,
    /// `window_end - origin`: one past the largest valid ts, events and clicks alike.
    pub ts_end: u32,
    /// Bits that hold any valid ts; a sort key is `person << ts_bits | ts`.
    pub ts_bits: u32,
    /// Day number (days since 1970) of `window_start`, and the days a session can start on.
    pub day0: i32,
    pub n_days: usize,

    /// Threads, from `NCPU` (the container's CPU quota), 8 when unset.
    pub workers: usize,
    /// Event dedup partitions = `1 << part_bits` (`SS_PART_BITS`, default 10).
    pub part_bits: u32,
    /// Person buckets = `1 << bucket_bits` (`SS_BUCKET_BITS`, default 10).
    pub bucket_bits: u32,

    /// Category names in byte order, `unknown` among them: a larger index is a larger name.
    pub categories: Vec<Vec<u8>>,
    /// The index of `unknown`, for a product not in `products.csv`.
    pub unknown: u8,
    /// product_id -> category index.
    pub products: IdMap<u8>,
    /// `(campaign_id, channel)` in file order, then `organic` last.
    pub campaigns: Vec<(Vec<u8>, Vec<u8>)>,
    /// campaign_id -> index into `campaigns`. `organic` is not in it.
    pub campaign_index: IdMap<u32>,

    origin_day: i64,
    origin_in_day: u64,
    ingest_base: i64,
}

impl Ctx {
    /// `products`: `(product_id, category)` rows; `campaigns`: `(campaign_id, channel)` rows.
    pub fn new(
        window_start: i64,
        window_end: i64,
        products: &[(Id, &[u8])],
        campaigns: &[(&[u8], &[u8])],
    ) -> Result<Ctx, String> {
        let origin = window_start - WEEK;
        // shortcut: a ts is 32 bits of milliseconds from 7 days before the window, so the window
        // can be at most 42 days (the generator's is always 14); widen `ts` if that ever changes.
        if window_end <= window_start || window_end - origin > i64::from(u32::MAX) {
            return Err(format!("unsupported window: {window_start} to {window_end} ms"));
        }
        let ts_end = (window_end - origin) as u32;
        let day0 = window_start.div_euclid(DAY);

        let mut categories: Vec<&[u8]> = products.iter().map(|&(_, category)| category).collect();
        categories.push(b"unknown");
        categories.sort_unstable();
        categories.dedup();
        // shortcut: a category index shares 8 bits with the price in `pay` and 0xFF is null, so
        // at most 255 categories (the generator has 21 with `unknown`); widen `pay` beyond that.
        if categories.len() > NO_CATEGORY as usize {
            return Err(format!("{} categories, at most 255 supported", categories.len()));
        }
        let category_of = |name: &[u8]| categories.binary_search(&name).unwrap() as u8;
        let mut product_map = IdMap::new(products.len());
        for &(product, category) in products {
            product_map.insert(product, category_of(category));
        }

        let mut campaign_index = IdMap::new(campaigns.len());
        for (at, &(campaign, _)) in campaigns.iter().enumerate() {
            campaign_index.insert(id::pack(campaign), at as u32);
        }
        let mut campaigns: Vec<(Vec<u8>, Vec<u8>)> =
            campaigns.iter().map(|&(campaign, channel)| (campaign.to_vec(), channel.to_vec())).collect();
        campaigns.push((b"organic".to_vec(), b"organic".to_vec()));

        Ok(Ctx {
            window_start,
            window_end,
            origin,
            event_lo: WEEK as u32,
            ts_end,
            ts_bits: 32 - (ts_end - 1).leading_zeros(),
            day0: day0 as i32,
            n_days: ((window_end - 1).div_euclid(DAY) - day0 + 1) as usize,
            workers: env::number("NCPU", 8).max(1),
            part_bits: env::number("SS_PART_BITS", 10) as u32,
            bucket_bits: env::number("SS_BUCKET_BITS", 10) as u32,
            unknown: category_of(b"unknown"),
            categories: categories.iter().map(|name| name.to_vec()).collect(),
            products: product_map,
            campaigns,
            campaign_index,
            origin_day: origin.div_euclid(DAY),
            origin_in_day: origin.rem_euclid(DAY) as u64,
            ingest_base: origin.div_euclid(SECOND) - (1 << 30),
        })
    }

    /// The index of `organic` in `campaigns`: one past the real campaigns.
    #[inline(always)]
    pub fn organic(&self) -> u32 {
        self.campaigns.len() as u32 - 1
    }

    /// The UTC day (days since 1970) a ts falls on. `- day0` indexes `0..n_days`.
    #[inline(always)]
    pub fn day_of(&self, ts: u32) -> i32 {
        (self.origin_day + ((u64::from(ts) + self.origin_in_day) / DAY as u64) as i64) as i32
    }

    /// An `ingest_ts` in epoch seconds as `EventRec::ingest` holds it: order-preserving over
    /// origin - 34 years to origin + 102 years, clamped beyond.
    #[inline(always)]
    pub fn ingest_rel(&self, seconds: i64) -> u32 {
        (seconds - self.ingest_base).clamp(0, i64::from(u32::MAX)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_f1;

    #[test]
    fn window_shape_and_lookups() {
        let start = parse_f1(b"2026-10-01T00:00:00Z").unwrap();
        let end = parse_f1(b"2026-10-15T00:00:00Z").unwrap();
        let products = [(id::pack(b"p1"), b"shoes".as_slice()), (id::pack(b"p2"), b"books")];
        let campaigns = [(b"c_1".as_slice(), b"search".as_slice()), (b"c_2", b"social")];
        let ctx = Ctx::new(start, end, &products, &campaigns).unwrap();
        assert_eq!((ctx.event_lo, ctx.ts_end, ctx.ts_bits, ctx.n_days), (604_800_000, 1_814_400_000, 31, 14));
        assert_eq!(ctx.categories, [b"books".to_vec(), b"shoes".to_vec(), b"unknown".to_vec()]);
        assert_eq!((ctx.products.get(id::pack(b"p1")), ctx.products.get(id::pack(b"p9")), ctx.unknown), (Some(1), None, 2));
        assert_eq!((ctx.campaign_index.get(id::pack(b"c_2")), ctx.campaign_index.get(id::pack(b"organic")), ctx.organic()), (Some(1), None, 2));
        assert_eq!(ctx.day_of(ctx.event_lo) - ctx.day0, 0);
        assert_eq!(ctx.day_of(ctx.ts_end - 1) - ctx.day0, 13);
        // A window that starts mid-day covers one more calendar day than its length.
        let ctx = Ctx::new(start + DAY / 2, end + DAY / 2, &products, &campaigns).unwrap();
        assert_eq!((ctx.n_days, ctx.day_of(ctx.event_lo) - ctx.day0, ctx.day_of(ctx.ts_end - 1) - ctx.day0), (15, 0, 14));
        assert!(Ctx::new(start, start + 50 * DAY, &products, &campaigns).is_err());
    }
}
