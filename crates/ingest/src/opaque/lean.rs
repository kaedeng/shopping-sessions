//! The lean line kernel: one straight loop for every event line, the rows sorted by kind a
//! batch at a time, each kind finished in a loop of its own that writes the record to its
//! list.
//!
//! What a line costs beyond its own decoding is what this file is about:
//!
//! - **No jump on what a line holds.** `kern` asks first whether both IDs are short and takes
//!   one of two paths; one line in four has a long ID, at random, and the processor guesses
//!   that wrong about as often. Here every line goes through the same instructions: its end
//!   from one compare of the 64 bytes that begin 49 into it (every line with two short IDs
//!   ends there), each ID decoded on its own as a short one (`SHORT_E`, `SHORT_U`: 20 rows of
//!   32 bytes in use, where the table of both IDs' shapes together is 266 KB), everything
//!   after the `user_id` by `kern::tail`, which never depended on the IDs. A row's word says
//!   what came out: which ID left a residue, and the residues of the rest.
//! - **Rows sorted, then finished kind by kind.** A batch's words are sorted eight at a time
//!   by masks (`split`): both IDs short, the same with a purchase, a `user_id` that is not
//!   short, an `event_id` that is not, both, and what the row code has to read. Each kind
//!   is then a loop with nothing to decide in it, and an ID that is not short is looked at
//!   as a long one only there.
//! - **A record is written once.** What a record holds beside its user is made for eight
//!   rows at a time in registers (`prepare`); the loop of a kind looks the user up and puts
//!   the record in its list's staging block. No second batch, no held records. Five rows
//!   in six become a narrow record of 16 bytes (`EvN`); a purchase, and a row whose ingest
//!   a narrow record cannot hold, two records in the same list (`super::push_wide`).
//! - **One batch of 64 rows**, 7 KB with its lists (5 KB of it in use), where `kern` keeps two batches and the
//!   held records (12 KB) in a first-level cache that two workers share on the scorer. A
//!   short user's line of the directory is asked for eight rows at a time as they are
//!   decoded, a long user's when its batch is sorted; the rows with two short IDs, three in
//!   four, are finished after the others, so every line has had their work to arrive in.
//!
//! Which lines are taken, and why a taken line is exactly one well-formed line, is as in
//! `kern`: the same residues, from the same tables.

use core::arch::x86_64::*;

use types::Ctx;
use types::chunks::Chunks;
use types::time::DAY;
use types::users::{ShortLines, Users};

use super::kern::{Lanes, Memo, ONE_E, ONE_U, OneRow, PURCHASE, line_end, mul_halves, one_id, one_rows, purchase, slow_line, tail};
use super::{Ev, EvL, EvN, NARROW_INGEST, Out, Plan, SIX};
use crate::ts::Dates;

/// Bytes the kernel may read from a line's first byte: the second window of the search for
/// its end, and a `ts` found 64 bytes after a `user_id` that ends 96 bytes in.
const WINDOW: usize = 224;
/// Where the search for a line's end begins: no line with every field present is shorter.
const NEAR: usize = 49;
/// Rows decoded before they are finished; a multiple of 8.
const BATCH: usize = 64;

/// A row's word: bits 0 to 3 say which half of which ID left a residue (the `event_id`'s
/// two, then the `user_id`'s), bits 4 to 7 the same for what follows the IDs; then `BUY`
/// `LATE` and `WIDE`, and in the high half the residues of the last two fields.
const E_BAD: u64 = 0x03;
const U_BAD: u64 = 0x0C;
const REST_BAD: u64 = 0xFFFF_FFFF_0000_00F0;
/// The type is `purchase`.
const BUY: u64 = PURCHASE >> 8;
/// An ID that is neither short nor long: the row code's.
const LATE: u64 = 1 << 25;
/// The row is no narrow record (`prepare`): its ingest does not fit one, or no row may be.
const WIDE: u64 = 1 << 26;
const _: () = assert!(BUY == 1 << 24);

/// The kinds of rows, in the order their lists are in.
const SHORT: usize = 0;
const BOUGHT: usize = 1;
const LONG_USER: usize = 2;
const LONG_EVENT: usize = 3;
const LONG_BOTH: usize = 4;
const ROW_CODE: usize = 5;

/// `kern::one_rows` by any width a line's first commas can give, so that no width has to be
/// clamped: a width of 16 or more is no short ID's.
const fn wide_rows(letter: u8) -> [OneRow; 128] {
    let narrow = one_rows(letter);
    let mut table = [narrow[15]; 128];
    let mut raw = 0;
    while raw < 16 {
        table[raw] = narrow[raw];
        raw += 1;
    }
    table
}

static SHORT_E: [OneRow; 128] = wide_rows(b'e');
static SHORT_U: [OneRow; 128] = wide_rows(b'u');

/// What the kernel leaves of a batch's rows.
#[repr(C, align(64))]
struct Batch {
    /// The `event_id` (its digits, each less `'0'`, or the first word of a long one), the
    /// `ts`, the `user_id` the same way, the `ingest_ts`; the times in milliseconds after
    /// `Ctx::origin`.
    rows: [[u64; 4]; BATCH],
    words: [u64; BATCH],
    /// Where each row's line begins, and one more: where the last one ends.
    lines: [*const u8; BATCH + 8],
    /// What `prepare` makes of a row: a short `event_id`'s digits of six bits a byte
    /// (`_pext_u64` of them by `SIX` is `super::canon` of the key); as an `Ev` has them,
    /// the `ts` above the user's place when the row is valid, and `Ctx::ingest_rel`; and
    /// as an `EvN` has them, what is above the key in its first word, and its second
    /// word without the user's place.
    keys: [u64; BATCH],
    stamps: [u64; BATCH],
    ingests: [u64; BATCH],
    firsts: [u64; BATCH],
    seconds: [u64; BATCH],
    /// The second words of a row's long IDs: the event's, the user's.
    lows: [[u64; 2]; BATCH],
    /// The rows of each kind, and how many.
    of: [[u32; BATCH + 8]; 6],
    len: [usize; 6],
}

/// What finishing a row needs, the same for every row of a chunk.
struct Env<'a> {
    ctx: &'a Ctx,
    dates: &'a Dates,
    plan: &'a Plan,
    users: &'a Users,
    short: ShortLines,
    /// The segment the rows' lines are in.
    base: *const u8,
    bytes: usize,
    event_lo: i64,
    ts_end: i64,
    /// The earliest midnight of the date table, and `Ctx::ingest_rel` of it.
    first_ms: i64,
    ingest_add: i64,
    /// 64 less the lists' bits: a key's list is the top bits of its hash.
    shift: u32,
    /// Whether a row may be a narrow record: the plan allows it, and every valid `ts` is
    /// below 2^31.
    narrow: bool,
}

impl Env<'_> {
    /// The number of the user whose short key is `key`.
    #[inline(always)]
    fn user(&self, key: u64) -> u32 {
        let found = self.short.hit(key);
        if found != 0 { found - 1 } else { first(self.users, key) }
    }
}

/// A user's first lookup, or one that has to wait or walk on.
#[cold]
#[inline(never)]
fn first(users: &Users, key: u64) -> u32 {
    users.short(key)
}

/// The comma after the `user_id` of a line whose first 64 bytes hold one comma: two long
/// IDs. A function of its own: written where it is used, the compiler loaded these bytes
/// for every line.
///
/// # Safety
/// 96 bytes readable at `line`.
#[cold]
#[inline(never)]
unsafe fn far_comma(line: *const u8) -> usize {
    // SAFETY: the caller's contract.
    unsafe { 64 + (_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(line.add(64).cast()), _mm256_set1_epi8(b',' as i8))) as u32).trailing_zeros() as usize }
}

/// Where the line at `line` ends when that is past the first window of the search, or 0:
/// a line with a long ID and much else, a few in a hundred.
///
/// # Safety
/// `NEAR + 128` bytes readable at `line`.
#[cold]
#[inline(never)]
unsafe fn far_end(line: *const u8) -> usize {
    // SAFETY: the caller's contract.
    unsafe {
        let far = _mm512_cmpeq_epi8_mask(_mm512_loadu_si512(line.add(NEAR + 64).cast()), _mm512_set1_epi8(b'\n' as i8));
        if far == 0 { 0 } else { NEAR + 64 + far.trailing_zeros() as usize }
    }
}

/// The commas after a line's two IDs.
///
/// # Safety
/// 96 bytes readable at `line`.
#[inline(always)]
unsafe fn commas_of(line: *const u8) -> (usize, usize) {
    // SAFETY: the caller's contract.
    unsafe {
        let commas = _mm512_cmpeq_epi8_mask(_mm512_loadu_si512(line.cast()), _mm512_set1_epi8(b',' as i8));
        let rest = commas & commas.wrapping_sub(1);
        (commas.trailing_zeros() as usize, if rest != 0 { rest.trailing_zeros() as usize } else { far_comma(line) })
    }
}

/// One event chunk.
///
/// # Safety
/// `p, len` is a segment, and `lead` bytes before `p` are readable.
#[inline(never)]
pub(super) unsafe fn events(p: *const u8, len: usize, lead: usize, ctx: &Ctx, dates: &Dates, plan: &Plan, out: &mut Out) {
    let Out { narrow, events, long_events, slow, .. } = out;
    let memo = Memo::of(ctx, dates);
    let lanes = Lanes::new();
    let env = Env {
        ctx,
        dates,
        plan,
        users: &plan.users,
        short: plan.users.short_lines(),
        base: p,
        bytes: len,
        event_lo: i64::from(ctx.event_lo),
        ts_end: i64::from(ctx.ts_end),
        first_ms: memo.first_ms,
        ingest_add: (memo.first_ms + ctx.origin).div_euclid(1000) - ctx.ingest_base(),
        shift: 64 - plan.part_bits,
        narrow: plan.narrow && u64::from(ctx.ts_end) <= 1 << 31,
    };
    // SAFETY: plain integers and pointers that are not read before they are written.
    let mut batch: Batch = unsafe { std::mem::zeroed() };
    // SAFETY: a line the kernel runs on begins at `line < stop`, at least `WINDOW - PAD`
    // bytes before the segment's end, which is a newline: the first newline from 49 bytes in
    // is inside the segment, and everything read lies between 16 bytes before the line and
    // `WINDOW` after its first byte, readable with the `PAD` bytes after the segment. Table
    // indexes are bounded by the counts they are made of.
    unsafe {
        let last = p.add(len);
        // The kernel reads up to 16 bytes before a line.
        let start = p.add(16usize.saturating_sub(lead).min(len));
        let stop = p.add((len + crate::PAD).saturating_sub(WINDOW));
        let days = memo.days.as_ptr().cast::<u8>();
        let (comma, newline) = (_mm512_set1_epi8(b',' as i8), _mm512_set1_epi8(b'\n' as i8));
        let short = env.short;
        let mut line = p;
        while line < last {
            if line >= start && line < stop {
                let mut k = 0usize;
                loop {
                    let commas = _mm512_cmpeq_epi8_mask(_mm512_loadu_si512(line.cast()), comma);
                    // The line's end, from its own address: the one thing the next line waits for.
                    let near = _mm512_cmpeq_epi8_mask(_mm512_loadu_si512(line.add(NEAR).cast()), newline);
                    let mut reach = NEAR + near.trailing_zeros() as usize;
                    if near == 0 {
                        reach = far_end(line);
                        if reach == 0 {
                            break;
                        }
                    }
                    let end = line.add(reach);
                    let c0 = commas.trailing_zeros() as usize;
                    let rest = commas & commas.wrapping_sub(1);
                    let c1 = if rest != 0 { rest.trailing_zeros() as usize } else { far_comma(line) };
                    let t = line.add(c1);
                    let gap = 1 + (_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(t.add(1).cast()), lanes.comma)) as u32).trailing_zeros() as usize;
                    let rest_of = tail(t, gap, end, days, &lanes);

                    // Each ID as a short one, in the 16 bytes before its comma: a width that
                    // is none of a short ID's picks a row that leaves a residue.
                    let (e_shape, u_shape) = (&*SHORT_E.as_ptr().add(c0), &*SHORT_U.as_ptr().add(c1.wrapping_sub(c0 + 1) & 127));
                    let e = _mm_sub_epi8(_mm_shuffle_epi8(_mm_loadu_si128(line.add(c0).sub(16).cast()), _mm_load_si128(e_shape.ctrl.as_ptr().cast())), _mm_load_si128(e_shape.base.as_ptr().cast()));
                    let u = _mm_sub_epi8(_mm_shuffle_epi8(_mm_loadu_si128(t.sub(16).cast()), _mm_load_si128(u_shape.ctrl.as_ptr().cast())), _mm_load_si128(u_shape.base.as_ptr().cast()));
                    let ids = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(e), u);
                    let ids_bad = _mm256_min_epu8(_mm256_subs_epu8(_mm256_add_epi8(ids, lanes.to_letter), lanes.letter_max), _mm256_subs_epu8(ids, lanes.digit_max));
                    // Which quarter of the IDs' residues and of the rest's is not zero.
                    let residues = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(ids_bad), rest_of.bad);
                    let dirty = u64::from(_mm512_test_epi64_mask(residues, residues));
                    // The digits' halves of the IDs beside the times: event, ts, user, ingest.
                    let stored = _mm256_castpd_si256(_mm256_shuffle_pd::<0b0101>(_mm256_castsi256_pd(ids), _mm256_castsi256_pd(rest_of.times)));

                    _mm256_store_si256(batch.rows.as_mut_ptr().add(k).cast(), stored);
                    *batch.words.get_unchecked_mut(k) = dirty | rest_of.class >> 8 | u64::from(rest_of.low) << 32;
                    *batch.lines.get_unchecked_mut(k) = line;
                    k += 1;
                    line = end.add(1);
                    // The directory's lines of the last eight rows' short users: asked for
                    // together, as the rows are decoded.
                    if k & 7 == 0 {
                        ask(&batch, k - 8, k, short);
                        if k == BATCH {
                            batch.lines[BATCH] = line;
                            finish(&mut batch, BATCH, &env, &lanes, narrow, events, long_events, slow);
                            k = 0;
                        }
                    }
                    if line >= stop {
                        break;
                    }
                }
                // The rows so far end where this line begins.
                if k != 0 {
                    batch.lines[k] = line;
                    ask(&batch, k & !7, k, short);
                    finish(&mut batch, k, &env, &lanes, narrow, events, long_events, slow);
                }
            }
            if line < last {
                // A line with no end in reach, or one of the segment's last or first.
                *slow += 1;
                let at = line.offset_from(p) as usize;
                slow_line(p, len, at, ctx, dates, plan, narrow, events, long_events);
                line = p.add(line_end(p, len, at));
            }
        }
    }
    // Streamed lines become visible to the threads that read the lists.
    narrow.fence();
    events.fence();
    long_events.fence();
}

/// Asks for the directory's lines of the short users of rows `from..to`; a row whose
/// `user_id` is not short asks for one line that stays where it is.
///
/// # Safety
/// The rows are stored, `to <= BATCH`.
#[inline(always)]
unsafe fn ask(batch: &Batch, from: usize, to: usize, short: ShortLines) {
    // SAFETY: the caller's contract.
    unsafe {
        for row in from..to {
            let digits = batch.rows.get_unchecked(row)[2];
            let key = if *batch.words.get_unchecked(row) & U_BAD == 0 { digits + 1 } else { 1 };
            types::mem::prefetch(short.line(key));
        }
    }
}

/// What the records of a batch's first `rows` rows hold beside their users, eight rows at a
/// time: `Batch::keys`, `stamps`, `ingests`, `firsts`, `seconds`, and `WIDE` in the word of
/// a row that is no narrow record. A row that is no record gets what its bytes happen to
/// give, and nothing reads it.
#[inline(always)]
unsafe fn prepare(batch: &mut Batch, rows: usize, env: &Env) {
    // SAFETY: register arithmetic; loads of eight rows that begin below `BATCH` at a
    // multiple of 8, stores of eight words at the same place.
    unsafe {
        let all = |value: i64| _mm512_set1_epi64(value);
        // Two rows a register: event, ts, user, ingest, twice. From two registers, the four
        // events then the four `ts`; and the four `ingest_ts` in the upper half.
        let (even, odd) = (_mm512_setr_epi64(0, 4, 8, 12, 1, 5, 9, 13), _mm512_setr_epi64(2, 6, 10, 14, 3, 7, 11, 15));
        let (event_lo, ts_end, first_ms, ingest_add) = (all(env.event_lo), all(env.ts_end), all(env.first_ms), all(env.ingest_add));
        let mut first = 0;
        while first < rows {
            let at = batch.rows.as_ptr().add(first).cast::<__m512i>();
            let (r0, r1, r2, r3) = (_mm512_load_si512(at), _mm512_load_si512(at.add(1)), _mm512_load_si512(at.add(2)), _mm512_load_si512(at.add(3)));
            let (low_a, low_b) = (_mm512_permutex2var_epi64(r0, even, r1), _mm512_permutex2var_epi64(r2, even, r3));
            let (high_a, high_b) = (_mm512_permutex2var_epi64(r0, odd, r1), _mm512_permutex2var_epi64(r2, odd, r3));
            let event = _mm512_shuffle_i64x2::<0b01_00_01_00>(low_a, low_b);
            let ts = _mm512_shuffle_i64x2::<0b11_10_11_10>(low_a, low_b);
            let ingest = _mm512_shuffle_i64x2::<0b11_10_11_10>(high_a, high_b);
            // `event_row`'s `valid` for a row the kernel took: the window, and not late.
            let valid = _mm512_cmpge_epi64_mask(ts, event_lo) & _mm512_cmplt_epi64_mask(ts, ts_end) & _mm512_cmple_epi64_mask(_mm512_sub_epi64(ingest, ts), all(DAY));
            // `Ctx::ingest_rel`: whole seconds since the table's first midnight (under 2^32
            // ms, so the product's high bits are the quotient by 1000), then from its base.
            let seconds = _mm512_srli_epi64::<38>(mul_halves(_mm512_sub_epi64(ingest, first_ms), _mm256_set1_epi64x(274_877_907)));
            let ingest_rel = _mm512_add_epi64(seconds, ingest_add);
            // The key's digits with 39 off every letter (`a-z` are 49 to 74 here, and 10 to
            // 35 after): six bits a byte, put together where the record is written.
            let key = _mm512_mask_sub_epi8(event, _mm512_cmpgt_epi8_mask(event, _mm512_set1_epi8(9)), event, _mm512_set1_epi8(39));
            let kept = _mm512_maskz_mov_epi64(valid, ts);
            // A narrow record's ingest counts from `NARROW_INGEST`, in 24 bits: the low 16
            // above the key, the next 8 above the place and the `ts`.
            let since = _mm512_sub_epi64(ingest_rel, all(i64::from(NARROW_INGEST)));
            let fits: __mmask8 = if env.narrow { _mm512_cmplt_epu64_mask(since, all(1 << 24)) } else { 0 };
            _mm512_store_si512(batch.keys.as_mut_ptr().add(first).cast(), key);
            _mm512_store_si512(batch.stamps.as_mut_ptr().add(first).cast(), _mm512_slli_epi64::<32>(kept));
            _mm512_store_si512(batch.ingests.as_mut_ptr().add(first).cast(), ingest_rel);
            _mm512_store_si512(batch.firsts.as_mut_ptr().add(first).cast(), _mm512_slli_epi64::<48>(since));
            _mm512_store_si512(batch.seconds.as_mut_ptr().add(first).cast(), _mm512_or_si512(_mm512_slli_epi64::<25>(kept), _mm512_slli_epi64::<56>(_mm512_srli_epi64::<16>(since))));
            let word = batch.words.as_mut_ptr().add(first).cast::<__m512i>();
            _mm512_store_si512(word, _mm512_mask_or_epi64(_mm512_load_si512(word), !fits, _mm512_load_si512(word), all(WIDE as i64)));
            first += 8;
        }
    }
}

/// The first `rows` rows of a batch by kind: `Batch::of` and `Batch::len`.
///
/// Eight rows a step, by masks, each kind's rows compressed in a register and stored whole:
/// the lanes past a kind's rows are overwritten by the next store.
#[inline(always)]
unsafe fn split(batch: &mut Batch, rows: usize) {
    // SAFETY: register arithmetic, loads of eight words that begin below `BATCH` at a
    // multiple of 8, and stores of eight numbers into a list at its length, at most `BATCH`.
    unsafe {
        let all = |value: u64| _mm512_set1_epi64(value as i64);
        let mut len = [0usize; 6];
        let mut number = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
        let mut first = 0;
        while first < rows {
            let live: __mmask8 = if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 };
            let word = _mm512_load_si512(batch.words.as_ptr().add(first).cast());
            let sound = live & _mm512_testn_epi64_mask(word, all(REST_BAD));
            let event_short = _mm512_testn_epi64_mask(word, all(E_BAD));
            let user_short = _mm512_testn_epi64_mask(word, all(U_BAD));
            // A purchase, or a row a narrow record does not hold: the wider record.
            let buy = _mm512_test_epi64_mask(word, all(BUY | WIDE));
            let short = sound & event_short & user_short;
            let kinds = [short & !buy, short & buy, sound & event_short & !user_short, sound & !event_short & user_short, sound & !event_short & !user_short, live & !sound];
            for (kind, &mask) in kinds.iter().enumerate() {
                _mm256_storeu_si256(batch.of.get_unchecked_mut(kind).as_mut_ptr().add(*len.get_unchecked(kind)).cast(), _mm256_maskz_compress_epi32(mask, number));
                *len.get_unchecked_mut(kind) += mask.count_ones() as usize;
            }
            number = _mm256_add_epi32(number, _mm256_set1_epi32(8));
            first += 8;
        }
        batch.len = len;
    }
}

/// The lines from row `row`'s to the next row's through the row code: whole lines, more
/// than one when a short line hid in them.
///
/// # Safety
/// `row` is a row of the batch, whose lines are in `env`'s segment.
#[cold]
#[inline(never)]
unsafe fn row_code(batch: &Batch, row: usize, env: &Env, narrow: &mut Chunks<EvN>, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>, slow: &mut usize) {
    // SAFETY: the caller's contract.
    unsafe {
        let (mut at, to) = (batch.lines[row].offset_from(env.base) as usize, batch.lines[row + 1].offset_from(env.base) as usize);
        while at < to {
            *slow += 1;
            slow_line(env.base, env.bytes, at, env.ctx, env.dates, env.plan, narrow, events, long_events);
            at = line_end(env.base, env.bytes, at);
        }
    }
}

/// The first `rows` rows of a batch as records, and what is not one through the row code.
///
/// # Safety
/// The rows are stored, their lines are in `env`'s segment, and `Batch::lines` holds where
/// each row's line begins and where the last one ends.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn finish(batch: &mut Batch, rows: usize, env: &Env, lanes: &Lanes, narrow: &mut Chunks<EvN>, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>, slow: &mut usize) {
    // SAFETY: the caller's contract; a list holds rows of the batch, and a record's list is
    // the top bits of a hash.
    unsafe {
        prepare(batch, rows, env);
        split(batch, rows);
        let (users, plan, shift) = (env.users, env.plan, env.shift);

        // The IDs that are not short first: looked at as long ones, and a long user's line
        // of the directory asked for, which then has every other kind's work to arrive in.
        for at in 0..batch.len[LONG_USER] {
            let row = *batch.of[LONG_USER].get_unchecked(at) as usize;
            let line = batch.lines[row];
            let (c0, c1) = commas_of(line);
            let (hi, lo, wrong, _) = one_id(line.add(c1), c1.wrapping_sub(c0 + 1), &ONE_U, b'u', lanes);
            (batch.rows[row][2], batch.lows[row][1]) = (hi, lo);
            if wrong != 0 { batch.words[row] |= LATE } else { users.prefetch_long(hi, lo) }
        }
        for at in 0..batch.len[LONG_EVENT] {
            let row = *batch.of[LONG_EVENT].get_unchecked(at) as usize;
            let line = batch.lines[row];
            let (c0, _) = commas_of(line);
            let (hi, lo, wrong, _) = one_id(line.add(c0), c0, &ONE_E, b'e', lanes);
            (batch.rows[row][0], batch.lows[row][0]) = (hi, lo);
            if wrong != 0 {
                batch.words[row] |= LATE;
            }
        }
        for at in 0..batch.len[LONG_BOTH] {
            let row = *batch.of[LONG_BOTH].get_unchecked(at) as usize;
            let line = batch.lines[row];
            let (c0, c1) = commas_of(line);
            let event = one_id(line.add(c0), c0, &ONE_E, b'e', lanes);
            let user = one_id(line.add(c1), c1.wrapping_sub(c0 + 1), &ONE_U, b'u', lanes);
            (batch.rows[row][0], batch.lows[row][0]) = (event.0, event.1);
            (batch.rows[row][2], batch.lows[row][1]) = (user.0, user.1);
            if event.2 | user.2 != 0 { batch.words[row] |= LATE } else { users.prefetch_long(user.0, user.1) }
        }

        for at in 0..batch.len[ROW_CODE] {
            row_code(batch, *batch.of[ROW_CODE].get_unchecked(at) as usize, env, narrow, events, long_events, slow);
        }

        // A purchase's price and product are read here (`event_row`: the row is valid when
        // its price is, too): the record's last two words.
        let bought = |next: *const u8, stamp: u64, ingest: u64| {
            let (pay, price_ok) = purchase(next, env.ctx);
            (if price_ok { stamp } else { 0 }, ingest | u64::from(pay) << 32)
        };
        for at in 0..batch.len[BOUGHT] {
            let row = *batch.of[BOUGHT].get_unchecked(at) as usize;
            let user = env.user(batch.rows.get_unchecked(row)[2] + 1);
            let (mut stamp, mut ingest_pay) = (batch.stamps[row], batch.ingests[row]);
            if batch.words[row] & BUY != 0 {
                (stamp, ingest_pay) = bought(batch.lines[row + 1], stamp, ingest_pay);
            }
            super::push_wide(plan, narrow, events, _pext_u64(batch.keys[row], SIX), stamp | u64::from(user), ingest_pay);
        }

        // Three rows in four: nothing to decide.
        for at in 0..batch.len[SHORT] {
            let row = *batch.of[SHORT].get_unchecked(at) as usize;
            let user = env.user(batch.rows.get_unchecked(row)[2] + 1);
            let key = _pext_u64(*batch.keys.get_unchecked(row), SIX);
            narrow.push_at((types::hash::hash(key) >> shift) as usize, EvN { a: key | *batch.firsts.get_unchecked(row), b: *batch.seconds.get_unchecked(row) | u64::from(user) });
        }

        for at in 0..batch.len[LONG_USER] {
            let row = *batch.of[LONG_USER].get_unchecked(at) as usize;
            let word = batch.words[row];
            if word & LATE != 0 {
                row_code(batch, row, env, narrow, events, long_events, slow);
                continue;
            }
            let user = users.long(batch.rows[row][2], batch.lows[row][1]);
            let key = _pext_u64(batch.keys[row], SIX);
            if word & (BUY | WIDE) == 0 {
                narrow.push_at((types::hash::hash(key) >> shift) as usize, EvN { a: key | batch.firsts[row], b: batch.seconds[row] | u64::from(user) });
                continue;
            }
            let (mut stamp, mut ingest_pay) = (batch.stamps[row], batch.ingests[row]);
            if word & BUY != 0 {
                (stamp, ingest_pay) = bought(batch.lines[row + 1], stamp, ingest_pay);
            }
            super::push_wide(plan, narrow, events, key, stamp | u64::from(user), ingest_pay);
        }
        for kind in [LONG_EVENT, LONG_BOTH] {
            for at in 0..batch.len[kind] {
                let row = *batch.of[kind].get_unchecked(at) as usize;
                let word = batch.words[row];
                if word & LATE != 0 {
                    row_code(batch, row, env, narrow, events, long_events, slow);
                    continue;
                }
                let digits = batch.rows[row][2];
                let user = if kind == LONG_EVENT { env.user(digits + 1) } else { users.long(digits, batch.lows[row][1]) };
                let (mut stamp, mut ingest_pay) = (batch.stamps[row], batch.ingests[row]);
                if word & BUY != 0 {
                    (stamp, ingest_pay) = bought(batch.lines[row + 1], stamp, ingest_pay);
                }
                let (hi, lo) = (batch.rows[row][0], batch.lows[row][0]);
                long_events.push_at(plan.long_part(hi, lo), EvL { hi, lo, user_ts: stamp | u64::from(user), ingest_pay });
            }
        }
    }
}
