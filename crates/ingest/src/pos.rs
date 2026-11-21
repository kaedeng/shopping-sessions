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
    let PosOut { events, pays, odd_events, odd_users, scratch, .. } = out;
    let limit = plan.events() as u64;
    unsafe {
        T::lines::<7>(p, len, scratch, |fields| event_row::<M>(ctx, dates, plan, limit, fields, events, pays, odd_events, odd_users))
    };
    // Streamed lines become visible to the placing threads.
    events.fence();
    pays.fence();
}

/// # Safety
/// `p, len` is a segment.
#[inline(always)]
unsafe fn clicks<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
    let PosOut { clicks, odd_clicks, scratch, .. } = out;
    unsafe { T::lines::<4>(p, len, scratch, |fields| click_row::<M>(ctx, dates, plan, fields, clicks, odd_clicks)) };
    clicks.fence();
}

/// The checks of `crate::event_row`, the same bits; only where the record goes differs.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn event_row<M: Mode>(
    ctx: &Ctx,
    dates: &Dates,
    plan: &PosPlan,
    limit: u64,
    fields: &[Field; 7],
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
            let rec = PosEvent { u, ts: ts as u32, ingest: ingest_rel, slot, flags: POS_VALID * u16::from(valid) | odd };
            // SAFETY: `number < parts << shift`.
            unsafe { events.push_as::<M>(part, rec) };
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
                unsafe { clicks.push_as::<M>(list, PosClick { click, u: number as u32, ts: ts as u32, campaign, spare: 0 }) };
            }
            _ => odd_clicks.push(ClickRec { user: unsafe { pack_at(user.p, user.len) }, click, ts: ts as u32, campaign }),
        }
    }
}
