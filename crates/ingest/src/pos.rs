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

use types::chunks::Chunks;
use types::id::pack_at;
use types::parts::{Combine, Mode, Parts, Plain};
use types::pos::{CLICK_CAMPAIGNS, OddIdClick, POS_ODD_USER, POS_VALID, PosClick, PosEvent, PosPay, click_key_at};
use types::record::{ClickRec, EventRec, PAY_CATEGORY_SHIFT, VALID};
use types::scramble::{self, ALIAS_BITS};
use types::time::DAY;
use types::{Ctx, Id};

use crate::tok::{Field, Scalar, Scratch, Tok};
use crate::ts::{Dates, rd};
use crate::{Ingest, ingest_slow, price_cents};

/// How event rows are decoded, the pick first (`SS_POS_DECODE`).
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
const DECODES: &[&str] = &["kernel", "fused", "wide", "row"];
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    not(all(feature = "tok-shape", target_feature = "bmi2", target_feature = "avx512cd"))
))]
const DECODES: &[&str] = &["wide", "row"];
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq")))]
const DECODES: &[&str] = &["row"];

/// Bytes of the first event chunk the key search reads, and the rows it takes from them.
pub(crate) const SAMPLE_BYTES: usize = 512 << 10;
const SAMPLE_ROWS: usize = 5000;
/// Fewer sampled rows than this say nothing about a numbering: the classic pipeline runs.
const MIN_ROWS: usize = 256;
/// The whales are the first persons (`generator/src/plan.rs`).
const WHALES: u64 = 8;
/// A user number's person must be below this to fit `u32` with its alias.
const PERSON_LIMIT: u64 = 1 << 22;

/// The shape of the dense arrays: what the row estimate alone decides, so the workers' lists
/// can be built while the keys are still being found (`crate::open`).
#[derive(Clone, Copy)]
pub struct PosShape {
    pub shift: u32,
    pub parts: usize,
    pub persons: u32,
    pub click_shift: u32,
    pub click_parts: usize,
}

impl PosShape {
    /// For about `event_rows` event lines.
    pub(crate) fn of(event_rows: usize) -> PosShape {
        let rows = event_rows.max(1);
        // Event numbers are below the number of distinct events, so below the lines.
        let room = rows + rows / 8 + 4096;
        let shift = (room / 1024).next_power_of_two().trailing_zeros().clamp(12, 16);
        // `SS_POS_SHIFT`: the numbers per partition as a power of two, for measurements.
        let shift = (types::env::number("SS_POS_SHIFT", shift as usize) as u32).clamp(10, 16);
        let persons = (rows / 8).clamp(4096, PERSON_LIMIT as usize);
        let click_shift = (persons / 1024).next_power_of_two().trailing_zeros().max(6);
        PosShape { shift, parts: room.div_ceil(1 << shift), persons: persons as u32, click_shift, click_parts: persons.div_ceil(1 << click_shift) }
    }
}

/// A worker's lists for the event lines of the positional parse (`PosOut::events`,
/// `PosOut::pays`), for `workers` workers; `scatter` as `types::parts::mode_from_env`.
pub(crate) fn event_lists(workers: usize, shape: &PosShape, scatter: Option<usize>) -> (Chunks<PosEvent>, Parts<PosPay>) {
    // A partition holds `1 << shift` event numbers and a few percent more lines; a quarter
    // to spare, as workers pull chunks unevenly.
    let room = (1usize << shape.shift) * 9 / 8 / workers * 5 / 4 + 64;
    // The events' chunks: the numbers the arrays hold are an eighth more than the lines
    // expected, and a worker's block has room for four times its share of them (it is
    // address space until written), a chunk a list and a huge page more.
    let bytes = (shape.parts << shape.shift) * size_of::<PosEvent>() * 4 / workers.max(4) + shape.parts * 4096 + (2 << 20);
    (Chunks::new(shape.parts, bytes), Parts::new(shape.parts, room / 16 + 32, scatter))
}

/// A worker's lists for the click lines (`PosOut::clicks`), for about `event_rows` event
/// lines and `click_rows` click lines.
pub(crate) fn click_lists(workers: usize, shape: &PosShape, click_rows: usize) -> Chunks<PosClick> {
    // As the events' block (`event_lists`): four times a worker's share of the lines, a chunk
    // a list and a huge page more.
    let bytes = (click_rows + click_rows / 8) * size_of::<PosClick>() * 4 / workers.max(4) + shape.click_parts * 4096 + (2 << 20);
    Chunks::new(shape.click_parts, bytes)
}

impl PosOut {
    /// A worker's buffers for the positional parse: its lists, and room to read a chunk of
    /// `chunk` bytes into.
    pub(crate) fn new(events: Chunks<PosEvent>, pays: Parts<PosPay>, clicks: Chunks<PosClick>, chunk: usize) -> PosOut {
        PosOut {
            events,
            pays,
            clicks,
            odd_events: Vec::new(),
            odd_users: Vec::new(),
            odd_clicks: Vec::new(),
            odd_id_clicks: Vec::new(),
            odd_ids: Vec::new(),
            scratch: Scratch::new(),
            // What `Ingest::read` takes for a chunk: untouched until read into, or `touch`.
            input: vec![0; crate::READ_ROOM + chunk + crate::READ_SLACK + 1 + crate::PAD],
            file: None,
            lead: 0,
        }
    }

    /// Has the kernel back the buffers now, until `stop` says so (`Parts::touch`): the chunk
    /// buffer and a fair share of the events' and the clicks' blocks.
    pub(crate) fn touch(&mut self, stop: &impl Fn() -> bool) {
        for page in self.input.chunks_mut(4096) {
            if stop() {
                return;
            }
            page[0] = 0;
        }
        // A fair share of the rows: a quarter of the block (`event_lists`).
        self.events.touch(self.events.capacity() / 4, stop);
        self.clicks.touch(self.clicks.capacity() / 4, stop);
        self.pays.touch(usize::MAX, stop);
    }
}

/// (event half, user half) of up to `SAMPLE_ROWS` of `lines` whose two IDs are spelled the
/// generator's way (`types::scramble::half`). `lines` is a padded buffer (`crate::padded`):
/// whole event lines from its second byte to `PAD` bytes before its end.
pub(crate) fn sample(lines: &[u8]) -> Vec<(u64, u64)> {
    let mut rows = Vec::new();
    // SAFETY: whole lines of a padded buffer; the fields are inside it.
    unsafe {
        crate::tok::Quick::lines::<7>(lines.as_ptr().add(1), lines.len() - 1 - crate::PAD, &mut Scratch::new(), |fields| {
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

/// The keys `sample` gives (`find_keys`), or `None` when the rows do not carry the
/// generator's numbering: then the classic pipeline runs. With `force`, any sample will do
/// and missing keys are zeros: every result is still exact, since nothing downstream trusts a
/// key (tests run the fixtures this way).
pub(crate) fn keys(sample: &[(u64, u64)], event_rows: usize, force: bool) -> Option<(u64, u64)> {
    if sample.len() < MIN_ROWS && !force {
        return None;
    }
    find_keys(sample, event_rows).or(force.then_some((0, 0)))
}

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
    /// Event lines read by their shape straight into the wide decode (`fused`), no tokenizer
    /// between; `SS_POS_DECODE`.
    pub fused: bool,
    /// Event lines turned into numbers where they are read, by tables (`kern`);
    /// `SS_POS_DECODE`. The click lines are then read as `fused` reads them.
    pub kernel: bool,
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
    pub events: Chunks<PosEvent>,
    /// The same lists: the `pay` of each valid purchase among them.
    pub pays: Parts<PosPay>,
    /// List `person >> click_shift`: valid clicks whose `user_id` is a number and whose
    /// `click_id` has a key (`types::pos::click_key_at`).
    pub clicks: Chunks<PosClick>,
    /// Every other well-formed event line with an `event_id`, as the classic record.
    pub odd_events: Vec<EventRec>,
    /// The `user_id` of each `PosEvent` flagged `POS_ODD_USER`, at its `u`.
    pub odd_users: Vec<Id>,
    /// Every other valid click whose `user_id` is not such a number, as the classic record
    /// (`user` as read).
    pub odd_clicks: Vec<ClickRec>,
    /// The `click_id` of each click in `clicks` that has no key (`PosClick::odd`).
    pub odd_ids: Vec<Id>,
    /// The valid clicks of numbered users whose campaign is past a `PosClick`'s bits.
    pub odd_id_clicks: Vec<OddIdClick>,
    scratch: Scratch,
    /// The chunk being parsed, when it is read and not mapped (`Ingest::read`), and the file
    /// it was read from, kept open for the next.
    input: Vec<u8>,
    file: Option<(usize, std::fs::File)>,
    /// Bytes readable before the chunk being parsed (`parse_with`).
    lead: usize,
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

/// One line of the links files with both sides non-empty.
#[derive(Clone, Copy)]
pub struct Link {
    pub ids: [Id; 2],
    /// Each ID's scrambled value when it is spelled the generator's way
    /// (`types::scramble::canonical_at`), `Link::ODD` otherwise: read from the text, where
    /// `canonical_packed` would unpack the ID again.
    pub scrambled: [u64; 2],
}

impl Link {
    pub const ODD: u64 = u64::MAX;
}

fn links_with<T: Tok>(ingest: &Ingest) -> Vec<Link> {
    let mut links = Vec::with_capacity(ingest.link_rows + 16);
    let mut scratch = Scratch::new();
    // Read like the parse's chunks (`Ingest::read`), the mapping only where that cannot be.
    let (mut input, mut file) = (Vec::new(), None);
    for seg in &ingest.links {
        let read = ingest.read(seg, &mut input, &mut file);
        let (p, len) = read.unwrap_or_else(|| ingest.seg(seg));
        // SAFETY: `plan` made it a segment, and a segment's fields have 16 bytes after them.
        unsafe {
            T::lines::<2>(p, len, &mut scratch, |&[a, b]| {
                let ids = [pack_at(a.p, a.len), pack_at(b.p, b.len)];
                if (ids[0] != 0) & (ids[1] != 0) {
                    let value = |field: Field| scramble::canonical_at(field.p, field.len, b'u').unwrap_or(Link::ODD);
                    links.push(Link { ids, scrambled: [value(a), value(b)] });
                }
            })
        };
        if read.is_none() {
            ingest.release(seg);
        }
    }
    links
}

/// The campaigns named `c_` and a number of at most four digits, by that number, for the click
/// kernel: one load, where the row code packs the ID (`pack_at`) and hashes it. Any other
/// campaign's clicks go through the row code.
pub(crate) struct Campaigns {
    /// The index of campaign `c_<n>` at `n`, `NONE` for none; two entries more than numbers,
    /// so that 8 bytes can be read at any of them.
    #[cfg_attr(
        not(all(
            feature = "tok-shape",
            target_arch = "x86_64",
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512dq",
            target_feature = "avx512cd",
            target_feature = "avx512vl",
            target_feature = "bmi2",
            target_feature = "lzcnt"
        )),
        allow(dead_code)
    )]
    pub(crate) numbers: Box<[u32]>,
}

impl Campaigns {
    pub(crate) const NONE: u32 = u32::MAX;
    /// Numbers the table holds: four digits' worth.
    pub(crate) const NUMBERS: usize = 10_000;

    pub(crate) fn new(ctx: &Ctx) -> Campaigns {
        let mut numbers = vec![Self::NONE; Self::NUMBERS + 2];
        for (name, _) in &ctx.campaigns[..ctx.organic() as usize] {
            // The number as its digits spell it, and only that spelling: `c_07` is not `c_7`.
            let digits = name.strip_prefix(b"c_").filter(|digits| (1..=4).contains(&digits.len()) && digits.iter().all(u8::is_ascii_digit) && (digits.len() == 1 || digits[0] != b'0'));
            // The index the row code gives this ID: its first row's.
            if let (Some(digits), Some(index)) = (digits, ctx.campaign_index.get(types::id::pack(name))) {
                numbers[digits.iter().fold(0usize, |number, &digit| number * 10 + usize::from(digit - b'0'))] = index;
            }
        }
        Campaigns { numbers: numbers.into() }
    }
}

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

/// Lists a parse appends to: `Parts`, or `Chunks` (which write one way only).
trait Lists<T> {
    /// # Safety
    /// `list` is one of the lists, and `M` the mode they were built for.
    unsafe fn append<M: Mode>(&mut self, list: usize, rec: T);
}

impl<T: Copy> Lists<T> for Parts<T> {
    #[inline(always)]
    unsafe fn append<M: Mode>(&mut self, list: usize, rec: T) {
        unsafe { self.push_as::<M>(list, rec) }
    }
}

impl<T: Copy> Lists<T> for Chunks<T> {
    #[inline(always)]
    unsafe fn append<M: Mode>(&mut self, list: usize, rec: T) {
        unsafe { self.push_at(list, rec) }
    }
}

/// Where `event_row` leaves a record: the row path's `Pending`, or the wide path's batch.
trait Sink {
    /// # Safety
    /// `list < events.lists()`.
    unsafe fn put<M: Mode>(&mut self, list: usize, rec: PosEvent, events: &mut Chunks<PosEvent>);
}

impl Sink for Pending<PosEvent> {
    #[inline(always)]
    unsafe fn put<M: Mode>(&mut self, list: usize, rec: PosEvent, events: &mut Chunks<PosEvent>) {
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
    unsafe fn push<M: Mode>(&mut self, list: usize, rec: T, out: &mut impl Lists<T>) {
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
    fn flush<M: Mode>(&mut self, out: &mut impl Lists<T>) {
        for at in 0..self.len {
            // SAFETY: every list here was in range when pushed.
            unsafe { out.append::<M>(self.lists[at] as usize, self.recs[at]) };
        }
        self.len = 0;
    }
}

impl Ingest {
    /// The plan for `shape` and the keys `keys` found (`crate::open`).
    pub(crate) fn pos_plan(&self, shape: PosShape, (user_key, event_key): (u64, u64)) -> PosPlan {
        PosPlan {
            user_key,
            event_key,
            shift: shape.shift,
            parts: shape.parts,
            persons: shape.persons,
            click_shift: shape.click_shift,
            click_parts: shape.click_parts,
            wide: DECODES[types::env::pick("SS_POS_DECODE", DECODES)] == "wide",
            // The fused decode finds a date among the table's months, one step each.
            fused: ["fused", "kernel"].contains(&DECODES[types::env::pick("SS_POS_DECODE", DECODES)]) && self.few_months(),
            kernel: DECODES[types::env::pick("SS_POS_DECODE", DECODES)] == "kernel",
        }
    }

    /// Whether the date table spans few enough calendar months for the fused decode.
    fn few_months(&self) -> bool {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq"))]
        return self.dates.few_months();
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq")))]
        false
    }

    /// `links` for the positional pipeline: every well-formed line of every links file with
    /// both sides non-empty, each ID with its scrambled value. The lines are read by shape
    /// where the build has it (`tok::fused::pair_shape`).
    pub fn pos_links(&self) -> Vec<Link> {
        #[cfg(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2"))]
        return links_with::<crate::tok::Fused>(self);
        #[cfg(not(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2")))]
        links_with::<Scalar>(self)
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
/// that gives the sampled IDs persons below `PERSON_LIMIT` (a wrong key passes one ID in
/// 512). Event key: the rows of the lowest person have the lowest event numbers, so count up
/// from 0 for one of them until the sampled rows have numbers inside the dataset.
///
/// A key may leave one sampled row in a hundred outside (`most`): an ID from elsewhere that
/// happens to be spelled the generator's way must not cost the whole run its fast path, and a
/// wrong key leaves nearly every row outside, so it is still told apart at once.
fn find_keys(sample: &[(u64, u64)], event_rows: usize) -> Option<(u64, u64)> {
    let mut counts: HashMap<u64, u32> = HashMap::new();
    for &(_, user) in sample {
        *counts.entry(user).or_default() += 1;
    }
    let (&whale, _) = counts.iter().max_by_key(|&(&half, &count)| (count, half))?;
    let person_of = |half, key| scramble::number_of_half(half, key) >> ALIAS_BITS;
    // Whether all but a hundredth of the rows pass; stops at the first row too many.
    let most = |pass: &dyn Fn(&(u64, u64)) -> bool| {
        let mut spare = sample.len() / 100;
        sample.iter().all(|row| {
            if pass(row) {
                return true;
            }
            let left = spare > 0;
            spare = spare.saturating_sub(1);
            left
        })
    };
    let user_key = (0..WHALES << ALIAS_BITS)
        .map(|number| scramble::key_for(whale, number))
        .find(|&key| most(&|&(_, user)| person_of(user, key) < PERSON_LIMIT))?;

    // The lowest person with a few rows: a stray ID could be a lower person, with one.
    let mut persons: Vec<u64> = sample.iter().map(|&(_, user)| person_of(user, user_key)).collect();
    persons.sort_unstable();
    let lowest = persons.chunk_by(|a, b| a == b).find(|rows| rows.len() >= 4)?[0];
    let first: Vec<u64> = sample.iter().filter(|&&(_, user)| person_of(user, user_key) == lowest).map(|&(event, _)| event).collect();
    let bound = 2 * event_rows as u64 + (1 << 16);
    // A whale has at most 2% of the events; a 16th of the lines leaves room.
    let tries = (event_rows as u64 / 16).max(1 << 16);
    let inside = |half, key| scramble::number_of_half(half, key) < bound;
    let event_key = (0..tries)
        .map(|number| scramble::key_for(first[0], number))
        .find(|&key| first[..4].iter().all(|&event| inside(event, key)) && most(&|&(event, _)| inside(event, key)))?;
    Some((user_key, event_key))
}

pub(crate) fn parse_with<T: Tok>(ingest: &Ingest, ctx: &Ctx, plan: &PosPlan, task: usize, out: &mut PosOut) {
    let (seg, click) = &ingest.tasks[task];
    // The chunk in the worker's buffer, or through the mapping.
    let mut input = std::mem::take(&mut out.input);
    let read = ingest.read(seg, &mut input, &mut out.file);
    let (p, len) = read.unwrap_or_else(|| ingest.seg(seg));
    // A chunk that was read has `READ_LEAD` bytes before it; of a mapped one only the byte
    // before its lines is known to be there.
    out.lead = if read.is_some() { crate::READ_LEAD } else { 1 };
    let dates = &ingest.dates;
    // SAFETY: `plan` made every task a segment, and `read` gives one as `seg` does: whole
    // lines, the byte before them and `PAD` bytes after them readable.
    unsafe {
        if *click {
            clicks::<T, Combine>(p, len, ctx, dates, &ingest.campaigns, plan, out);
        } else if out.pays.combining() {
            events::<T, Combine>(p, len, ctx, dates, plan, out);
        } else {
            events::<T, Plain>(p, len, ctx, dates, plan, out);
        }
    }
    if read.is_none() {
        ingest.release(seg);
    }
    out.input = input;
}

/// # Safety
/// `p, len` is a segment.
#[inline(always)]
unsafe fn events<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
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
    if plan.kernel {
        let lead = out.lead;
        return unsafe { wide::fused::kern::events::<M>(p, len, lead, ctx, dates, plan, out) };
    }
    #[cfg(all(
        feature = "tok-shape",
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512dq",
        target_feature = "avx512cd",
        target_feature = "bmi2"
    ))]
    if plan.fused {
        return unsafe { wide::fused::events::<M>(p, len, ctx, dates, plan, out) };
    }
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
unsafe fn clicks<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, campaigns: &Campaigns, plan: &PosPlan, out: &mut PosOut) {
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
    if plan.fused || plan.kernel {
        let lead = out.lead;
        return unsafe { wide::fused::kern::clicks::<M>(p, len, lead, ctx, dates, campaigns, plan, out) };
    }
    let _ = campaigns;
    let PosOut { clicks, odd_clicks, odd_id_clicks, odd_ids, scratch, .. } = out;
    let mut pending = Pending::new();
    unsafe { T::lines::<4>(p, len, scratch, |fields| click_row::<M>(ctx, dates, plan, fields, &mut pending, clicks, odd_clicks, odd_id_clicks, odd_ids)) };
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
    events: &mut Chunks<PosEvent>,
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
    clicks: &mut Chunks<PosClick>,
    odd_clicks: &mut Vec<ClickRec>,
    odd_id_clicks: &mut Vec<OddIdClick>,
    odd_ids: &mut Vec<Id>,
) {
    let [click, user, campaign, ts] = *fields;
    let key = unsafe { click_key_at(click.p, click.len) };
    let click = unsafe { pack_at(click.p, click.len) };
    let (ok, ts) = unsafe { dates.ts(ts.p, ts.len) };
    if (click != 0) & (user.len != 0) & ok & (ts >= 0) & (ts < i64::from(ctx.ts_end))
        && let Some(campaign) = ctx.campaign_index.get(unsafe { pack_at(campaign.p, campaign.len) })
    {
        let number = unsafe { scramble::canonical_at(user.p, user.len, b'u') }.map(|scrambled| scramble::number(scrambled, plan.user_key));
        match (number, key) {
            (Some(number), Some(key)) if number >> ALIAS_BITS < u64::from(plan.persons) && (campaign as usize) < CLICK_CAMPAIGNS => {
                let list = (number >> ALIAS_BITS >> plan.click_shift) as usize;
                // SAFETY: the person is below `persons`, so the list is below `click_parts`.
                unsafe { pending.push::<M>(list, PosClick::new(key, number as u32, ts as u32, campaign), clicks) };
            }
            // A `click_id` spelled another way (one line in 200,000 has its IDs in upper
            // case): the click with its person's, and the ID kept beside the lists.
            (Some(number), None) if number >> ALIAS_BITS < u64::from(plan.persons) && (campaign as usize) < CLICK_CAMPAIGNS => {
                let list = (number >> ALIAS_BITS >> plan.click_shift) as usize;
                odd_ids.push(click);
                // SAFETY: as above.
                unsafe { pending.push::<M>(list, PosClick::odd(odd_ids.len() - 1, number as u32, ts as u32, campaign), clicks) };
            }
            // A campaign past the record's bits: the click as it is, and its person leaves
            // the fast lane (`src/positional.rs`).
            (Some(number), _) if number >> ALIAS_BITS < u64::from(plan.persons) => {
                odd_id_clicks.push(OddIdClick { u: number as u32, click, ts: ts as u32, campaign });
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

    /// The records decoded and not yet scattered: list, then the record's two words.
    pub(super) struct Held {
        lists: [u64; HELD],
        low: [u64; HELD],
        high: [u64; HELD],
        len: usize,
    }

    impl Sink for Held {
        #[inline(always)]
        unsafe fn put<M: Mode>(&mut self, list: usize, rec: PosEvent, _: &mut Chunks<PosEvent>) {
            debug_assert!(self.len < HELD);
            let (low, high) = rec.words();
            (self.lists[self.len], self.low[self.len], self.high[self.len]) = (list as u64, low, high);
            self.len += 1;
        }
    }

    impl Held {
        fn new() -> Held {
            Held { lists: [0; HELD], low: [0; HELD], high: [0; HELD], len: 0 }
        }

        /// A function of its own: inside the decode its loop had no registers left, and
        /// reloaded the lists' base, room and staging from the stack for every record.
        #[inline(never)]
        fn flush<M: Mode>(&mut self, events: &mut Chunks<PosEvent>) {
            let (lists, low, high) = (&self.lists, &self.low, &self.high);
            // SAFETY: `len <= HELD`, and every list here was in range when put.
            unsafe {
                events.extend(self.len, |at| (*lists.get_unchecked(at) as usize, PosEvent::from_words(*low.get_unchecked(at), *high.get_unchecked(at))))
            };
            self.len = 0;
        }

        /// `flush` for clicks held the same way: list, then the click's two words.
        #[cfg_attr(not(all(feature = "tok-shape", target_feature = "avx512vl", target_feature = "avx512cd", target_feature = "bmi2", target_feature = "lzcnt")), allow(dead_code))]
        #[inline(never)]
        fn flush_clicks(&mut self, clicks: &mut Chunks<PosClick>) {
            let (lists, low, high) = (&self.lists, &self.low, &self.high);
            // SAFETY: `len <= HELD`, and only lists of persons below `PosPlan::persons` are held.
            unsafe {
                clicks.extend(self.len, |at| (*lists.get_unchecked(at) as usize, PosClick::from_words(*low.get_unchecked(at), *high.get_unchecked(at))))
            };
            self.len = 0;
        }
    }

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

    struct Batch {
        /// `at[field][row]`, `len[field][row]`: the row's fields as the tokenizer gave them.
        at: [[u64; ROWS]; 7],
        len: [[u64; ROWS]; 7],
        words: [[u64; ROWS]; WORDS],
        rows: usize,
        held: Held,
    }

    impl Batch {
        fn new() -> Batch {
            Batch { at: [[0; ROWS]; 7], len: [[0; ROWS]; 7], words: [[0; ROWS]; WORDS], rows: 0, held: Held::new() }
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
    }

    #[inline(always)]
    fn all(value: u64) -> __m512i {
        // SAFETY: AVX-512F is compiled in (cfg above).
        #[allow(unused_unsafe)]
        unsafe {
            _mm512_set1_epi64(value as i64)
        }
    }

    /// Eight rows as the kernels read them: each lane a row's words (little-endian, at the
    /// first byte of the field, wrapping quotes stripped) and its fields' lengths.
    struct Lanes {
        /// The lanes that hold a row.
        live: __mmask8,
        /// The live lanes whose words are a well-formed line's: only these can be records
        /// straight from the registers.
        fit: __mmask8,
        id_first: __m512i,
        id_digits: __m512i,
        id_len: __m512i,
        user_first: __m512i,
        user_digits: __m512i,
        user_len: __m512i,
        ts_0: __m512i,
        ts_8: __m512i,
        ts_12: __m512i,
        ts_19: __m512i,
        ts_len: __m512i,
        ts_date: __m512i,
        ingest_8: __m512i,
        ingest_12: __m512i,
        ingest_len: __m512i,
        ingest_date: __m512i,
        kind: __m512i,
        kind_len: __m512i,
        product_len: __m512i,
    }

    /// Where `records` gets a group's fields, one field at a time: each kernel asks for its
    /// words just before it runs, so a source that has to compute them (`fused::RawLanes`)
    /// does it then, and not all twenty vectors are alive at once. (Computed up front, they
    /// left the decode with more live vectors than registers: 60 of its 640 instructions
    /// a group were moves to and from the stack, and 78 reloaded constants.)
    trait Fields {
        /// The lanes that hold a row.
        fn live(&self) -> __mmask8;
        /// The live lanes whose words are a well-formed line's.
        fn fit(&self) -> __mmask8;
        /// The `event_id`: bytes 0..8, bytes 1..9, the length.
        fn id(&self) -> [__m512i; 3];
        /// The `user_id`, the same.
        fn user(&self) -> [__m512i; 3];
        /// The `ts`: bytes 0..8, 8..16, 12..20, 19..27, the length, the date (`Dates::date_s`).
        fn ts(&self, dates: &Dates) -> [__m512i; 6];
        /// The `ingest_ts`: bytes 8..16, 12..20, the length, the date.
        fn ingest(&self, dates: &Dates) -> [__m512i; 4];
        /// The type's first bytes and its length, then the product's length.
        fn kind(&self) -> [__m512i; 3];
    }

    impl Fields for Lanes {
        #[inline(always)]
        fn live(&self) -> __mmask8 {
            self.live
        }

        #[inline(always)]
        fn fit(&self) -> __mmask8 {
            self.fit
        }

        #[inline(always)]
        fn id(&self) -> [__m512i; 3] {
            [self.id_first, self.id_digits, self.id_len]
        }

        #[inline(always)]
        fn user(&self) -> [__m512i; 3] {
            [self.user_first, self.user_digits, self.user_len]
        }

        #[inline(always)]
        fn ts(&self, _: &Dates) -> [__m512i; 6] {
            [self.ts_0, self.ts_8, self.ts_12, self.ts_19, self.ts_len, self.ts_date]
        }

        #[inline(always)]
        fn ingest(&self, _: &Dates) -> [__m512i; 4] {
            [self.ingest_8, self.ingest_12, self.ingest_len, self.ingest_date]
        }

        #[inline(always)]
        fn kind(&self) -> [__m512i; 3] {
            [self.kind, self.kind_len, self.product_len]
        }
    }

    /// What `records` leaves to its caller.
    struct Records {
        /// The live lanes that are not records yet: the row code's, unless the caller makes
        /// the purchases among them (`buys`) records itself.
        rest: __mmask8,
        /// Lanes of `rest` that are ordinary rows but for their type, which is 8 bytes
        /// starting `purchas`: a record once the type is checked whole and the price and the
        /// product are read.
        buys: __mmask8,
        /// For a lane of `buys`: `event_row`'s `valid` but for the price.
        valid: __mmask8,
        /// For a lane of `buys`: its list and its record's words, the second without flags.
        list: __m512i,
        low: __m512i,
        plain: __m512i,
    }

    /// The records of the lanes that are plainly ordinary rows of type `view` or `cart`,
    /// appended to `held`.
    ///
    /// # Safety
    /// `held.len + 8 <= HELD`.
    #[inline(always)]
    unsafe fn records<F: Fields>(lanes: &F, held: &mut Held, ctx: &Ctx, dates: &Dates, plan: &PosPlan, limit: u64) -> Records {
        // SAFETY: register arithmetic, and stores of 8 words at `held.len <= HELD - 8`.
        unsafe {
            // Each field's words are asked for where its kernel runs, and each kernel's
            // result is narrowed at once to what the record needs.
            let [id_first, id_digits, id_len] = lanes.id();
            let (number, id_ok) = numbers(id_first, id_digits, id_len, b'e', plan.event_key);
            let id_ok = id_ok & _mm512_cmplt_epu64_mask(number, all(limit));
            let [user_first, user_digits, user_len] = lanes.user();
            let (user, user_ok) = numbers(user_first, user_digits, user_len, b'u', plan.user_key);
            let user_ok = user_ok & _mm512_cmplt_epu64_mask(_mm512_srli_epi64::<{ ALIAS_BITS }>(user), all(u64::from(plan.persons)));
            let [ts_0, ts_8, ts_12, ts_19, ts_len, ts_date] = lanes.ts(dates);
            let (ts_ok, ts) = dates.ts_wide(ts_0, ts_8, ts_12, ts_19, ts_len, ts_date);
            let [ingest_8, ingest_12, ingest_len, ingest_date] = lanes.ingest(dates);
            let (ingest_ok, ingest_ms, ingest_s) = dates.f1_wide(ingest_8, ingest_12, ingest_len, ingest_date);
            let [kind, kind_len, product_len] = lanes.kind();
            let short = _mm512_and_si512(kind, all(0xFFFF_FFFF));
            let view_or_cart = (_mm512_cmpeq_epi64_mask(short, all(u64::from(u32::from_le_bytes(*b"view"))))
                | _mm512_cmpeq_epi64_mask(short, all(u64::from(u32::from_le_bytes(*b"cart")))))
                & _mm512_cmpeq_epi64_mask(kind_len, all(4));
            let ordinary = lanes.fit() & id_ok & user_ok & ts_ok & ingest_ok & _mm512_cmpneq_epi64_mask(product_len, all(0));
            let fast = ordinary & view_or_cart;
            // The type's first 7 bytes: all a word holds of a wrapped type's 8.
            let buys = ordinary
                & _mm512_cmpeq_epi64_mask(kind_len, all(8))
                & _mm512_cmpeq_epi64_mask(_mm512_and_si512(kind, all(0x00FF_FFFF_FFFF_FFFF)), all(u64::from_le_bytes(*b"purchas\0")));

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
            let plain = _mm512_or_si512(ingest_rel, _mm512_slli_epi64::<32>(slot));
            let high = _mm512_or_si512(plain, _mm512_maskz_mov_epi64(valid, all(u64::from(POS_VALID) << 48)));
            let list = _mm512_srl_epi64(number, _mm_cvtsi32_si128(plan.shift as i32));
            // Compressed in a register and stored whole (the lanes past the fast ones are
            // overwritten by the next store): the compressing store is microcoded on Zen 4.
            let at = held.len;
            _mm512_storeu_si512(held.lists.as_mut_ptr().add(at).cast(), _mm512_maskz_compress_epi64(fast, list));
            _mm512_storeu_si512(held.low.as_mut_ptr().add(at).cast(), _mm512_maskz_compress_epi64(fast, low));
            _mm512_storeu_si512(held.high.as_mut_ptr().add(at).cast(), _mm512_maskz_compress_epi64(fast, high));
            held.len = at + fast.count_ones() as usize;
            Records { rest: lanes.live() & !fast, buys, valid, list, low, plain }
        }
    }

    /// The lanes of a group that hold a row.
    #[inline(always)]
    fn live(rows: usize, first: usize) -> __mmask8 {
        if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 }
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
        batch.held.flush::<M>(events);
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
        events: &mut Chunks<PosEvent>,
        pays: &mut Parts<PosPay>,
        odd_events: &mut Vec<EventRec>,
        odd_users: &mut Vec<Id>,
    ) {
        let rows = std::mem::take(&mut batch.rows);
        let mut first = 0;
        while first < rows {
            if batch.held.len + 8 > HELD {
                batch.held.flush::<M>(events);
            }
            let live = live(rows, first);
            // SAFETY: `first + 8 <= ROWS`. A lane past `rows` holds an older row's words or
            // zeros, and `live` keeps it out of everything below.
            unsafe {
                let word = |word: usize| _mm512_loadu_si512(batch.words[word].as_ptr().add(first).cast());
                let len = |field: usize| _mm512_loadu_si512(batch.len[field].as_ptr().add(first).cast());
                let lanes = Lanes {
                    live,
                    fit: live,
                    id_first: word(ID_FIRST),
                    id_digits: word(ID_DIGITS),
                    id_len: len(0),
                    user_first: word(USER_FIRST),
                    user_digits: word(USER_DIGITS),
                    user_len: len(1),
                    ts_0: word(TS_0),
                    ts_8: word(TS_8),
                    ts_12: word(TS_12),
                    ts_19: word(TS_19),
                    ts_len: len(2),
                    ts_date: word(TS_DATE),
                    ingest_8: word(INGEST_8),
                    ingest_12: word(INGEST_12),
                    ingest_len: len(3),
                    ingest_date: word(INGEST_DATE),
                    kind: word(KIND),
                    kind_len: len(4),
                    product_len: len(5),
                };
                // Every row that is not a record already, through the row code.
                let mut rest = records(&lanes, &mut batch.held, ctx, dates, plan, limit).rest;
                while rest != 0 {
                    let row = first + rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    let fields: [Field; 7] = std::array::from_fn(|field| Field { p: batch.at[field][row] as *const u8, len: batch.len[field][row] as usize });
                    event_row::<M, Held>(ctx, dates, plan, limit, &fields, &mut batch.held, events, pays, odd_events, odd_users);
                }
            }
            first += 8;
        }
    }

    /// Slot `pos-fused`: event and click lines read by their shape straight into the 8-row
    /// decode, no tokenizer and no `Field` between.
    ///
    /// Per line, the scalar code finds the separators (two 64-byte windows for an event line,
    /// as `tok::fused::event_shape`: the first three commas, then three commas and the
    /// newline), checks nothing else, and copies what the decode will need: the separator and
    /// quote bits of each window and the raw bytes at the first five fields, as words. Per 8
    /// rows the decode then does the rest in registers: the quote rules (`pairs`), where each
    /// field starts and how long it is, the fields' words with a wrapping quote shifted off
    /// (`cut`), the dates (`Dates::date_wide`), and from there the kernels of `pos-wide`
    /// (`records`). `tests::raw_rows_are_the_scalar_fields` checks that what the kernels are
    /// given is what `Scalar` and `Batch::take` give them, lane for lane.
    ///
    /// A row keeps its line's offset. A line that is not of the usual shape, or whose quotes
    /// are not wrapping pairs, or that `records` does not recognise (a purchase, anything
    /// dirty), is tokenized again from that offset and goes through the row code.
    #[cfg(all(feature = "tok-shape", target_feature = "bmi2", target_feature = "avx512cd"))]
    pub(super) mod fused {
        use super::*;
        use crate::tok::fused::{EVENT_WINDOW, click_shape, event_shape, masks};

        #[cfg(all(target_feature = "avx512vl", target_feature = "lzcnt"))]
        pub(in super::super) mod kern;

        /// The words of an event row, one array per word.
        const R_AT: usize = 0;
        /// The first window: its three commas, and the quotes before the third.
        const R_SEPS_A: usize = 1;
        const R_QUOTES_A: usize = 2;
        /// The second window, from the byte after the third comma: three commas and the
        /// newline, and the quotes before the newline.
        const R_SEPS_B: usize = 3;
        const R_QUOTES_B: usize = 4;
        /// The raw bytes from each field's first (a wrapping quote included): 16 of the
        /// `event_id`, 16 of the `user_id`, 32 of the `ts`, 24 of the `ingest_ts`, 8 of the type.
        const R_ID: usize = 5;
        const R_USER: usize = 7;
        const R_TS: usize = 9;
        const R_INGEST: usize = 13;
        const R_KIND: usize = 16;
        const R_WORDS: usize = 17;

        struct Raw {
            words: [[u64; ROWS]; R_WORDS],
            rows: usize,
            held: Held,
        }

        /// The event line at `line` into row `row`: its length with the newline, or 0 when
        /// it has not the usual shape (three commas within 64 bytes and before any newline,
        /// then three commas and the newline within the next 64) and nothing was stored.
        ///
        /// # Safety
        /// `row < ROWS`, and `EVENT_WINDOW` bytes are readable at `line`.
        #[inline(always)]
        unsafe fn raw_row(words: &mut [[u64; ROWS]; R_WORDS], row: usize, line: *const u8) -> usize {
            // SAFETY: every read is within 64 bytes of a comma of the first window, which is
            // within 64 of `line`... the second window's 64 bytes start at most 64 on.
            unsafe {
                let (commas, newlines, quotes) = masks(line);
                let after_first = commas & commas.wrapping_sub(1);
                let after_second = after_first & after_first.wrapping_sub(1);
                let third = after_second & after_second.wrapping_neg();
                let before = third.wrapping_sub(1);
                if (third == 0) | (newlines & before != 0) {
                    return 0;
                }
                let (p1, p2, p3) = (commas.trailing_zeros() as usize, after_first.trailing_zeros() as usize, third.trailing_zeros() as usize);
                let second = line.add(p3 + 1);
                let (commas_b, newlines_b, quotes_b) = masks(second);
                let stops = commas_b | newlines_b;
                let rest = stops & stops.wrapping_sub(1);
                let rest = rest & rest.wrapping_sub(1);
                let rest = rest & rest.wrapping_sub(1);
                let end = rest & rest.wrapping_neg();
                let before_b = end.wrapping_sub(1);
                if (newlines_b & end == 0) | (newlines_b & before_b != 0) {
                    return 0;
                }
                let mut set = |word: usize, value: u64| *words.get_unchecked_mut(word).get_unchecked_mut(row) = value;
                set(R_SEPS_A, commas & (before | third));
                set(R_QUOTES_A, quotes & before);
                set(R_SEPS_B, stops & (before_b | end));
                set(R_QUOTES_B, quotes_b & before_b);
                let (user, ts, kind) = (line.add(p1 + 1), line.add(p2 + 1), second.add(stops.trailing_zeros() as usize + 1));
                set(R_ID, rd(line));
                set(R_ID + 1, rd(line.add(8)));
                set(R_USER, rd(user));
                set(R_USER + 1, rd(user.add(8)));
                set(R_TS, rd(ts));
                set(R_TS + 1, rd(ts.add(8)));
                set(R_TS + 2, rd(ts.add(16)));
                set(R_TS + 3, rd(ts.add(24)));
                set(R_INGEST, rd(second));
                set(R_INGEST + 1, rd(second.add(8)));
                set(R_INGEST + 2, rd(second.add(16)));
                set(R_KIND, rd(kind));
                p3 + end.trailing_zeros() as usize + 2
            }
        }

        /// The byte of an event line from which its newline is looked for first (`events`);
        /// the 64 bytes from there are inside `EVENT_WINDOW`.
        const GUESS_FROM: usize = 40;

        /// The `\n` bits of the 64 bytes at `p`.
        ///
        /// # Safety
        /// 64 bytes readable at `p`.
        #[inline(always)]
        unsafe fn newline_bits(p: *const u8) -> u64 {
            // SAFETY: the caller's contract; AVX-512BW is compiled in (cfg above).
            unsafe { _mm512_cmpeq_epi8_mask(_mm512_loadu_si512(p.cast()), _mm512_set1_epi8(b'\n' as i8)) }
        }

        /// One event chunk.
        ///
        /// # Safety
        /// `p, len` is a segment.
        #[inline(never)]
        pub(in super::super) unsafe fn events<M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
            let PosOut { events, pays, odd_events, odd_users, .. } = out;
            let limit = plan.events() as u64;
            let mut raw = Raw { words: [[0; ROWS]; R_WORDS], rows: 0, held: Held::new() };
            let (mut at, mut rows) = (0, 0);
            // SAFETY: `at < len` is a line's first byte; `rows < ROWS` between decodes; a line
            // of the usual shape is read within its window, and `PAD` bytes after the segment
            // are readable.
            unsafe {
                while at < len {
                    *raw.words.get_unchecked_mut(R_AT).get_unchecked_mut(rows) = at as u64;
                    let inside = at + EVENT_WINDOW <= len + crate::PAD;
                    // Where the next line begins, guessed from this line's address alone: after
                    // the first newline from byte `GUESS_FROM` on (an event line is 63 to 102
                    // bytes). `raw_row` finds the same end through the line's two windows, the
                    // second of which starts where the first one's third comma is: taken from
                    // there, a line's address waited for the whole of the line before it, two
                    // loads and a dozen steps (about 45 cycles a line, whatever else the core
                    // could do meanwhile). The guess is used only when it is `raw_row`'s
                    // answer, so nothing depends on it but the time.
                    let guess = if inside { GUESS_FROM + newline_bits(p.add(at + GUESS_FROM)).trailing_zeros() as usize + 1 } else { 0 };
                    let ahead = at + guess;
                    let next = if inside { raw_row(&mut raw.words, rows, p.add(at)) } else { 0 };
                    if (next == guess) & (next != 0) {
                        at = ahead;
                    } else if next != 0 {
                        at += next;
                    } else {
                        // Not a line `pairs` could pass: a quote that is no field's.
                        *raw.words.get_unchecked_mut(R_SEPS_A).get_unchecked_mut(rows) = 0;
                        *raw.words.get_unchecked_mut(R_QUOTES_A).get_unchecked_mut(rows) = !0;
                        at = line_end(p, len, at);
                    }
                    rows += 1;
                    if rows == ROWS {
                        raw.rows = rows;
                        decode::<M>(&mut raw, p, len, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                        rows = 0;
                    }
                }
                raw.rows = rows;
                decode::<M>(&mut raw, p, len, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
            }
            raw.held.flush::<M>(events);
            // Streamed lines become visible to the placing threads.
            events.fence();
            pays.fence();
        }

        /// Each lane's lowest set bit: its position (64 for none), and the lane without it.
        #[inline(always)]
        fn lowest(bits: __m512i) -> (__m512i, __m512i) {
            // SAFETY: register arithmetic; AVX-512CD is compiled in (cfg above).
            #[allow(unused_unsafe)]
            unsafe {
                let less = _mm512_add_epi64(bits, all(u64::MAX));
                // The bits below the lowest set one: as many as its position.
                (_mm512_sub_epi64(all(64), _mm512_lzcnt_epi64(_mm512_andnot_si512(bits, less))), _mm512_and_si512(bits, less))
            }
        }

        /// The lanes where the quotes `quotes` are only wrapping pairs, for fields that
        /// start at bit 0 and end at the separators `seps` (`quotes` holds no bit from the
        /// last separator on).
        ///
        /// A field's first byte is bit 0 or the bit after a separator, its last the bit
        /// before its separator. A quote anywhere else is stray, and one on a byte that is
        /// both is a field of one quote. That leaves quotes on first and last bytes; what
        /// remains to show is that a field has the one exactly when it has the other. Take
        /// `(last << 1) - first`: a wrapped field contributes its separator's bit less its
        /// first byte's, the bits of the field itself, and wrapped fields do not overlap, so
        /// no separator's bit is set. A first quote without its last borrows through its
        /// field's separator, and a last quote without its first leaves its separator's bit:
        /// either way a separator's bit is set.
        #[inline(always)]
        fn pairs(quotes: __m512i, seps: __m512i) -> __mmask8 {
            // SAFETY: register arithmetic.
            #[allow(unused_unsafe)]
            unsafe {
                let starts = _mm512_or_si512(_mm512_slli_epi64::<1>(seps), all(1));
                let ends = _mm512_srli_epi64::<1>(seps);
                let (first, last) = (_mm512_and_si512(quotes, starts), _mm512_and_si512(quotes, ends));
                let stray = _mm512_andnot_si512(_mm512_or_si512(starts, ends), quotes);
                let alone = _mm512_and_si512(first, ends);
                let spans = _mm512_sub_epi64(_mm512_slli_epi64::<1>(last), first);
                let bad = _mm512_or_si512(_mm512_or_si512(stray, alone), _mm512_and_si512(spans, seps));
                _mm512_testn_epi64_mask(bad, bad)
            }
        }

        /// Eight bytes from bit `bits` of the 16 whose words are `low` and `high`.
        #[inline(always)]
        fn cut(low: __m512i, high: __m512i, bits: __m512i) -> __m512i {
            // SAFETY: register arithmetic. A shift count of 64 or more gives 0, which is what
            // `bits == 0` needs of `high`.
            #[allow(unused_unsafe)]
            unsafe {
                _mm512_or_si512(_mm512_srlv_epi64(low, bits), _mm512_sllv_epi64(high, _mm512_sub_epi64(all(64), bits)))
            }
        }

        /// Rows `first..first + 8` of the raw words, as a source of fields for `records`: the
        /// quote rules and the separators' positions at once (`new`), each field's words when
        /// its kernel asks.
        struct RawLanes<'a> {
            words: &'a [[u64; ROWS]; R_WORDS],
            first: usize,
            live: __mmask8,
            fit: __mmask8,
            /// The six commas' positions: three from the line's first byte, three from the
            /// byte after the third.
            p: [__m512i; 6],
            /// Whether each of the first six fields is wrapped: a quote at its first byte.
            q: [__m512i; 6],
        }

        impl<'a> RawLanes<'a> {
            /// # Safety
            /// `first + 8 <= ROWS`.
            #[inline(always)]
            unsafe fn new(words: &'a [[u64; ROWS]; R_WORDS], first: usize, live: __mmask8) -> RawLanes<'a> {
                // SAFETY: loads of 8 words at `first <= ROWS - 8`, then register arithmetic.
                unsafe {
                    let word = |word: usize| _mm512_loadu_si512(words.get_unchecked(word).as_ptr().add(first).cast());
                    let (seps_a, quotes_a, seps_b, quotes_b) = (word(R_SEPS_A), word(R_QUOTES_A), word(R_SEPS_B), word(R_QUOTES_B));
                    let fit = live & pairs(quotes_a, seps_a) & pairs(quotes_b, seps_b);
                    // The separators' positions; a field starts one after the one before it.
                    let (p1, rest) = lowest(seps_a);
                    let (p2, rest) = lowest(rest);
                    let (p3, _) = lowest(rest);
                    let (p4, rest) = lowest(seps_b);
                    let (p5, rest) = lowest(rest);
                    let (p6, _) = lowest(rest);
                    let one = all(1);
                    let bit = |quotes: __m512i, comma: __m512i| _mm512_and_si512(_mm512_srlv_epi64(quotes, _mm512_add_epi64(comma, one)), one);
                    let q = [_mm512_and_si512(quotes_a, one), bit(quotes_a, p1), bit(quotes_a, p2), _mm512_and_si512(quotes_b, one), bit(quotes_b, p4), bit(quotes_b, p5)];
                    RawLanes { words, first, live, fit, p: [p1, p2, p3, p4, p5, p6], q }
                }
            }

            /// Eight rows' word `word`.
            #[inline(always)]
            fn word(&self, word: usize) -> __m512i {
                // SAFETY: `first + 8 <= ROWS` (`new`).
                unsafe { _mm512_loadu_si512(self.words.get_unchecked(word).as_ptr().add(self.first).cast()) }
            }

            /// Field `field`'s length: from the comma before it (the window's first byte for
            /// the first of a window) to its own, less its two quotes.
            #[inline(always)]
            fn len(&self, field: usize) -> __m512i {
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    let whole = if field == 0 || field == 3 {
                        self.p[field]
                    } else {
                        _mm512_sub_epi64(_mm512_sub_epi64(self.p[field], self.p[field - 1]), all(1))
                    };
                    _mm512_sub_epi64(whole, _mm512_slli_epi64::<1>(self.q[field]))
                }
            }

            /// The bits of field `field`'s wrapping quote, to shift off its words.
            #[inline(always)]
            fn quote(&self, field: usize) -> __m512i {
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    _mm512_slli_epi64::<3>(self.q[field])
                }
            }
        }

        impl Fields for RawLanes<'_> {
            #[inline(always)]
            fn live(&self) -> __mmask8 {
                self.live
            }

            #[inline(always)]
            fn fit(&self) -> __mmask8 {
                self.fit
            }

            #[inline(always)]
            fn id(&self) -> [__m512i; 3] {
                let (low, high, bits) = (self.word(R_ID), self.word(R_ID + 1), self.quote(0));
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    [cut(low, high, bits), cut(low, high, _mm512_add_epi64(bits, all(8))), self.len(0)]
                }
            }

            #[inline(always)]
            fn user(&self) -> [__m512i; 3] {
                let (low, high, bits) = (self.word(R_USER), self.word(R_USER + 1), self.quote(1));
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    [cut(low, high, bits), cut(low, high, _mm512_add_epi64(bits, all(8))), self.len(1)]
                }
            }

            #[inline(always)]
            fn ts(&self, dates: &Dates) -> [__m512i; 6] {
                let (w0, w1, w2, w3) = (self.word(R_TS), self.word(R_TS + 1), self.word(R_TS + 2), self.word(R_TS + 3));
                let bits = self.quote(2);
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    let (ts_0, ts_8) = (cut(w0, w1, bits), cut(w1, w2, bits));
                    let ts_12 = cut(w1, w2, _mm512_add_epi64(bits, all(32)));
                    let ts_19 = cut(w2, w3, _mm512_add_epi64(bits, all(24)));
                    [ts_0, ts_8, ts_12, ts_19, self.len(2), dates.date_wide(ts_0, ts_8)]
                }
            }

            #[inline(always)]
            fn ingest(&self, dates: &Dates) -> [__m512i; 4] {
                let (w0, w1, w2) = (self.word(R_INGEST), self.word(R_INGEST + 1), self.word(R_INGEST + 2));
                let bits = self.quote(3);
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    let (ingest_0, ingest_8) = (cut(w0, w1, bits), cut(w1, w2, bits));
                    [ingest_8, cut(w1, w2, _mm512_add_epi64(bits, all(32))), self.len(3), dates.date_wide(ingest_0, ingest_8)]
                }
            }

            #[inline(always)]
            fn kind(&self) -> [__m512i; 3] {
                // SAFETY: register arithmetic.
                #[allow(unused_unsafe)]
                unsafe {
                    [_mm512_srlv_epi64(self.word(R_KIND), self.quote(4)), self.len(4), self.len(5)]
                }
            }
        }

        /// Rows `first..first + 8` of `words` with every field cut: what `RawLanes` hands
        /// `records`, all at once, for the tests to compare with the scalar tokenizer.
        ///
        /// # Safety
        /// `first + 8 <= ROWS`.
        #[cfg(test)]
        unsafe fn lanes_of(words: &[[u64; ROWS]; R_WORDS], first: usize, live: __mmask8, dates: &Dates) -> Lanes {
            let raw = unsafe { RawLanes::new(words, first, live) };
            let ([id_first, id_digits, id_len], [user_first, user_digits, user_len]) = (raw.id(), raw.user());
            let [ts_0, ts_8, ts_12, ts_19, ts_len, ts_date] = raw.ts(dates);
            let [ingest_8, ingest_12, ingest_len, ingest_date] = raw.ingest(dates);
            let [kind, kind_len, product_len] = raw.kind();
            Lanes {
                live: raw.live,
                fit: raw.fit,
                id_first,
                id_digits,
                id_len,
                user_first,
                user_digits,
                user_len,
                ts_0,
                ts_8,
                ts_12,
                ts_19,
                ts_len,
                ts_date,
                ingest_8,
                ingest_12,
                ingest_len,
                ingest_date,
                kind,
                kind_len,
                product_len,
            }
        }

        /// Decodes the gathered rows and leaves `raw` empty of rows.
        ///
        /// # Safety
        /// `base, bytes` is the segment the rows' lines are in.
        #[inline(never)]
        #[allow(clippy::too_many_arguments)]
        unsafe fn decode<M: Mode>(
            raw: &mut Raw,
            base: *const u8,
            bytes: usize,
            ctx: &Ctx,
            dates: &Dates,
            plan: &PosPlan,
            limit: u64,
            events: &mut Chunks<PosEvent>,
            pays: &mut Parts<PosPay>,
            odd_events: &mut Vec<EventRec>,
            odd_users: &mut Vec<Id>,
        ) {
            let rows = std::mem::take(&mut raw.rows);
            let mut first = 0;
            while first < rows {
                if raw.held.len + 8 > HELD {
                    raw.held.flush::<M>(events);
                }
                // SAFETY: `first + 8 <= ROWS`. A lane past `rows` holds an older row's words
                // or zeros, and `live` keeps it out of the records and the row code.
                unsafe {
                    let lanes = RawLanes::new(&raw.words, first, live(rows, first));
                    let made = records(&lanes, &mut raw.held, ctx, dates, plan, limit);
                    let mut rest = made.rest;
                    // The purchases: the kernels' record, with the price and the product read
                    // here. About 3% of the rows, and the row code was 600 instructions each.
                    if made.buys != 0 {
                        let (mut list, mut low, mut plain) = ([0u64; 8], [0u64; 8], [0u64; 8]);
                        _mm512_storeu_si512(list.as_mut_ptr().cast(), made.list);
                        _mm512_storeu_si512(low.as_mut_ptr().cast(), made.low);
                        _mm512_storeu_si512(plain.as_mut_ptr().cast(), made.plain);
                        let mut buys = made.buys;
                        while buys != 0 {
                            let lane = buys.trailing_zeros() as usize;
                            buys &= buys - 1;
                            if let Some((pay, price_ok)) = purchase(base, &raw.words, first + lane, ctx) {
                                // `event_row`: the row is valid when its price is, too, and a
                                // valid purchase leaves its `pay` beside its record.
                                let valid = (made.valid >> lane & 1 == 1) & price_ok;
                                let high = plain[lane] | u64::from(POS_VALID * u16::from(valid)) << 48;
                                raw.held.put::<M>(list[lane] as usize, PosEvent::from_words(low[lane], high), events);
                                if valid & (pay != 0) {
                                    pays.push(list[lane] as usize, PosPay { slot: (plain[lane] >> 32) as u32, ingest: plain[lane] as u32, pay });
                                }
                                rest &= !(1 << lane);
                            }
                        }
                    }
                    while rest != 0 {
                        let row = first + rest.trailing_zeros() as usize;
                        rest &= rest - 1;
                        slow_line::<M>(base, bytes, raw.words[R_AT][row] as usize, &mut raw.held, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                    }
                }
                first += 8;
            }
        }

        /// For row `row`, a well-formed line that `records` found ordinary but for its type:
        /// its `pay` and whether its price is valid, as `event_row` computes them, when the
        /// type is `purchase`; `None` for any other type (the row code's).
        ///
        /// # Safety
        /// `row < ROWS` is a row `raw_row` stored from a line of the segment at `base`, and
        /// `pairs` passed its second window.
        #[inline(always)]
        unsafe fn purchase(base: *const u8, words: &[[u64; ROWS]; R_WORDS], row: usize, ctx: &Ctx) -> Option<(u32, bool)> {
            // SAFETY: the fields are inside the line, and `PAD` bytes after the segment are
            // readable.
            unsafe {
                let (seps_a, seps_b, quotes) = (words[R_SEPS_A][row], words[R_SEPS_B][row], words[R_QUOTES_B][row]);
                // The second window starts after the third comma, the first window's last.
                let second = base.add(words[R_AT][row] as usize + 64 - seps_a.leading_zeros() as usize);
                let p4 = seps_b.trailing_zeros() as usize;
                let rest = seps_b & seps_b.wrapping_sub(1);
                let p5 = rest.trailing_zeros() as usize;
                let rest = rest & rest.wrapping_sub(1);
                let p6 = rest.trailing_zeros() as usize;
                let rest = rest & rest.wrapping_sub(1);
                let p7 = rest.trailing_zeros() as usize;
                // A field is wrapped when a quote is its first byte (`pairs`).
                let wrapped = |start: usize| (quotes >> start) as usize & 1;
                let (q4, q5, q6) = (wrapped(p4 + 1), wrapped(p5 + 1), wrapped(p6 + 1));
                if (p5 - p4 - 1 - 2 * q4 != 8) | (rd(second.add(p4 + 1 + q4)) != u64::from_le_bytes(*b"purchase")) {
                    return None;
                }
                let (ok, cents) = price_cents(second.add(p6 + 1 + q6), p7 - p6 - 1 - 2 * q6);
                let category = ctx.products.get(pack_at(second.add(p5 + 1 + q5), p6 - p5 - 1 - 2 * q5)).unwrap_or(ctx.unknown);
                Some((cents | u32::from(category) << PAY_CATEGORY_SHIFT, ok))
            }
        }

        /// The first byte after the line at `at`.
        #[cold]
        #[inline(never)]
        unsafe fn line_end(p: *const u8, len: usize, at: usize) -> usize {
            // SAFETY: `at..len` is inside the segment.
            let rest = unsafe { std::slice::from_raw_parts(p.add(at), len - at) };
            at + memchr::memchr(b'\n', rest).map_or(rest.len(), |newline| newline + 1)
        }

        /// A row through the row code: the line at `at` tokenized again (by its shape where
        /// it has one, a purchase mostly; by `Scalar` otherwise), then `event_row`.
        ///
        /// # Safety
        /// `p, len` is a segment and `at < len` a line's first byte in it.
        #[cold]
        #[inline(never)]
        #[allow(clippy::too_many_arguments)]
        unsafe fn slow_line<M: Mode>(
            p: *const u8,
            len: usize,
            at: usize,
            held: &mut Held,
            ctx: &Ctx,
            dates: &Dates,
            plan: &PosPlan,
            limit: u64,
            events: &mut Chunks<PosEvent>,
            pays: &mut Parts<PosPay>,
            odd_events: &mut Vec<EventRec>,
            odd_users: &mut Vec<Id>,
        ) {
            // SAFETY: one line of the segment; the shape is asked for only where its window
            // is readable, and its fields are inside the line.
            unsafe {
                let line = p.add(at);
                if at + EVENT_WINDOW <= len + crate::PAD
                    && let Some(shape) = event_shape(line)
                {
                    return event_row::<M, Held>(ctx, dates, plan, limit, &shape.fields(line), held, events, pays, odd_events, odd_users);
                }
                let bytes = line_end(p, len, at) - at;
                Scalar::lines::<7>(line, bytes, &mut Scratch::new(), |fields| {
                    event_row::<M, Held>(ctx, dates, plan, limit, fields, held, events, pays, odd_events, odd_users)
                });
            }
        }

        /// A click row through the row code: the line at `at` tokenized again, then
        /// `click_row`.
        ///
        /// # Safety
        /// `p, len` is a segment and `at < len` a line's first byte in it.
        #[cold]
        #[inline(never)]
        #[allow(clippy::too_many_arguments)]
        unsafe fn slow_click<M: Mode>(
            p: *const u8,
            len: usize,
            at: usize,
            ctx: &Ctx,
            dates: &Dates,
            plan: &PosPlan,
            pending: &mut Pending<PosClick>,
            clicks: &mut Chunks<PosClick>,
            odd_clicks: &mut Vec<ClickRec>,
            odd_id_clicks: &mut Vec<OddIdClick>,
            odd_ids: &mut Vec<Id>,
        ) {
            // SAFETY: one line of the segment, 64 bytes readable at its first; a shape's
            // fields are inside the line.
            unsafe {
                let line = p.add(at);
                if let Some(shape) = click_shape(line) {
                    return click_row::<M>(ctx, dates, plan, &shape.fields(line), pending, clicks, odd_clicks, odd_id_clicks, odd_ids);
                }
                let bytes = line_end(p, len, at) - at;
                Scalar::lines::<4>(line, bytes, &mut Scratch::new(), |fields| click_row::<M>(ctx, dates, plan, fields, pending, clicks, odd_clicks, odd_id_clicks, odd_ids));
            }
        }

        #[cfg(test)]
        mod tests {
            use types::time::parse_f1;

            use super::*;

            struct Rng(u64);

            impl Rng {
                fn next(&mut self) -> u64 {
                    self.0 ^= self.0 << 13;
                    self.0 ^= self.0 >> 7;
                    self.0 ^= self.0 << 17;
                    self.0
                }

                fn below(&mut self, n: u64) -> u64 {
                    self.next() % n
                }

                fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
                    options[self.below(options.len() as u64) as usize]
                }
            }

            /// An event line much as the generator writes one, then perhaps spoiled.
            fn line(rng: &mut Rng) -> Vec<u8> {
                let digits = |rng: &mut Rng, letter: char| {
                    let n = 1 + rng.below(9) as usize;
                    let mut text = String::from(letter);
                    text.extend((0..n).map(|_| char::from(b"0123456789abcdefghijklmnopqrstuvwxyz"[rng.below(36) as usize])));
                    text
                };
                let day = 1 + rng.below(31);
                let clock = format!("{:02}:{:02}:{:02}", rng.below(25), rng.below(61), rng.below(61));
                let ts = match rng.below(8) {
                    0..=3 => format!("2026-10-{day:02}T{clock}Z"),
                    4 => format!("2026-1{}-{day:02}T{clock}{}{:02}:{:02}", rng.below(3), rng.pick(&["+", "-"]), rng.below(14), rng.below(60)),
                    5 | 6 => format!("17{:011}", rng.below(100_000_000_000)),
                    _ => rng.pick(&["", "0", "2026-10-03", "2026-10-03T10:00:00", "1791071890000.0", "2026-10-03t10:00:00Z"]).to_string(),
                };
                let ingest = match rng.below(10) {
                    0 => rng.pick(&["", "2026-10-03 10:00:00Z", "1791071890000"]).to_string(),
                    _ => format!("2026-1{}-{:02}T{clock}Z", rng.below(2), 1 + rng.below(30)),
                };
                let fields = [
                    match rng.below(12) {
                        0 => rng.pick(&["", "E12ab", "e", "x1234567", "e123456789ab"]).to_string(),
                        _ => digits(rng, 'e'),
                    },
                    match rng.below(12) {
                        0 => rng.pick(&["", "U12ab", "u", "anonymous-user-17"]).to_string(),
                        _ => digits(rng, 'u'),
                    },
                    ts,
                    ingest,
                    rng.pick(&["view", "view", "view", "cart", "purchase", "refund", "", "VIEW", "views"]).to_string(),
                    rng.pick(&["p1", "p123", "p4711", "", "p99999"]).to_string(),
                    rng.pick(&["", "", "100", "2599", "0", "12a"]).to_string(),
                ];
                let mut line = Vec::new();
                for (at, field) in fields.iter().enumerate() {
                    if at > 0 {
                        line.push(b',');
                    }
                    if rng.below(4) == 0 {
                        line.push(b'"');
                        line.extend_from_slice(field.as_bytes());
                        line.push(b'"');
                    } else {
                        line.extend_from_slice(field.as_bytes());
                    }
                }
                // Spoiled: a byte replaced, inserted or removed, mostly by one that matters.
                for _ in 0..[0, 0, 0, 1, 1, 2, 5][rng.below(7) as usize] {
                    let at = rng.below(line.len() as u64 + 1) as usize;
                    let byte = b"\"\",,\n a0Z"[rng.below(9) as usize];
                    match rng.below(3) {
                        0 if at < line.len() => line[at] = byte,
                        1 if at < line.len() => drop(line.remove(at)),
                        _ => line.insert(at, byte),
                    }
                }
                line
            }

            #[test]
            fn raw_rows_are_the_scalar_fields() {
                let ctx = Ctx::new(parse_f1(b"2026-10-05T00:00:00Z").unwrap(), parse_f1(b"2026-11-02T00:00:00Z").unwrap(), &[], &[]).unwrap();
                let dates = Dates::new(&ctx);
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
                let (mut fits, mut rows) = (0, 0);
                for round in 0..60_000 {
                    // The line, then what may follow it in a file: more lines, any bytes.
                    let mut buf = vec![b'\n'];
                    buf.extend(line(&mut rng));
                    buf.push(b'\n');
                    for _ in 0..3 {
                        buf.extend(line(&mut rng));
                        buf.push(b'\n');
                    }
                    buf.resize(buf.len().max(200) + 64, b'"');
                    // SAFETY: the buffer holds whole lines and at least 263 bytes after its
                    // first, so every window and word below is inside it.
                    unsafe {
                        let p = buf.as_ptr().add(1);
                        let end = buf[1..].iter().position(|&b| b == b'\n').unwrap() + 1;
                        let mut want: Option<[Field; 7]> = None;
                        Scalar::lines::<7>(p, end, &mut Scratch::new(), |fields| want = Some(*fields));

                        let lane = round % 8;
                        let mut words = [[0x2222_2222_2222_2222u64; ROWS]; R_WORDS];
                        let next = raw_row(&mut words, lane, p);
                        if next == 0 {
                            continue;
                        }
                        rows += 1;
                        assert_eq!(next, end, "{:?}", String::from_utf8_lossy(&buf[1..end]));
                        let lanes = lanes_of(&words, 0, 0xFF, &dates);
                        let fit = lanes.fit >> lane & 1 == 1;
                        // Whatever passes `pairs` is a line `Scalar` takes, and the reverse.
                        assert_eq!(fit, want.is_some(), "{:?}", String::from_utf8_lossy(&buf[1..end]));
                        let Some([id, user, ts, ingest, kind, product, _]) = want else { continue };
                        fits += 1;
                        let got = |v: __m512i| {
                            let mut out = [0u64; 8];
                            _mm512_storeu_si512(out.as_mut_ptr().cast(), v);
                            out[lane]
                        };
                        let text = String::from_utf8_lossy(&buf[1..end]);
                        assert_eq!((got(lanes.id_first), got(lanes.id_digits), got(lanes.id_len)), (rd(id.p), rd(id.p.add(1)), id.len as u64), "id of {text:?}");
                        assert_eq!((got(lanes.user_first), got(lanes.user_digits), got(lanes.user_len)), (rd(user.p), rd(user.p.add(1)), user.len as u64), "user of {text:?}");
                        assert_eq!(
                            (got(lanes.ts_0), got(lanes.ts_8), got(lanes.ts_12), got(lanes.ts_19), got(lanes.ts_len), got(lanes.ts_date)),
                            (rd(ts.p), rd(ts.p.add(8)), rd(ts.p.add(12)), rd(ts.p.add(19)), ts.len as u64, dates.date_s(ts.p) as u64),
                            "ts of {text:?}"
                        );
                        assert_eq!(
                            (got(lanes.ingest_8), got(lanes.ingest_12), got(lanes.ingest_len), got(lanes.ingest_date)),
                            (rd(ingest.p.add(8)), rd(ingest.p.add(12)), ingest.len as u64, dates.date_s(ingest.p) as u64),
                            "ingest of {text:?}"
                        );
                        assert_eq!((got(lanes.kind) as u32, got(lanes.kind_len)), (rd(kind.p) as u32, kind.len as u64), "type of {text:?}");
                        assert_eq!(got(lanes.product_len), product.len as u64, "product of {text:?}");
                    }
                }
                assert!(fits > 20_000 && rows > fits, "{fits} well-formed of {rows} rows");
            }
        }
    }
}
