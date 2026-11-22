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
use std::sync::OnceLock;

use types::chunks::Chunks;
use types::hash::hash;
use types::id::{self, Id, pack_at};
use types::record::{ClickN, PAY_CATEGORY_SHIFT};
use types::time::DAY;
use types::users::{NO_USER, UserKey, Users, key_at_wide, long_key_at, mix_long, short_key_at};
use types::Ctx;

use crate::tok::{Field, Mask, Scalar, Scratch, Tok};
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
mod lean;

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
const DECODES: &[&str] = &["kernel", "row", "lean"];
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
    /// The `event_id`: `canon` of its `short_key_at`, or the interned `Id` of its bytes (top
    /// bit set).
    pub key: u64,
    /// The user's place in the directory (`NO_USER` for none), and above it the `ts` in
    /// milliseconds after `Ctx::origin`, or 0 for a row that is not valid (a valid `ts` is at
    /// least `Ctx::event_lo`).
    pub user_ts: u64,
    /// `Ctx::ingest_rel` (the dedup keeps the smallest), and above it the `pay` of a purchase.
    pub ingest_pay: u64,
}

/// What a narrow record's ingest counts from, in `Ctx::ingest_rel`'s seconds: 2^20 seconds,
/// 12 days, before the origin.
pub const NARROW_INGEST: u32 = (1 << 30) - (1 << 20);

/// Six bits of each byte of a word.
pub const SIX: u64 = 0x3F3F_3F3F_3F3F_3F3F;

/// The bits of `value` that `mask` names, together at the bottom.
#[inline(always)]
fn pext(value: u64, mask: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    // SAFETY: the instruction is compiled in.
    #[allow(unused_unsafe)]
    unsafe {
        core::arch::x86_64::_pext_u64(value, mask)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let (mut out, mut at, mut left) = (0u64, 0, mask);
        while left != 0 {
            out |= (value >> left.trailing_zeros() & 1) << at;
            at += 1;
            left &= left - 1;
        }
        out
    }
}

/// A short key (`short_key_at`) in 48 bits, the form every short-keyed event is hashed and
/// compared by: its digits six bits each (`0-9a-z` as 0 to 35), in their order, the last
/// highest. An interned `Id` (top bit set) stays as it is: it is no short key's 48 bits.
#[inline(always)]
pub fn canon(key: u64) -> u64 {
    if key & id::SLOW != 0 || key == 0 {
        return key;
    }
    // The digits a byte each, `0-9` as 0 to 9 and `a-z` as 49 to 74: 39 off every letter
    // (a byte is one when adding 79 sets its top bit, and no sum reaches the next byte).
    let digits = key - 1;
    let letters = ((digits + 0x4F4F_4F4F_4F4F_4F4F) >> 7) & 0x0101_0101_0101_0101;
    pext(digits - letters * 39, SIX)
}

/// One short-keyed event line that is nothing but a key, a user and two times: 16 bytes.
/// Five rows in six are such a row, and as an `Ev` each carried a `pay` of zero and a key
/// of 42 bits in 64: a third of what the parse wrote to its lists, of the fresh pages under
/// them, of what the dedup read back and of what it kept in cache.
///
/// - `a`: the key (`canon`) in the low 48 bits; above it the low 16 bits of the ingest.
/// - `b`: the user's place (25 bits), the `ts` (31 bits; 0 for a row that is not valid), the
///   ingest's next 8 bits.
///
/// The ingest is `Ctx::ingest_rel` less `NARROW_INGEST`, in 24 bits: 194 days from 12 days
/// before the origin. A row that does not fit (a purchase, a key that is an interned `Id`, a
/// place past 2^25, a `ts` past 2^31, an ingest outside those days) is written to the same
/// list as two records, an `Ev`'s three words behind a mark (`EvN::head`): a row in 25.
/// The narrow records and those rows are then one list a dedup task reads once. (In
/// lists of their own, an eighth as many, the wider rows were read by eight tasks each,
/// twice: once to count a task's share, once to take it.)
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub struct EvN {
    pub a: u64,
    pub b: u64,
}

impl EvN {
    /// The `ts` in `b`.
    pub const TS: u64 = ((1 << 31) - 1) << 25;

    /// In `a`, the six bits of a key's last digit all set: no narrow record has that (a
    /// digit is at most 35), so a record with them is the first of a wider row's two, or
    /// a pad.
    pub const HEAD: u64 = 63 << 42;

    /// A record that only fills a staging block, so that the two of a wider row lie in one
    /// (`Chunks::push_pair_at`).
    pub const PAD: EvN = EvN { a: Self::HEAD | 1, b: 0 };

    /// The first record of a wider row: its key, as `canon` gives it. The second is the
    /// row's `user_ts` and `ingest_pay`, as an `Ev` has them.
    #[inline(always)]
    pub fn head(key: u64) -> EvN {
        EvN { a: Self::HEAD, b: key }
    }

    /// Whether this is a narrow record, and not a wider row's first or a pad.
    #[inline(always)]
    pub fn is_narrow(&self) -> bool {
        self.a & Self::HEAD != Self::HEAD
    }

    #[inline(always)]
    pub fn is_pad(&self) -> bool {
        self.a == Self::PAD.a
    }

    /// The record of a row, or `None` for one that does not fit: `key`, `user_ts` and
    /// `ingest_pay` as an `Ev` has them.
    #[inline(always)]
    pub fn pack(key: u64, user_ts: u64, ingest_pay: u64) -> Option<EvN> {
        let (user, ts) = (user_ts as u32, (user_ts >> 32) as u32);
        // A row that is not valid names no user that is read.
        let user = if ts == 0 { 0 } else { user };
        let ingest = (ingest_pay as u32).wrapping_sub(NARROW_INGEST);
        if key & id::SLOW != 0 || key == 0 || ingest_pay >> 32 != 0 || user >= 1 << 25 || ts >= 1 << 31 || ingest >= 1 << 24 {
            return None;
        }
        Some(EvN { a: canon(key) | u64::from(ingest & 0xFFFF) << 48, b: u64::from(user) | u64::from(ts) << 25 | u64::from(ingest >> 16) << 56 })
    }

    /// The key, as `canon` gives it.
    #[inline(always)]
    pub fn key(&self) -> u64 {
        self.a & ((1 << 48) - 1)
    }

    /// The ingest, as `Ctx::ingest_rel` gives it.
    #[inline(always)]
    pub fn ingest(&self) -> u32 {
        ((self.a >> 48) | (self.b >> 56) << 16) as u32 + NARROW_INGEST
    }

    /// `user | ts << 32`, as an `Ev` has them.
    #[inline(always)]
    pub fn user_ts(&self) -> u64 {
        (self.b & ((1 << 25) - 1)) | ((self.b >> 25) & ((1 << 31) - 1)) << 32
    }
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

const _: () = assert!(size_of::<Ev>() == 24 && size_of::<EvN>() == 16 && size_of::<EvL>() == 32 && size_of::<Ck>() == 24 && size_of::<LinkRec>() == 24);

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
pub(crate) unsafe fn click_id_at(p: *const u8, len: usize, whole: bool) -> Id {
    // SAFETY: the caller's contract.
    unsafe {
        if len == 33 && !whole && long_key_at(p, len, b'k').is_some() {
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

/// Where a click goes once the links are joined: straight to the bucket of its person, as
/// the record the sessions stage reads (`Out::placed`).
///
/// A click was written twice: by the parse to its worker's list with its user's place, 122
/// MB at 20M events, and by the stage after it, which read that list and wrote each click to
/// its person's bucket. Which person a user is is known as soon as the links are joined, 44
/// ms into a parse of 208 at 20M events (241 of 1,100 at 60M), and the clicks' chunks are
/// the last the parse takes: a click chunk taken after that writes its clicks once. One
/// taken before it writes them to the list, as before (`ClickSink`).
///
/// What is written is a `ClickN`, 16 bytes: within its bucket a person is its number's low
/// `shift` bits, which fit a word beside the `ts` and the campaign.
pub struct Placing {
    /// Per user's number: 1 + the root of its component, 0 for a root.
    up: *const u32,
    /// The shift from a root's number to its bucket: `ClickN::SPAN_BITS` at the most.
    pub shift: u32,
    /// Buckets a number of the directory can fall in.
    pub lists: usize,
}

// SAFETY: the array is only read once a `Placing` is made of it.
unsafe impl Send for Placing {}
unsafe impl Sync for Placing {}

impl Placing {
    /// # Safety
    /// `up` holds an entry for every number the directory can give, is not written again,
    /// and outlives every use of this; `lists` is more than any number shifted by `shift`.
    pub unsafe fn new(up: *const u32, shift: u32, lists: usize) -> Placing {
        assert!(shift <= ClickN::SPAN_BITS, "a bucket of 2^{shift} persons");
        Placing { up, shift, lists }
    }

    /// The person of user `user`: its component's root.
    #[inline(always)]
    fn root(&self, user: u32) -> u32 {
        // SAFETY: a user's number is below the directory's bound, which the array has.
        let up = unsafe { *self.up.add(user as usize) };
        if up == 0 { user } else { up - 1 }
    }
}

/// Where a chunk's clicks are written: to their persons' buckets when the links were joined
/// before the chunk was taken (`Placing`), to the worker's list otherwise.
pub(crate) struct ClickSink<'a> {
    listed: &'a mut Chunks<Ck>,
    placed: Option<(&'a Placing, &'a mut Chunks<ClickN>)>,
}

impl<'a> ClickSink<'a> {
    fn new(listed: &'a mut Chunks<Ck>, placed: Option<&'a mut Chunks<ClickN>>, plan: &'a Plan) -> ClickSink<'a> {
        ClickSink { listed, placed: plan.placing.get().zip(placed) }
    }

    /// Whether `ask` does anything.
    #[inline(always)]
    pub(crate) fn places(&self) -> bool {
        self.placed.is_some()
    }

    /// Asks for the cache line that says which person user `user` is.
    #[inline(always)]
    pub(crate) fn ask(&self, user: u32) {
        if let Some((placing, _)) = &self.placed {
            types::mem::prefetch(placing.up.wrapping_add(user as usize));
        }
    }

    /// One valid click of user `user`; `campaign` is below `ClickN::CAMPAIGNS` where clicks
    /// are placed.
    ///
    /// # Safety
    /// `user` is a number the directory gave.
    #[inline(always)]
    pub(crate) unsafe fn push(&mut self, click: Id, user: u32, ts: u32, campaign: u32) {
        // SAFETY: a root is a number of the directory, so its bucket is below `lists`, which
        // the placed lists have; the worker's list is list 0 of one.
        unsafe {
            match &mut self.placed {
                Some((placing, placed)) => {
                    let root = placing.root(user);
                    // The person within its bucket: the number's low `shift` bits.
                    placed.push_at((root >> placing.shift) as usize, ClickN::new(click, root & ((1 << placing.shift) - 1), campaign, ts));
                }
                None => self.listed.push_at(0, Ck { click, user_ts: u64::from(user) | u64::from(ts) << 32, campaign: u64::from(campaign) }),
            }
        }
    }

    /// Streamed lines become visible to the threads that read the lists.
    pub(crate) fn fence(&self) {
        self.listed.fence();
        if let Some((_, placed)) = &self.placed {
            placed.fence();
        }
    }
}

/// The run's directory and the shape of the lists.
pub struct Plan {
    pub users: Users,
    /// Lists of short-keyed events = `1 << part_bits`: a record's is the top bits of
    /// `types::hash::hash(canon(key))`.
    pub part_bits: u32,
    /// Lists of long-keyed events = `1 << long_bits`: the top bits of `mix_long(hi, lo)`.
    pub long_bits: u32,
    /// Lines decoded by the kernel (`DECODES`).
    kernel: bool,
    /// Event lines by the lean kernel (`lean`).
    lean: bool,
    /// Long `click_id`s kept whole (`click_id_at`).
    pub(crate) whole_clicks: bool,
    /// Whether rows may be narrow records at all (`SS_RECORDS=narrow|wide`): every place of
    /// the directory is below 2^25. Where not, every short-keyed row is an `Ev` in
    /// `Out::events`, as before there were narrow records.
    pub narrow: bool,
    /// Set by the stage's caller once the links are joined, when clicks are to be written
    /// to their persons' buckets (`Placing`).
    pub placing: OnceLock<Placing>,
    /// A click chunk waits for `placing` (`SS_CLICKS=wait`, for tests: every click is then
    /// placed, however short the parse). Only set when `placing` will be.
    pub wait_for_placing: bool,
    /// `k & ask_step == 0` after a row is when the kernel asks for the directory's lines of
    /// the last eight rows' users (`SS_ASK=rows|batch`): 63 for `rows`, the pick; for
    /// `batch` every bit, which no count of rows clears, and a batch's users are asked for
    /// when it is full.
    pub(crate) ask_step: usize,
    /// The row code's single lines tokenized from masks (`SS_SLOW_TOK=mask|scalar`, `mask`
    /// the pick; `kern::one_line`), not by `Scalar`'s byte loop.
    #[allow(dead_code)]
    pub(crate) slow_mask: bool,
}

impl Plan {
    /// The list of the short-keyed row with key `key`, as `canon` has it.
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
    /// spelled the long way, valid or not: a narrow record, or two records for a row that
    /// is none (`push_wide`). Empty where no row may be narrow.
    pub narrow: Chunks<EvN>,
    /// List `Plan::part(key)`: the same lines as an `Ev` each, where no row may be narrow
    /// (`Plan::narrow`); empty otherwise.
    pub events: Chunks<Ev>,
    /// List `Plan::long_part(hi, lo)`: the others.
    pub long_events: Chunks<EvL>,
    /// One list: the valid clicks of the chunks taken before the links were joined.
    pub clicks: Chunks<Ck>,
    /// List `Placing`'s bucket of the click's person: the valid clicks of every other
    /// chunk, as the sessions stage reads them. Made by the first such chunk.
    pub placed: Option<Chunks<ClickN>>,
    /// Bytes a worker's share of the clicks takes as placed records.
    placed_room: usize,
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
        self.narrow.release();
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
        let users = Users::for_rows(self.event_rows + self.click_rows + self.link_rows, grow);
        let narrow = users.bound() <= 1 << 25 && types::env::pick("SS_RECORDS", &["narrow", "wide"]) == 0;
        Plan {
            users,
            part_bits,
            narrow,
            long_bits: part_bits.saturating_sub(3).max(1),
            kernel: DECODES[types::env::pick("SS_DECODE", DECODES)] != "row",
            lean: DECODES[types::env::pick("SS_DECODE", DECODES)] == "lean",
            whole_clicks,
            placing: OnceLock::new(),
            wait_for_placing: false,
            ask_step: if types::env::pick("SS_ASK", &["rows", "batch"]) == 0 { 63 } else { usize::MAX },
            slow_mask: types::env::pick("SS_SLOW_TOK", &["mask", "scalar"]) == 0,
        }
    }

    /// A worker's lists and buffers, for `workers` workers.
    pub fn opaque_out(&self, plan: &Plan, workers: usize) -> Out {
        // A worker's block has room for four times its share of the lines (it is address
        // space until written), a chunk a list and a huge page more.
        let room = |rows: usize, record: usize, lists: usize| (rows + rows / 8) * record * 4 / workers.max(4) + lists * 4096 + (2 << 20);
        let (parts, long_parts) = (1usize << plan.part_bits, 1usize << plan.long_bits);
        Out {
            narrow: Chunks::new(parts, room(self.event_rows, size_of::<EvN>(), parts)),
            // Used only when no row may be narrow.
            events: Chunks::new(parts, room(if plan.narrow { 0 } else { self.event_rows }, size_of::<Ev>(), parts)),
            long_events: Chunks::new(long_parts, room(self.event_rows / 4, size_of::<EvL>(), long_parts)),
            clicks: Chunks::new(1, room(self.click_rows, size_of::<Ck>(), 1)),
            placed: None,
            placed_room: room(self.click_rows, size_of::<ClickN>(), 0),
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
            if *click {
                // `SS_CLICKS=wait`: the links' task was taken before this one, and sets it.
                while plan.wait_for_placing && plan.placing.get().is_none() {
                    std::hint::spin_loop();
                }
                // The buckets' lists, when this is the worker's first chunk that places.
                if out.placed.is_none() && let Some(placing) = plan.placing.get() {
                    out.placed = Some(Chunks::new(placing.lists, out.placed_room + placing.lists * 4096));
                }
                clicks(p, len, ctx, &self.dates, &self.campaigns, plan, out)
            } else {
                events(p, len, ctx, &self.dates, plan, out)
            }
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
            links.extend(held.iter().map(|&(keys, order)| LinkRec { users: [users.get(keys[0]), users.get(keys[1])], order }));
        };
        // SAFETY: a segment, and a segment's fields have 16 bytes readable after them.
        // (The lines by their masks and the keys a word at a time: through the byte-by-byte
        // tokenizer and `key_at`, 0.9M link lines were 4.7% of a run's instructions at 20M
        // events, 730 a line.)
        unsafe {
            Mask::lines::<2>(p, len, &mut out.scratch, |&[a, b]| {
                if a.len != 0 && b.len != 0 {
                    held[count] = ([key_at_wide(a.p, a.len), key_at_wide(b.p, b.len)], [order_at(a.p, a.len), order_at(b.p, b.len)]);
                    count += 1;
                    if count == LINKS_HELD {
                        flush(&held, &mut links);
                        count = 0;
                    }
                }
            })
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
        return unsafe { if plan.lean { lean::events(p, len, lead, ctx, dates, plan, out) } else { kern::events(p, len, lead, ctx, dates, plan, out) } };
    }
    let Out { narrow, events, long_events, scratch, .. } = out;
    unsafe { Scalar::lines::<7>(p, len, scratch, |fields| event_row(ctx, dates, plan, fields, narrow, events, long_events)) };
    narrow.fence();
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
        return unsafe { kern::clicks(p, len, lead, ctx, dates, campaigns, plan, out) };
    }
    let _ = campaigns;
    let Out { clicks, placed, scratch, .. } = out;
    let mut sink = ClickSink::new(clicks, placed.as_mut(), plan);
    unsafe { Scalar::lines::<4>(p, len, scratch, |fields| click_row(ctx, dates, plan, fields, &mut sink)) };
    sink.fence();
}

/// One event line as its record, for any spelling: the checks of `crate::event_row`.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn event_row(ctx: &Ctx, dates: &Dates, plan: &Plan, fields: &[Field; 7], narrow: &mut Chunks<EvN>, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>) {
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
    let place = if valid { unsafe { plan.users.at(user.p, user.len) } } else { NO_USER };
    let (user_ts, ingest_pay) = words(place, ts as u32, valid, ingest_rel, pay);
    // SAFETY: the fields' bytes are readable; a list is below the lists' count by its bits.
    unsafe {
        if let Some((hi, lo)) = long_key_at(id.p, id.len, b'e') {
            long_events.push_at(plan.long_part(hi, lo), EvL { hi, lo, user_ts, ingest_pay });
        } else {
            let key = canon(short_key_at(id.p, id.len, b'e').unwrap_or_else(|| id::intern_any(std::slice::from_raw_parts(id.p, id.len))));
            push_wide(plan, narrow, events, key, user_ts, ingest_pay);
        }
    }
}

/// A short-keyed row that is not written as a narrow record (a purchase, a line the row
/// code read, an ingest a narrow record cannot hold): two records in the narrow records'
/// list of its key, or an `Ev` where no row may be narrow. `key` as `canon` gives it.
///
/// # Safety
/// The lists are the plan's.
#[inline(always)]
pub(crate) unsafe fn push_wide(plan: &Plan, narrow: &mut Chunks<EvN>, events: &mut Chunks<Ev>, key: u64, user_ts: u64, ingest_pay: u64) {
    let list = plan.part(key);
    // SAFETY: a list is below the lists' count by its bits.
    unsafe {
        if plan.narrow { narrow.push_pair_at(list, EvN::PAD, EvN::head(key), EvN { a: user_ts, b: ingest_pay }) } else { events.push_at(list, Ev { key, user_ts, ingest_pay }) }
    }
}

/// One click line as its record, for any spelling: the checks of `crate::click_row`.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn click_row(ctx: &Ctx, dates: &Dates, plan: &Plan, fields: &[Field; 4], sink: &mut ClickSink) {
    let [click, user, campaign, ts] = *fields;
    let (ok, ts) = unsafe { dates.ts(ts.p, ts.len) };
    if (click.len != 0) & (user.len != 0) & ok & (ts >= 0) & (ts < i64::from(ctx.ts_end))
        && let Some(campaign) = ctx.campaign_index.get(unsafe { pack_at(campaign.p, campaign.len) })
    {
        // SAFETY: the fields' bytes are readable; the user is not empty, so it has a number.
        unsafe {
            let place = plan.users.at(user.p, user.len);
            sink.push(click_id_at(click.p, click.len, plan.whole_clicks), place, ts as u32, campaign);
        }
    }
}

#[cfg(test)]
mod tests {
    use types::time::parse_f1;

    use super::*;

    /// A narrow record gives back the row it was made of, takes only rows it can hold, and
    /// two keys are one `canon` only when they are one key.
    #[test]
    fn narrow_records_come_back() {
        let ids: [&[u8]; 12] = [b"e0", b"e1", b"ez", b"e10", b"e1a0", b"ezzzzzzzz", b"e4k2nn2cj", b"e10000000", b"ea0b0c0d0", b"e9z9z9z9z", b"ea", b"e99999999"];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            let mut bytes = [0u8; 48];
            bytes[..id.len()].copy_from_slice(id);
            // SAFETY: 48 bytes are readable.
            let key = unsafe { short_key_at(bytes.as_ptr(), id.len(), b'e') }.unwrap();
            assert!(seen.insert(canon(key)), "{:?}", std::str::from_utf8(id));
            assert!(canon(key) < 1 << 48);
            // The digits come back out of the 48 bits: six bits each, the last highest.
            let digits: Vec<u8> = (0..8).map(|at| (canon(key) >> (6 * at) & 63) as u8).map(|value| if value < 10 { b'0' + value } else { b'a' + value - 10 }).collect();
            assert_eq!(&digits[8 - (id.len() - 1)..], &id[1..], "{:?}", std::str::from_utf8(id));
            for (user, ts, ingest) in [(0u32, 0u32, NARROW_INGEST), (1, 1, NARROW_INGEST + 1), ((1 << 25) - 1, (1 << 31) - 1, NARROW_INGEST + (1 << 24) - 1), (77, 1_234_567_890, (1 << 30) + 86_400 * 9 + 5), (NO_USER, 0, 1 << 30)] {
                let (user_ts, ingest_pay) = (u64::from(user) | u64::from(ts) << 32, u64::from(ingest));
                let row = EvN::pack(key, user_ts, ingest_pay).unwrap_or_else(|| panic!("{user} {ts} {ingest}"));
                // A row with no `ts` keeps no user.
                let kept = if ts == 0 { 0 } else { user };
                assert_eq!((row.key(), row.user_ts(), row.ingest()), (canon(key), u64::from(kept) | u64::from(ts) << 32, ingest));
                assert_eq!(row.b & EvN::TS, u64::from(ts) << 25);
            }
            // What does not fit: a purchase, a place, a `ts` or an ingest too wide.
            assert_eq!(EvN::pack(key, 5 | 9 << 32, u64::from(1u32 << 30) | 1 << 32), None);
            assert_eq!(EvN::pack(key, 1 << 25 | 9 << 32, 1 << 30), None);
            assert_eq!(EvN::pack(key, 5 | 1 << 63, 1 << 30), None);
            assert_eq!(EvN::pack(key, 5 | 9 << 32, u64::from(NARROW_INGEST - 1)), None);
            assert_eq!(EvN::pack(key, 5 | 9 << 32, u64::from(NARROW_INGEST + (1 << 24))), None);
        }
        assert_eq!(EvN::pack(id::SLOW | 5, 5 | 9 << 32, 1 << 30), None, "an interned key");
        assert_eq!(canon(id::SLOW | 5), id::SLOW | 5);
    }


    /// Lists of both key widths take a row by its `event_id`'s spelling, and an invalid row
    /// names no user.
    #[test]
    fn rows_go_by_their_ids_spelling() {
        let ctx = Ctx::new(parse_f1(b"2026-10-08T00:00:00Z").unwrap(), parse_f1(b"2026-10-22T00:00:00Z").unwrap(), &[], &[]).unwrap();
        let dates = Dates::new(&ctx);
        let plan = Plan { users: Users::new(64, 64), part_bits: 3, long_bits: 1, kernel: false, lean: false, whole_clicks: false, narrow: false, placing: OnceLock::new(), wait_for_placing: false, ask_step: 63, slow_mask: true };
        let (mut narrow, mut events, mut long_events) = (Chunks::<EvN>::new(8, 1 << 16), Chunks::<Ev>::new(8, 1 << 16), Chunks::<EvL>::new(2, 1 << 16));
        let text = b"\ne1,u1,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,view,p1,\n\
e02423c3d84ab053af78764ce49f23b5c,u02423c3d84ab053af78764ce49f23b5c,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,cart,p1,\n\
E1,U1,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,view,p1,\n\
e2,u1,2026-10-09T10:00:00Z,2026-10-09T10:00:05Z,refund,p1,\n";
        let mut buf = text.to_vec();
        buf.extend([0u8; 64]);
        // SAFETY: whole lines after the first byte, 64 bytes readable after them.
        unsafe { Scalar::lines::<7>(buf.as_ptr().add(1), text.len() - 1, &mut Scratch::new(), |fields| event_row(&ctx, &dates, &plan, fields, &mut narrow, &mut events, &mut long_events)) };
        assert!(narrow.is_empty(), "no row is narrow under this plan");
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
