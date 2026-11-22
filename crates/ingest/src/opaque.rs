//! The parse for IDs that are opaque strings (`src/opaque.rs` has the stages after it).
//!
//! Since generator v5 an ID says nothing but which thing it names: a keyed bijection's value,
//! written as a letter and up to 8 of `0-9a-z`, or, one in eight, as a letter and 32 hex
//! digits. So the parse keeps of each ID exactly what a later stage needs of it:
//!
//! - An `event_id` is only ever compared with other `event_id`s. A short one is its digits in
//!   a word (`types::users::short_key_at`), a long one the 128 bits of its hex digits, any
//!   other spelling an interned `Id`; the event's record goes to the list of its key's hash,
//!   short keys (`Ev`, 24 bytes) and long ones (`EvL`, 32 bytes) in lists of their own, since
//!   a long ID is never equal to a short one.
//! - A `user_id` is looked up in the run's directory (`types::users::Users`) as the line is
//!   read, and the record carries its place there: 4 bytes whatever the ID's length. The
//!   links are pairs of places.
//! - A `click_id` is needed for one thing, the byte order among a person's clicks of one
//!   `ts`: it is kept as an `Id` (`types::id`), which orders as the bytes do; of an ID
//!   spelled the long way only the first 9 bytes are kept (`click_id_at`), which order two
//!   such IDs as their bytes do unless those 9 bytes are the same.
//!
//! Every row is exact for any bytes: the kernel (`kern`) takes the lines spelled the
//! generator's way and sends every other line through the row code here, which reads any
//! spelling.

use std::fs::File;

use types::chunks::Chunks;
use types::hash::hash;
use types::id::{self, Id, pack_at};
use types::record::PAY_CATEGORY_SHIFT;
use types::time::DAY;
use types::users::{NO_USER, UserKey, Users, key_at_wide, long_key_at, mix_long, short_key_at};
use types::width::{Narrow, Wide, Width};
use types::Ctx;

use crate::tok::{Field, Mask, NarrowMask, Scalar, Scratch, Tok};
use crate::ts::{Dates, rd};
use crate::{Ingest, ingest_slow, price_cents};

#[cfg(all(
    feature = "tok-shape",
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512cd",
    target_feature = "avx512vl",
    target_feature = "bmi2",
    target_feature = "lzcnt"
))]
mod kern;

/// How lines are decoded, the pick first (`SS_DECODE`): by the kernel's tables where the
/// build has them, or every line through the row code.
#[cfg(all(
    feature = "tok-shape",
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512cd",
    target_feature = "avx512vl",
    target_feature = "bmi2",
    target_feature = "lzcnt"
))]
const DECODES: &[&str] = &["kernel", "row"];
#[cfg(not(all(
    feature = "tok-shape",
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512cd",
    target_feature = "avx512vl",
    target_feature = "bmi2",
    target_feature = "lzcnt"
)))]
const DECODES: &[&str] = &["row"];

/// One well-formed event line whose `event_id` is not spelled the long way. 24 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct Ev {
    /// The `event_id`: `short_key_at` of it, or the interned `Id` of its bytes (top bit set).
    pub key: u64,
    /// The user's place in the directory (`NO_USER` for none), and above it the `ts` in
    /// milliseconds after `Ctx::origin`, or 0 for a row that is not valid (a valid `ts` is at
    /// least `Ctx::event_lo`).
    pub user_ts: u64,
    /// `Ctx::ingest_rel` (the dedup keeps the smallest), and above it the `pay` of a purchase.
    pub ingest_pay: u64,
}

/// One well-formed event line whose `event_id` is spelled the long way. 32 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct EvL {
    /// `long_key_at` of the `event_id`.
    pub hi: u64,
    pub lo: u64,
    pub user_ts: u64,
    pub ingest_pay: u64,
}

/// One valid click. 24 bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct Ck {
    /// The `click_id` (`click_id_at`).
    pub click: Id,
    /// The user's place in the directory, and above it the `ts`.
    pub user_ts: u64,
    /// Index into `Ctx::campaigns`.
    pub campaign: u64,
}

/// One link: the two users' places, and each ID's first 8 bytes as a number (the first byte
/// highest, zeros after a shorter ID), which orders two IDs as their bytes do unless the
/// numbers are equal.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct LinkRec {
    pub users: [u32; 2],
    pub order: [u64; 2],
}

const _: () = assert!(size_of::<Ev>() == 24 && size_of::<EvL>() == 32 && size_of::<Ck>() == 24 && size_of::<LinkRec>() == 24);

/// The first 8 bytes of the `len` bytes at `p` as `LinkRec::order` has them.
///
/// # Safety
/// 8 bytes readable at `p`.
#[inline(always)]
unsafe fn order_at(p: *const u8, len: usize) -> u64 {
    let word = u64::from_be(unsafe { p.cast::<u64>().read_unaligned() });
    if len >= 8 { word } else { word & !(u64::MAX >> (8 * len)) }
}

impl Ev {
    #[inline(always)]
    pub fn user(&self) -> u32 {
        self.user_ts as u32
    }

    #[inline(always)]
    pub fn ts(&self) -> u32 {
        (self.user_ts >> 32) as u32
    }

    #[inline(always)]
    pub fn ingest(&self) -> u32 {
        self.ingest_pay as u32
    }

    #[inline(always)]
    pub fn pay(&self) -> u32 {
        (self.ingest_pay >> 32) as u32
    }
}

/// What is kept of a `click_id`: its `Id` (`types::id::pack_at`), or, for one spelled the
/// long way (`k` and 32 hex digits), the `Id` of its first 9 bytes. Interned whole, as any ID
/// over 9 bytes is, the long ones were a quarter of a run's cycles at 20M events: 640,000
/// of them, each into a hash map behind a lock.
///
/// Two clicks are only ever compared when they are one person's and of one `ts`, and then
/// by their `click_id`'s bytes. The first 9 bytes decide that unless they are the same in
/// both, which for two of a keyed bijection's values happens once in 2^32 pairs. The
/// sessions stage says when it met such a pair for two campaigns
/// (`sessions::ambiguous_tie`), and the run is then made again with `whole`: every ID
/// interned whole.
///
/// # Safety
/// 16 bytes readable at `p`, and `len` of them the ID's.
#[inline(always)]
pub(crate) unsafe fn click_id_at<W: Width>(p: *const u8, len: usize, whole: bool) -> Id {
    // SAFETY: the caller's contract.
    unsafe {
        if len == 33 && !whole && long_key_at::<W>(p, len, b'k').is_some() {
            return pack_at(p, 9);
        }
        pack_at(p, len)
    }
}

/// Link lines whose users are looked up together (`Ingest::opaque_links`).
const LINKS_HELD: usize = 32;

/// The two words every event record ends with: `user | ts << 32` with 0 for the `ts` of a
/// row that is not valid, and `ingest | pay << 32`.
#[inline(always)]
fn words(user: u32, ts: u32, valid: bool, ingest: u32, pay: u32) -> (u64, u64) {
    (u64::from(user) | u64::from(ts * u32::from(valid)) << 32, u64::from(ingest) | u64::from(pay) << 32)
}

/// The run's directory and the shape of the lists.
pub struct Plan {
    pub users: Users,
    /// Lists of short-keyed events = `1 << part_bits`: a record's is the top bits of
    /// `types::hash::hash(key)`.
    pub part_bits: u32,
    /// Lists of long-keyed events = `1 << long_bits`: the top bits of `mix_long(hi, lo)`.
    pub long_bits: u32,
    /// Lines decoded by the kernel (`DECODES`).
    kernel: bool,
    /// Long `click_id`s kept whole (`click_id_at`).
    pub(crate) whole_clicks: bool,
    /// `k & ask_step == 0` after a row is when the kernel asks for the directory's lines of
    /// the last eight rows' users (`SS_ASK=rows|batch`): 63 for `rows`, the pick; for
    /// `batch` every bit, which no count of rows clears, and a batch's users are asked for
    /// when it is full.
    pub(crate) ask_step: usize,
    /// The parse made for `types::width::Narrow` (`SS_PARSE_WIDTH=256`): the kernel, the row
    /// code and the links with no instruction that takes a core off its base clock.
    pub(crate) narrow: bool,
}

impl Plan {
    #[inline(always)]
    pub fn part(&self, key: u64) -> usize {
        (hash(key) >> (64 - self.part_bits)) as usize
    }

    #[inline(always)]
    pub fn long_part(&self, hi: u64, lo: u64) -> usize {
        (mix_long(hi, lo) >> (64 - self.long_bits)) as usize
    }
}

/// One worker's parse output.
pub struct Out {
    /// List `Plan::part(key)`: every well-formed event line with a non-empty `event_id` not
    /// spelled the long way, valid or not.
    pub events: Chunks<Ev>,
    /// List `Plan::long_part(hi, lo)`: the others.
    pub long_events: Chunks<EvL>,
    /// One list: the valid clicks.
    pub clicks: Chunks<Ck>,
    /// Lines the kernel left to the row code (`SS_TIMING` reports them).
    pub slow: usize,
    scratch: Scratch,
    /// The chunk being parsed (`Ingest::read`), and the file it was read from.
    input: Vec<u8>,
    file: Option<(usize, File)>,
    /// Bytes readable before the chunk being parsed.
    lead: usize,
}

impl Out {
    /// Slot `free-early`: the lists are dead once the dedup has read them.
    pub fn release(&mut self) {
        self.events.release();
        self.long_events.release();
        self.clicks.release();
    }
}

impl Ingest {
    /// The plan for this input; `grow` doubles the directory that many times (after one
    /// that overflowed), `whole_clicks` keeps every `click_id` whole (`click_id_at`).
    pub fn opaque_plan(&self, grow: u32, whole_clicks: bool) -> Plan {
        // About 8,000 lines a list, so that a list's rows are deduplicated in a table that
        // stays in L2, but no more than 2,048 lists: a worker stages 192 bytes a list, and
        // with 8,192 of them at 60M events that staging was past the caches (a run was
        // 1,711 ms, and 1,502 with 2,048 lists of four times the rows).
        let part_bits = (self.event_rows / 8192).next_power_of_two().trailing_zeros().clamp(6, 11);
        let part_bits = types::env::number("SS_PART_BITS_OPAQUE", part_bits as usize).clamp(1, 16) as u32;
        Plan {
            users: Users::for_rows(self.event_rows + self.click_rows + self.link_rows, grow),
            part_bits,
            long_bits: part_bits.saturating_sub(3).max(1),
            kernel: DECODES[types::env::pick("SS_DECODE", DECODES)] == "kernel",
            whole_clicks,
            ask_step: if types::env::pick("SS_ASK", &["rows", "batch"]) == 0 { 63 } else { usize::MAX },
            narrow: types::width::narrow_from_env(),
        }
    }

    /// A worker's lists and buffers, for `workers` workers.
    pub fn opaque_out(&self, plan: &Plan, workers: usize) -> Out {
        // A worker's block has room for four times its share of the lines (it is address
        // space until written), a chunk a list and a huge page more.
        let room = |rows: usize, record: usize, lists: usize| (rows + rows / 8) * record * 4 / workers.max(4) + lists * 4096 + (2 << 20);
        let (parts, long_parts) = (1usize << plan.part_bits, 1usize << plan.long_bits);
        Out {
            events: Chunks::new(parts, room(self.event_rows, size_of::<Ev>(), parts)),
            long_events: Chunks::new(long_parts, room(self.event_rows / 4, size_of::<EvL>(), long_parts)),
            clicks: Chunks::new(1, room(self.click_rows, size_of::<Ck>(), 1)),
            slow: 0,
            scratch: Scratch::new(),
            input: vec![0; crate::READ_ROOM + crate::chunk_bytes() + crate::READ_SLACK + 1 + crate::PAD],
            file: None,
            lead: 0,
        }
    }

    /// Parses chunk `task` (`0..tasks()`) into `out`. Called concurrently, one `out` a worker.
    pub fn opaque_parse(&self, ctx: &Ctx, plan: &Plan, task: usize, out: &mut Out) {
        let (seg, click) = &self.tasks[task];
        let mut input = std::mem::take(&mut out.input);
        let read = self.read(seg, &mut input, &mut out.file);
        let (p, len) = read.unwrap_or_else(|| self.seg(seg));
        // A chunk that was read has `READ_LEAD` bytes before it; of a mapped one only the
        // byte before its lines is known to be there.
        out.lead = if read.is_some() { crate::READ_LEAD } else { 1 };
        // SAFETY: `plan` made every task a segment, and `read` gives one as `seg` does: whole
        // lines, the byte before them and `PAD` bytes after them readable.
        unsafe {
            if *click { clicks(p, len, ctx, &self.dates, &self.campaigns, plan, out) } else { events(p, len, ctx, &self.dates, plan, out) }
        }
        if read.is_none() {
            self.release(seg);
        }
        out.input = input;
    }

    /// Chunks of the links files: the tasks of `opaque_links`.
    pub fn link_tasks(&self) -> usize {
        self.links.len()
    }

    /// Chunk `task` (`0..link_tasks()`) of the links files: each well-formed line with both
    /// sides non-empty. Called concurrently, one `out` a worker.
    pub fn opaque_links(&self, plan: &Plan, task: usize, out: &mut Out) -> Vec<LinkRec> {
        if plan.narrow { self.links_of::<Narrow>(plan, task, out) } else { self.links_of::<Wide>(plan, task, out) }
    }

    /// `opaque_links` made for one width: a function of its own for each, so that the
    /// narrow one holds no wide instruction (`types::width`).
    #[inline(never)]
    fn links_of<W: Width>(&self, plan: &Plan, task: usize, out: &mut Out) -> Vec<LinkRec> {
        let seg = &self.links[task];
        let mut links = Vec::with_capacity((seg.end - seg.start) / 16 + 16);
        let mut input = std::mem::take(&mut out.input);
        let read = self.read(seg, &mut input, &mut out.file);
        let (p, len) = read.unwrap_or_else(|| self.seg(seg));
        // The lines a few at a time: their keys, each key's line of the directory asked for,
        // then the lookups. (Looked up as it was read, each ID waited for its line: 3% of a
        // run's cycles for a twentieth of its lines.)
        let mut held: [([UserKey; 2], [u64; 2]); LINKS_HELD] = [([UserKey::Short(0); 2], [0; 2]); LINKS_HELD];
        let mut count = 0;
        let users = &plan.users;
        let flush = |held: &[([UserKey; 2], [u64; 2])], links: &mut Vec<LinkRec>| {
            for (keys, _) in held {
                users.prefetch(keys[0]);
                users.prefetch(keys[1]);
            }
            links.extend(held.iter().map(|&(keys, order)| LinkRec { users: [users.get::<W>(keys[0]), users.get::<W>(keys[1])], order }));
        };
        // SAFETY: a segment, and a segment's fields have 16 bytes readable after them.
        // (The lines by their masks and the keys a word at a time: through the byte-by-byte
        // tokenizer and `key_at`, 0.9M link lines were 4.7% of a run's instructions at 20M
        // events, 730 a line.)
        unsafe {
            let row = |&[a, b]: &[Field; 2]| {
                if a.len != 0 && b.len != 0 {
                    held[count] = ([key_at_wide::<W>(a.p, a.len), key_at_wide::<W>(b.p, b.len)], [order_at(a.p, a.len), order_at(b.p, b.len)]);
                    count += 1;
                    if count == LINKS_HELD {
                        flush(&held, &mut links);
                        count = 0;
                    }
                }
            };
            if W::NARROW { NarrowMask::lines::<2>(p, len, &mut out.scratch, row) } else { Mask::lines::<2>(p, len, &mut out.scratch, row) }
        };
        flush(&held[..count], &mut links);
        if read.is_none() {
            self.release(seg);
        }
        out.input = input;
        links
    }
}

/// # Safety
/// `p, len` is a segment.
unsafe fn events(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &Plan, out: &mut Out) {
    #[cfg(all(
        feature = "tok-shape",
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512dq",
        target_feature = "avx512cd",
        target_feature = "avx512vl",
        target_feature = "bmi2",
        target_feature = "lzcnt"
    ))]
    if plan.kernel && dates.few_months() {
        let lead = out.lead;
        return unsafe { if plan.narrow { kern::events::<Narrow>(p, len, lead, ctx, dates, plan, out) } else { kern::events::<Wide>(p, len, lead, ctx, dates, plan, out) } };
    }
    let Out { events, long_events, scratch, .. } = out;
    unsafe {
        if plan.narrow {
            Scalar::lines::<7>(p, len, scratch, |fields| event_row::<Narrow>(ctx, dates, plan, fields, events, long_events))
        } else {
            Scalar::lines::<7>(p, len, scratch, |fields| event_row::<Wide>(ctx, dates, plan, fields, events, long_events))
        }
    };
    events.fence();
    long_events.fence();
}

/// # Safety
/// `p, len` is a segment.
unsafe fn clicks(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, campaigns: &crate::pos::Campaigns, plan: &Plan, out: &mut Out) {
    #[cfg(all(
        feature = "tok-shape",
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512dq",
        target_feature = "avx512cd",
        target_feature = "avx512vl",
        target_feature = "bmi2",
        target_feature = "lzcnt"
    ))]
    if plan.kernel && dates.few_months() {
        let lead = out.lead;
        return unsafe { if plan.narrow { kern::clicks::<Narrow>(p, len, lead, ctx, dates, campaigns, plan, out) } else { kern::clicks::<Wide>(p, len, lead, ctx, dates, campaigns, plan, out) } };
    }
    let _ = campaigns;
    let Out { clicks, scratch, .. } = out;
    unsafe { if plan.narrow { Scalar::lines::<4>(p, len, scratch, |fields| click_row::<Narrow>(ctx, dates, plan, fields, clicks)) } else { Scalar::lines::<4>(p, len, scratch, |fields| click_row::<Wide>(ctx, dates, plan, fields, clicks)) } };
    clicks.fence();
}

/// One event line as its record, for any spelling: the checks of `crate::event_row`.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn event_row<W: Width>(ctx: &Ctx, dates: &Dates, plan: &Plan, fields: &[Field; 7], events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>) {
    let [id, user, ts, ingest, kind, product, price] = *fields;
    if id.len == 0 {
        return;
    }
    let (ts_ok, ts) = unsafe { dates.ts(ts.p, ts.len) };
    let (mut ingest_ok, mut ingest_ms, ingest_s) = unsafe { dates.f1(ingest.p, ingest.len) };
    let mut ingest_rel = ctx.ingest_rel(ingest_s);
    if !ingest_ok {
        (ingest_ok, ingest_ms, ingest_rel) = ingest_slow(ctx, ingest);
    }
    let word = unsafe { rd(kind.p) };
    let purchase = (kind.len == 8) & (word == u64::from_le_bytes(*b"purchase"));
    let short = word as u32;
    let known = purchase | ((kind.len == 4) & ((short == u32::from_le_bytes(*b"view")) | (short == u32::from_le_bytes(*b"cart"))));
    let (mut pay, mut price_ok) = (0, true);
    if purchase {
        let (ok, cents) = unsafe { price_cents(price.p, price.len) };
        let category = ctx.products.get(unsafe { pack_at(product.p, product.len) }).unwrap_or(ctx.unknown);
        (pay, price_ok) = (cents | u32::from(category) << PAY_CATEGORY_SHIFT, ok);
    }
    let valid = (user.len != 0)
        & ts_ok
        & (ts >= i64::from(ctx.event_lo))
        & (ts < i64::from(ctx.ts_end))
        & known
        & (product.len != 0)
        & price_ok
        & ingest_ok
        & (ingest_ms - ts <= DAY);
    // A row that is not valid names no user: nothing reads it.
    let place = if valid { unsafe { plan.users.at::<W>(user.p, user.len) } } else { NO_USER };
    let (user_ts, ingest_pay) = words(place, ts as u32, valid, ingest_rel, pay);
    // SAFETY: the fields' bytes are readable; a list is below the lists' count by its bits.
    unsafe {
        if let Some((hi, lo)) = long_key_at::<W>(id.p, id.len, b'e') {
            long_events.push_at(plan.long_part(hi, lo), EvL { hi, lo, user_ts, ingest_pay });
        } else {
            let key = short_key_at(id.p, id.len, b'e').unwrap_or_else(|| id::intern_any(std::slice::from_raw_parts(id.p, id.len)));
            events.push_at(plan.part(key), Ev { key, user_ts, ingest_pay });
        }
    }
}

/// One click line as its record, for any spelling: the checks of `crate::click_row`.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn click_row<W: Width>(ctx: &Ctx, dates: &Dates, plan: &Plan, fields: &[Field; 4], clicks: &mut Chunks<Ck>) {
    let [click, user, campaign, ts] = *fields;
    let (ok, ts) = unsafe { dates.ts(ts.p, ts.len) };
    if (click.len != 0) & (user.len != 0) & ok & (ts >= 0) & (ts < i64::from(ctx.ts_end))
        && let Some(campaign) = ctx.campaign_index.get(unsafe { pack_at(campaign.p, campaign.len) })
    {
        // SAFETY: the fields' bytes are readable; the clicks are one list.
        unsafe {
            let place = plan.users.at::<W>(user.p, user.len);
            clicks.push_at(0, Ck { click: click_id_at::<W>(click.p, click.len, plan.whole_clicks), user_ts: u64::from(place) | (ts as u64) << 32, campaign: u64::from(campaign) });
        }
    }
}

#[cfg(test)]
mod tests {
    use types::time::parse_f1;

    use super::*;

    /// Lists of both key widths take a row by its `event_id`'s spelling, and an invalid row
    /// names no user.
    #[test]
    fn rows_go_by_their_ids_spelling() {
        let ctx = Ctx::new(parse_f1(b"2026-10-08T00:00:00Z").unwrap(), parse_f1(b"2026-10-22T00:00:00Z").unwrap(), &[], &[]).unwrap();
        let dates = Dates::new(&ctx);
        let plan = Plan { users: Users::new(64, 64), part_bits: 3, long_bits: 1, kernel: false, whole_clicks: false, ask_step: 63, narrow: false };
        let (mut events, mut long_events) = (Chunks::<Ev>::new(8, 1 << 16), Chunks::<EvL>::new(2, 1 << 16));
        let text = b"\ne1,u1,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,view,p1,\n\
e02423c3d84ab053af78764ce49f23b5c,u02423c3d84ab053af78764ce49f23b5c,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,cart,p1,\n\
E1,U1,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,view,p1,\n\
e2,u1,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,refund,p1,\n";
        let mut buf = text.to_vec();
        buf.extend([0u8; 64]);
        // SAFETY: whole lines after the first byte, 64 bytes readable after them.
        unsafe { Scalar::lines::<7>(buf.as_ptr().add(1), text.len() - 1, &mut Scratch::new(), |fields| event_row::<Wide>(&ctx, &dates, &plan, fields, &mut events, &mut long_events)) };
        events.fence();
        long_events.fence();
        let (mut short, mut long) = (Vec::new(), Vec::new());
        for list in 0..8 {
            events.for_each(list, |piece| short.extend_from_slice(piece));
        }
        for list in 0..2 {
            long_events.for_each(list, |piece| long.extend_from_slice(piece));
        }
        assert_eq!((short.len(), long.len()), (3, 1));
        let users: std::collections::HashSet<u32> = short.iter().map(Ev::user).chain([long[0].user_ts as u32]).collect();
        // `u1`, the long user, `U1`, and none for the row with an unknown type.
        assert_eq!(users.len(), 4);
        assert!(users.contains(&NO_USER));
        let invalid: Vec<&Ev> = short.iter().filter(|row| row.ts() == 0).collect();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].user(), NO_USER);
        assert!(short.iter().any(|row| row.key & id::SLOW != 0), "`E1` is interned");
        assert!(!plan.users.overflowed());
    }
}
