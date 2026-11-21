//! Slot `pipe-positional`: the parse for the positional pipeline (`src/positional.rs`).
//!
//! The generator numbers events `0..` in (person, time) order and writes each number scrambled
//! under a key the dataset keeps to itself (`types::scramble`). `Ingest::pos_plan` finds the
//! keys from a sample; with them a row's `event_id` says where the event belongs in a table
//! that is already deduplicated, grouped and sorted once every row sits at its number.
//!
//! So an event line becomes one 16-byte `PosEvent` in the list of its number's partition
//! (a range of numbers, not a hash), and a click a `PosClick` in the list of its person's
//! range. A row the numbering does not cover (an ID spelled another way, a number past the
//! arrays) goes to the `odd_*` lists as the classic records, and the pipeline runs those
//! through the classic code. Which list a row takes depends on its `event_id` alone, so every
//! copy of an event meets its duplicates.

use std::collections::HashMap;

use memchr::memrchr;
use types::id::pack_at;
use types::parts::{Combine, Mode, Parts, Plain};
use types::pos::{POS_ODD_USER, POS_VALID, PosClick, PosEvent, PosPay};
use types::record::{ClickRec, EventRec, PAY_CATEGORY_SHIFT, VALID};
use types::scramble::{self, ALIAS_BITS};
use types::time::DAY;
use types::{Ctx, Id};

use crate::tok::{Field, Scalar, Scratch, Tok};
use crate::ts::{Dates, rd};
use crate::{Ingest, ingest_slow, price_cents};

/// How event rows are decoded, the pick first (`SS_POS_DECODE`).
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq"))]
const DECODES: &[&str] = &["wide", "row"];
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq")))]
const DECODES: &[&str] = &["row"];

/// Bytes of the first event chunk the key search reads, and the rows it takes from them.
const SAMPLE_BYTES: usize = 512 << 10;
const SAMPLE_ROWS: usize = 5000;
/// Fewer sampled rows than this say nothing about a numbering: the classic pipeline runs.
const MIN_ROWS: usize = 256;
/// The whales are the first persons (`generator/src/plan.rs`).
const WHALES: u64 = 8;
/// A user number's person must be below this to fit `u32` with its alias.
const PERSON_LIMIT: u64 = 1 << 22;

/// The keys and the shape of the dense arrays.
pub struct PosPlan {
    pub user_key: u64,
    pub event_key: u64,
    /// Event numbers per partition = `1 << shift`, at most 16 (`PosEvent::slot`).
    pub shift: u32,
    pub parts: usize,
    /// Persons the per-person arrays hold; a user number's person is below it.
    pub persons: u32,
    /// Persons per click list = `1 << click_shift`.
    pub click_shift: u32,
    pub click_parts: usize,
    /// Event rows decoded eight at a time (`wide`), where the build has AVX-512; `SS_POS_DECODE`.
    pub wide: bool,
}

impl PosPlan {
    /// Event numbers the arrays hold: `parts << shift`.
    pub fn events(&self) -> usize {
        self.parts << self.shift
    }
}

/// One worker's positional parse output.
pub struct PosOut {
    /// List `number >> shift`: every well-formed line whose `event_id` is a number below
    /// `PosPlan::events`, valid or not.
    pub events: Parts<PosEvent>,
    /// The same lists: the `pay` of each valid purchase among them.
    pub pays: Parts<PosPay>,
    /// List `person >> click_shift`: valid clicks whose `user_id` is a number.
    pub clicks: Parts<PosClick>,
    /// Every other well-formed event line with an `event_id`, as the classic record.
    pub odd_events: Vec<EventRec>,
    /// The `user_id` of each `PosEvent` flagged `POS_ODD_USER`, at its `u`.
    pub odd_users: Vec<Id>,
    /// Every other valid click, as the classic record (`user` as read).
    pub odd_clicks: Vec<ClickRec>,
    scratch: Scratch,
}

impl PosOut {
    /// Slot `free-early`: the lists are dead once placed and grouped.
    pub fn release(&mut self) {
        self.events.release();
        self.pays.release();
        self.clicks.release();
    }
}

pub(crate) type PosFn = fn(&Ingest, &Ctx, &PosPlan, usize, &mut PosOut);

/// Rows parsed before they are scattered.
const PENDING: usize = 64;

/// The last few parsed records and their lists, scattered `PENDING` at a time. A record goes
/// here with a store whose address is known at once. Stored straight into its list, its
/// address came at the end of the row's whole decode (the ID's number picks the list), and
/// with speculative store bypass disabled no later load may run before every earlier store's
/// address is known: the next row could not start until this one was finished (`x86.md`).
struct Pending<T> {
    lists: [u32; PENDING],
    recs: [T; PENDING],
    len: usize,
}

/// Where `event_row` leaves a record: the row path's `Pending`, or the wide path's batch.
trait Sink {
    /// # Safety
    /// `list < events.lists()`.
    unsafe fn put<M: Mode>(&mut self, list: usize, rec: PosEvent, events: &mut Parts<PosEvent>);
}

impl Sink for Pending<PosEvent> {
    #[inline(always)]
    unsafe fn put<M: Mode>(&mut self, list: usize, rec: PosEvent, events: &mut Parts<PosEvent>) {
        unsafe { self.push::<M>(list, rec, events) }
    }
}

impl<T: Copy + Default> Pending<T> {
    fn new() -> Pending<T> {
        Pending { lists: [0; PENDING], recs: [T::default(); PENDING], len: 0 }
    }

    /// # Safety
    /// `list < out.lists()`.
    #[inline(always)]
    unsafe fn push<M: Mode>(&mut self, list: usize, rec: T, out: &mut Parts<T>) {
        // SAFETY: `len < PENDING` between calls.
        unsafe {
            *self.lists.get_unchecked_mut(self.len) = list as u32;
            *self.recs.get_unchecked_mut(self.len) = rec;
        }
        self.len += 1;
        if self.len == PENDING {
            self.flush::<M>(out);
        }
    }

    #[inline(always)]
    fn flush<M: Mode>(&mut self, out: &mut Parts<T>) {
        for at in 0..self.len {
            // SAFETY: every list here was in range when pushed.
            unsafe { out.push_as::<M>(self.lists[at] as usize, self.recs[at]) };
        }
        self.len = 0;
    }
}

impl Ingest {
    /// The keys, found from the first rows of the largest event chunk, and the arrays' shape;
    /// `None` when the rows do not carry the generator's numbering (then the classic pipeline
    /// runs). With `force`, any sample will do and missing keys are zeros: every result is
    /// still exact, since nothing downstream trusts a key (tests run the fixtures this way).
    pub fn pos_plan(&self, ctx: &Ctx, force: bool) -> Option<PosPlan> {
        let sample = self.pos_sample();
        if sample.len() < MIN_ROWS && !force {
            return None;
        }
        let keys = find_keys(&sample, self.event_rows);
        if keys.is_none() && !force {
            return None;
        }
        let (user_key, event_key) = keys.unwrap_or((0, 0));
        let rows = self.event_rows.max(1);
        // Event numbers are below the number of distinct events, so below the lines.
        let room = rows + rows / 8 + 4096;
        let shift = (room / 1024).next_power_of_two().trailing_zeros().clamp(12, 16);
        let persons = (rows / 8).clamp(4096, PERSON_LIMIT as usize);
        let click_shift = (persons / 1024).next_power_of_two().trailing_zeros().max(6);
        let _ = ctx;
        Some(PosPlan {
            user_key,
            event_key,
            shift,
            parts: room.div_ceil(1 << shift),
            persons: persons as u32,
            click_shift,
            click_parts: persons.div_ceil(1 << click_shift),
            wide: DECODES[types::env::pick("SS_POS_DECODE", DECODES)] == "wide",
        })
    }

    /// (event half, user half) of up to `SAMPLE_ROWS` lines whose two IDs are spelled the
    /// generator's way (`types::scramble::half`).
    fn pos_sample(&self) -> Vec<(u64, u64)> {
        let mut rows = Vec::new();
        let Some((seg, _)) = self.tasks.iter().find(|(_, click)| !click) else { return rows };
        let (p, len) = self.seg(seg);
        // SAFETY: `seg` returned whole lines of a mapped or owned buffer.
        let bytes = unsafe { std::slice::from_raw_parts(p, len) };
        let len = if len > SAMPLE_BYTES { memrchr(b'\n', &bytes[..SAMPLE_BYTES]).map_or(0, |at| at + 1) } else { len };
        // SAFETY: whole lines of a segment; the fields are inside it.
        unsafe {
            Scalar::lines::<7>(p, len, &mut Scratch::new(), |fields| {
                let (id, user) = (fields[0], fields[1]);
                if rows.len() < SAMPLE_ROWS
                    && let Some(event) = scramble::canonical_at(id.p, id.len, b'e')
                    && let Some(user) = scramble::canonical_at(user.p, user.len, b'u')
                {
                    rows.push((scramble::half(event), scramble::half(user)));
                }
            })
        };
        rows
    }

    /// A worker's buffers for the positional parse.
    pub fn pos_out(&self, ctx: &Ctx, plan: &PosPlan) -> PosOut {
        // A partition holds `1 << shift` event numbers and a few percent more lines; a quarter
        // to spare, as workers pull chunks unevenly.
        let room = (1usize << plan.shift) * 9 / 8 / ctx.workers * 5 / 4 + 64;
        // The persons in use are about a 30th of the events, far fewer than the lists allow.
        let lists_in_use = ((self.event_rows / 30) >> plan.click_shift).max(1);
        let click_room = self.click_rows / ctx.workers / lists_in_use * 5 / 4 + 64;
        PosOut {
            events: Parts::new(plan.parts, room, self.scatter),
            pays: Parts::new(plan.parts, room / 16 + 32, self.scatter),
            clicks: Parts::new(plan.click_parts, click_room, self.scatter),
            odd_events: Vec::new(),
            odd_users: Vec::new(),
            odd_clicks: Vec::new(),
            scratch: Scratch::new(),
        }
    }

    /// Parses chunk `task` (`0..tasks()`) into `out`. Called concurrently, one `out` per worker.
    pub fn pos_parse(&self, ctx: &Ctx, plan: &PosPlan, task: usize, out: &mut PosOut) {
        (self.pos_fn)(self, ctx, plan, task, out)
    }
}

/// The user key, then the event key, or `None`.
///
/// User key: the most common user ID of the sample is a whale's, so its number is a person
/// below `WHALES` and an alias: a few thousand candidates, and the right one is the only key
/// that gives every sampled ID a person below `PERSON_LIMIT` (a wrong key passes one ID in
/// 512). Event key: the rows of the lowest person have the lowest event numbers, so count up
/// from 0 for one of them until every sampled row has a number inside the dataset.
fn find_keys(sample: &[(u64, u64)], event_rows: usize) -> Option<(u64, u64)> {
    let mut counts: HashMap<u64, u32> = HashMap::new();
    for &(_, user) in sample {
        *counts.entry(user).or_default() += 1;
    }
    let (&whale, _) = counts.iter().max_by_key(|&(&half, &count)| (count, half))?;
    let person_of = |half, key| scramble::number_of_half(half, key) >> ALIAS_BITS;
    let user_key = (0..WHALES << ALIAS_BITS)
        .map(|number| scramble::key_for(whale, number))
        .find(|&key| sample.iter().all(|&(_, user)| person_of(user, key) < PERSON_LIMIT))?;

    let lowest = sample.iter().map(|&(_, user)| person_of(user, user_key)).min()?;
    let first: Vec<u64> = sample.iter().filter(|&&(_, user)| person_of(user, user_key) == lowest).map(|&(event, _)| event).collect();
    let bound = 2 * event_rows as u64 + (1 << 16);
    // A whale has at most 2% of the events; a 16th of the lines leaves room.
    let tries = (event_rows as u64 / 16).max(1 << 16);
    let inside = |half, key| scramble::number_of_half(half, key) < bound;
    let event_key = (0..tries)
        .map(|number| scramble::key_for(first[0], number))
        .find(|&key| first.iter().all(|&event| inside(event, key)) && sample.iter().all(|&(event, _)| inside(event, key)))?;
    Some((user_key, event_key))
}

pub(crate) fn parse_with<T: Tok>(ingest: &Ingest, ctx: &Ctx, plan: &PosPlan, task: usize, out: &mut PosOut) {
    let (seg, click) = &ingest.tasks[task];
    let (p, len) = ingest.seg(seg);
    let dates = &ingest.dates;
    // SAFETY: `plan` made every task a segment.
    unsafe {
        if *click {
            if out.clicks.combining() {
                clicks::<T, Combine>(p, len, ctx, dates, plan, out);
            } else {
                clicks::<T, Plain>(p, len, ctx, dates, plan, out);
            }
        } else if out.events.combining() {
            events::<T, Combine>(p, len, ctx, dates, plan, out);
        } else {
            events::<T, Plain>(p, len, ctx, dates, plan, out);
        }
    }
    ingest.release(seg);
}

/// # Safety
/// `p, len` is a segment.
#[inline(always)]
unsafe fn events<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq"))]
    if plan.wide {
        return unsafe { wide::events::<T, M>(p, len, ctx, dates, plan, out) };
    }
    let PosOut { events, pays, odd_events, odd_users, scratch, .. } = out;
    let limit = plan.events() as u64;
    let mut pending = Pending::new();
    unsafe {
        T::lines::<7>(p, len, scratch, |fields| event_row::<M, Pending<PosEvent>>(ctx, dates, plan, limit, fields, &mut pending, events, pays, odd_events, odd_users))
    };
    pending.flush::<M>(events);
    // Streamed lines become visible to the placing threads.
    events.fence();
    pays.fence();
}

/// # Safety
/// `p, len` is a segment.
#[inline(always)]
unsafe fn clicks<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
    let PosOut { clicks, odd_clicks, scratch, .. } = out;
    let mut pending = Pending::new();
    unsafe { T::lines::<4>(p, len, scratch, |fields| click_row::<M>(ctx, dates, plan, fields, &mut pending, clicks, odd_clicks)) };
    pending.flush::<M>(clicks);
    clicks.fence();
}

/// The checks of `crate::event_row`, the same bits; only where the record goes differs.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn event_row<M: Mode, S: Sink>(
    ctx: &Ctx,
    dates: &Dates,
    plan: &PosPlan,
    limit: u64,
    fields: &[Field; 7],
    pending: &mut S,
    events: &mut Parts<PosEvent>,
    pays: &mut Parts<PosPay>,
    odd_events: &mut Vec<EventRec>,
    odd_users: &mut Vec<Id>,
) {
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
    let known = purchase
        | ((kind.len == 4) & ((short == u32::from_le_bytes(*b"view")) | (short == u32::from_le_bytes(*b"cart"))));
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
    let number = unsafe { scramble::canonical_at(id.p, id.len, b'e') }.map(|scrambled| scramble::number(scrambled, plan.event_key));
    match number {
        Some(number) if number < limit => {
            let (u, odd) = unsafe { user_number(user, plan, odd_users) };
            let (part, slot) = ((number >> plan.shift) as usize, (number & ((1 << plan.shift) - 1)) as u16);
            let rec = PosEvent::new(u, ts as u32, ingest_rel, slot, POS_VALID * u16::from(valid) | odd);
            // SAFETY: `number < parts << shift`.
            unsafe { pending.put::<M>(part, rec, events) };
            if valid & (pay != 0) {
                pays.push(part, PosPay { slot: u32::from(slot), ingest: ingest_rel, pay });
            }
        }
        _ => odd_event(id, user, ts as u32, ingest_rel, pay, valid, odd_events),
    }
}

#[cold]
#[inline(never)]
fn odd_event(id: Field, user: Field, ts: u32, ingest: u32, pay: u32, valid: bool, odd_events: &mut Vec<EventRec>) {
    // SAFETY: the fields are from a segment, 16 bytes readable after each.
    let (id, user) = unsafe { (pack_at(id.p, id.len), pack_at(user.p, user.len)) };
    odd_events.push(EventRec { id, user, ts, ingest, pay, flags: VALID * u32::from(valid) });
}

/// A `user_id` as `PosEvent::u` and its flag: the number when it is one inside the arrays, an
/// index into `odd_users` for any other non-empty ID.
///
/// # Safety
/// The field is from a segment.
#[inline(always)]
unsafe fn user_number(user: Field, plan: &PosPlan, odd_users: &mut Vec<Id>) -> (u32, u16) {
    match unsafe { scramble::canonical_at(user.p, user.len, b'u') }.map(|scrambled| scramble::number(scrambled, plan.user_key)) {
        Some(number) if number >> ALIAS_BITS < u64::from(plan.persons) => (number as u32, 0),
        _ if user.len == 0 => (0, 0),
        _ => odd_user(user, odd_users),
    }
}

#[cold]
#[inline(never)]
fn odd_user(user: Field, odd_users: &mut Vec<Id>) -> (u32, u16) {
    // SAFETY: the field is from a segment.
    odd_users.push(unsafe { pack_at(user.p, user.len) });
    (u32::try_from(odd_users.len() - 1).expect("odd users fit u32"), POS_ODD_USER)
}

/// The checks of `crate::click_row`.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn click_row<M: Mode>(
    ctx: &Ctx,
    dates: &Dates,
    plan: &PosPlan,
    fields: &[Field; 4],
    pending: &mut Pending<PosClick>,
    clicks: &mut Parts<PosClick>,
    odd_clicks: &mut Vec<ClickRec>,
) {
    let [click, user, campaign, ts] = *fields;
    let click = unsafe { pack_at(click.p, click.len) };
    let (ok, ts) = unsafe { dates.ts(ts.p, ts.len) };
    if (click != 0) & (user.len != 0) & ok & (ts >= 0) & (ts < i64::from(ctx.ts_end))
        && let Some(campaign) = ctx.campaign_index.get(unsafe { pack_at(campaign.p, campaign.len) })
    {
        let number = unsafe { scramble::canonical_at(user.p, user.len, b'u') }.map(|scrambled| scramble::number(scrambled, plan.user_key));
        match number {
            Some(number) if number >> ALIAS_BITS < u64::from(plan.persons) => {
                let list = (number >> ALIAS_BITS >> plan.click_shift) as usize;
                // SAFETY: the person is below `persons`, so the list is below `click_parts`.
                unsafe { pending.push::<M>(list, PosClick::new(click, number as u32, ts as u32, campaign), clicks) };
            }
            _ => odd_clicks.push(ClickRec { user: unsafe { pack_at(user.p, user.len) }, click, ts: ts as u32, campaign }),
        }
    }
}

/// Slot `pos-wide`: the event rows decoded eight at a time (AVX-512).
///
/// The rows of a chunk are gathered 64 at a time: each row's fields as the tokenizer gave
/// them, the few 8-byte words of them the decode reads, and the dates of its two timestamps
/// looked up. For each group of 8 the wide kernels (`types::scramble::wide::numbers`,
/// `Dates::ts_wide`, `Dates::f1_wide`) decode both IDs, the `ts` and the `ingest_ts` in one
/// pass, and the lanes that are plainly ordinary go out as records straight from the
/// registers: both IDs spelled the generator's way with numbers inside the arrays, a `ts` that
/// is F1, F2 or 13-digit F3, an F1 `ingest_ts` in the date table, the type `view` or `cart`, a
/// `product_id`. Such a row's record is, bit for bit, the one `event_row` makes (the window
/// and the late check are the same compares). Every other row (a purchase, anything dirty or
/// odd) goes to `event_row` itself, so the wide path never has to be right about an unusual
/// row, only to recognise one.
///
/// The words are copied row by row and not gathered: a gather is one instruction, but slow on
/// AMD cores and on Intel ones under the Downfall mitigation.
///
/// One core, 20M: an event row was about 670 instructions, 255 of them the tokenizer; the
/// decode's 350 are what this replaces for about 95% of the rows.
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq"
))]
pub(crate) mod wide {
    use core::arch::x86_64::*;

    use types::scramble::wide::numbers;

    use super::*;
    use crate::ts::rd;

    /// Rows gathered before they are decoded; a multiple of 8.
    const ROWS: usize = 64;
    /// Records held before they are scattered; a group adds at most 8.
    const HELD: usize = 128;

    /// The words of a row the kernels read, one array per word (see `Batch::take`).
    const ID_FIRST: usize = 0;
    const ID_DIGITS: usize = 1;
    const USER_FIRST: usize = 2;
    const USER_DIGITS: usize = 3;
    const TS_0: usize = 4;
    const TS_8: usize = 5;
    const TS_12: usize = 6;
    const TS_19: usize = 7;
    const TS_DATE: usize = 8;
    const INGEST_8: usize = 9;
    const INGEST_12: usize = 10;
    const INGEST_DATE: usize = 11;
    const KIND: usize = 12;
    const WORDS: usize = 13;

    pub(super) struct Batch {
        /// `at[field][row]`, `len[field][row]`: the row's fields as the tokenizer gave them.
        at: [[u64; ROWS]; 7],
        len: [[u64; ROWS]; 7],
        words: [[u64; ROWS]; WORDS],
        rows: usize,
        /// The records decoded and not yet scattered: list, then the record's two words.
        lists: [u64; HELD],
        low: [u64; HELD],
        high: [u64; HELD],
        held: usize,
    }

    impl Sink for Batch {
        #[inline(always)]
        unsafe fn put<M: Mode>(&mut self, list: usize, rec: PosEvent, _: &mut Parts<PosEvent>) {
            debug_assert!(self.held < HELD);
            let (low, high) = rec.words();
            (self.lists[self.held], self.low[self.held], self.high[self.held]) = (list as u64, low, high);
            self.held += 1;
        }
    }

    impl Batch {
        fn new() -> Batch {
            Batch { at: [[0; ROWS]; 7], len: [[0; ROWS]; 7], words: [[0; ROWS]; WORDS], rows: 0, lists: [0; HELD], low: [0; HELD], high: [0; HELD], held: 0 }
        }

        /// Adds a row: its fields, and the words of them the kernels read.
        ///
        /// # Safety
        /// `self.rows < ROWS`, and the fields are from a segment (27 bytes readable at each).
        #[inline(always)]
        unsafe fn take(&mut self, dates: &Dates, fields: &[Field; 7]) {
            let row = self.rows;
            // SAFETY: `row < ROWS`; every read is within 27 bytes of a field's first.
            unsafe {
                for (field, value) in fields.iter().enumerate() {
                    *self.at.get_unchecked_mut(field).get_unchecked_mut(row) = value.p as u64;
                    *self.len.get_unchecked_mut(field).get_unchecked_mut(row) = value.len as u64;
                }
                let [id, user, ts, ingest, kind, ..] = *fields;
                let mut set = |word: usize, value: u64| *self.words.get_unchecked_mut(word).get_unchecked_mut(row) = value;
                set(ID_FIRST, rd(id.p));
                set(ID_DIGITS, rd(id.p.add(1)));
                set(USER_FIRST, rd(user.p));
                set(USER_DIGITS, rd(user.p.add(1)));
                set(TS_0, rd(ts.p));
                set(TS_8, rd(ts.p.add(8)));
                set(TS_12, rd(ts.p.add(12)));
                set(TS_19, rd(ts.p.add(19)));
                set(TS_DATE, dates.date_s(ts.p) as u64);
                set(INGEST_8, rd(ingest.p.add(8)));
                set(INGEST_12, rd(ingest.p.add(12)));
                set(INGEST_DATE, dates.date_s(ingest.p) as u64);
                set(KIND, rd(kind.p));
            }
            self.rows = row + 1;
        }

        #[inline(always)]
        fn flush<M: Mode>(&mut self, events: &mut Parts<PosEvent>) {
            for at in 0..self.held {
                // SAFETY: every list here was in range when put.
                unsafe { events.push_as::<M>(self.lists[at] as usize, PosEvent::from_words(self.low[at], self.high[at])) };
            }
            self.held = 0;
        }
    }

    #[inline(always)]
    fn all(value: u64) -> __m512i {
        // SAFETY: AVX-512F is compiled in (cfg above).
        #[allow(unused_unsafe)]
        unsafe {
            _mm512_set1_epi64(value as i64)
        }
    }

    /// One event chunk through the batches.
    ///
    /// # Safety
    /// `p, len` is a segment.
    #[inline(always)]
    pub(super) unsafe fn events<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
        let PosOut { events, pays, odd_events, odd_users, scratch, .. } = out;
        let limit = plan.events() as u64;
        let mut batch = Batch::new();
        unsafe {
            T::lines::<7>(p, len, scratch, |fields| {
                // An empty `event_id` drops the line (rules "Events" step 1).
                if fields[0].len == 0 {
                    return;
                }
                batch.take(dates, fields);
                if batch.rows == ROWS {
                    decode::<M>(&mut batch, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                }
            });
            decode::<M>(&mut batch, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
        }
        batch.flush::<M>(events);
        // Streamed lines become visible to the placing threads.
        events.fence();
        pays.fence();
    }

    /// Decodes the gathered rows and leaves the batch empty of rows.
    ///
    /// # Safety
    /// Every gathered row's fields are from a segment still mapped.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    unsafe fn decode<M: Mode>(
        batch: &mut Batch,
        ctx: &Ctx,
        dates: &Dates,
        plan: &PosPlan,
        limit: u64,
        events: &mut Parts<PosEvent>,
        pays: &mut Parts<PosPay>,
        odd_events: &mut Vec<EventRec>,
        odd_users: &mut Vec<Id>,
    ) {
        let rows = std::mem::take(&mut batch.rows);
        let mut first = 0;
        while first < rows {
            if batch.held + 8 > HELD {
                batch.flush::<M>(events);
            }
            let live: __mmask8 = if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 };
            // SAFETY: `first + 8 <= ROWS`. A lane past `rows` holds an older row's words or
            // zeros, and `live` keeps it out of everything below.
            unsafe {
                let word = |word: usize| _mm512_loadu_si512(batch.words[word].as_ptr().add(first).cast());
                let len = |field: usize| _mm512_loadu_si512(batch.len[field].as_ptr().add(first).cast());

                let (number, id_ok) = numbers(word(ID_FIRST), word(ID_DIGITS), len(0), b'e', plan.event_key);
                let (user, user_ok) = numbers(word(USER_FIRST), word(USER_DIGITS), len(1), b'u', plan.user_key);
                let (ts_ok, ts) = dates.ts_wide(word(TS_0), word(TS_8), word(TS_12), word(TS_19), len(2), word(TS_DATE));
                let (ingest_ok, ingest_ms, ingest_s) = dates.f1_wide(word(INGEST_8), word(INGEST_12), len(3), word(INGEST_DATE));
                let kind = _mm512_and_si512(word(KIND), all(0xFFFF_FFFF));
                let view_or_cart = (_mm512_cmpeq_epi64_mask(kind, all(u64::from(u32::from_le_bytes(*b"view"))))
                    | _mm512_cmpeq_epi64_mask(kind, all(u64::from(u32::from_le_bytes(*b"cart")))))
                    & _mm512_cmpeq_epi64_mask(len(4), all(4));
                let fast = live
                    & id_ok
                    & _mm512_cmplt_epu64_mask(number, all(limit))
                    & user_ok
                    & _mm512_cmplt_epu64_mask(_mm512_srli_epi64::<{ ALIAS_BITS }>(user), all(u64::from(plan.persons)))
                    & ts_ok
                    & ingest_ok
                    & view_or_cart
                    & _mm512_cmpneq_epi64_mask(len(5), all(0));

                // `event_row`'s `valid` for such a row: the window, and not late.
                let valid = _mm512_cmpge_epi64_mask(ts, all(u64::from(ctx.event_lo)))
                    & _mm512_cmplt_epi64_mask(ts, all(u64::from(ctx.ts_end)))
                    & _mm512_cmple_epi64_mask(_mm512_sub_epi64(ingest_ms, ts), all(DAY as u64));
                // `Ctx::ingest_rel`.
                let ingest_rel = _mm512_min_epi64(
                    _mm512_max_epi64(_mm512_sub_epi64(ingest_s, all(ctx.ingest_base() as u64)), all(0)),
                    all(u64::from(u32::MAX)),
                );
                // `PosEvent::new`: u | ts << 32, then ingest | slot << 32 | flags << 48.
                let low = _mm512_or_si512(_mm512_and_si512(user, all(0xFFFF_FFFF)), _mm512_slli_epi64::<32>(ts));
                let slot = _mm512_and_si512(number, all((1u64 << plan.shift) - 1));
                let high = _mm512_or_si512(
                    _mm512_or_si512(ingest_rel, _mm512_slli_epi64::<32>(slot)),
                    _mm512_maskz_mov_epi64(valid, all(u64::from(POS_VALID) << 48)),
                );
                let list = _mm512_srl_epi64(number, _mm_cvtsi32_si128(plan.shift as i32));
                // Compressed in a register and stored whole (the lanes past the fast ones are
                // overwritten by the next store): the compressing store is microcoded on Zen 4.
                let held = batch.held;
                _mm512_storeu_si512(batch.lists.as_mut_ptr().add(held).cast(), _mm512_maskz_compress_epi64(fast, list));
                _mm512_storeu_si512(batch.low.as_mut_ptr().add(held).cast(), _mm512_maskz_compress_epi64(fast, low));
                _mm512_storeu_si512(batch.high.as_mut_ptr().add(held).cast(), _mm512_maskz_compress_epi64(fast, high));
                batch.held = held + fast.count_ones() as usize;

                // Every other row, through the row code.
                let mut rest = live & !fast;
                while rest != 0 {
                    let row = first + rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    let fields: [Field; 7] = std::array::from_fn(|field| Field { p: batch.at[field][row] as *const u8, len: batch.len[field][row] as usize });
                    event_row::<M, Batch>(ctx, dates, plan, limit, &fields, batch, events, pays, odd_events, odd_users);
                }
            }
            first += 8;
        }
    }
}
