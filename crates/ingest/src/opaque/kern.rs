//! The line kernel for opaque IDs: an event or click line turned into numbers where it is
//! read, by tables.
//!
//! A line's layout picks a few table rows, and those rows do the work in 256-bit registers:
//!
//! - The commas of the line's first 64 bytes say how wide the two IDs are. Both short (nearly
//!   four lines in five): a row of `ID` shuffles both IDs' digits to the right of a register
//!   half each, with the letter, the quotes and the first digit beside them and what to
//!   subtract so that every byte that is as it should be reads 0 or a digit's value. One of
//!   them long (a letter and 32 hex digits, one ID in eight): each ID on its own, a short one
//!   by a row of `ONE_*`, a long one as its 128 bits (`hex32`).
//! - Everything after the `user_id` is found from its comma, wherever the IDs' widths put it:
//!   the width of `ts` and whether `ingest_ts` is wrapped pick a row of `TS` for both
//!   timestamps' digits and literals, with the weights that turn them into seconds (or the
//!   13 digits of F3 into a number) in three multiply-adds; a date is looked up in a memo of
//!   the run's few dates by three of its bytes and compared whole (`Memo`); the type by its
//!   second byte (`KIND`); the last two fields' quotes by their widths (`TAIL`).
//!
//! Nothing branches on what a line holds but the one test of the IDs' widths. Every check
//! leaves a residue that is zero for a line as the generator writes one; the residues are
//! OR-ed into one word a row, and the rows go 64 at a time through `finish`, which looks the
//! users up in the directory (every row's line of it asked for first, then read), makes the
//! records of the rows whose word is zero, reads price and product for the purchases among
//! them, and sends every other row's bytes through the row code. So the kernel never has to
//! be right about an unusual line, only to recognise one: a line it takes is well-formed,
//! with both IDs spelled the short or the long way, a `ts` that is F1, F2 or 13-digit F3, an
//! F1 `ingest_ts`, a date of the table in each, a type of the three, and a `product_id`.
//!
//! Why a taken line is exactly one well-formed line: the bytes up to the comma after the
//! `user_id` are the two IDs, matched byte for byte (a shuffle's residues, or 32 hex digits,
//! a letter and the quotes); from there to the comma after the type they are matched byte
//! for byte too, so none of them is a newline, a stray quote or an extra comma; the last two
//! fields lie between the last two commas before the newline, the one found from the end
//! must be the one the type's width gives from the front, and the 32 bytes before the
//! newline hold no other newline.

use core::arch::x86_64::*;

use types::Ctx;
use types::chunks::Chunks;
use types::id::pack_at;
use types::record::PAY_CATEGORY_SHIFT;
use types::time::DAY;
use types::users::{NO_USER, pairs_by_shifts};
use types::width::{Narrow, Wide, Width};

use super::{Ck, Ev, EvL, Out, Plan, click_row, event_row};
use crate::pos::Campaigns;
use crate::price_cents;
use crate::tok::{Scalar, Scratch, Tok};
use crate::ts::Dates;

/// Bytes the kernel may read from a line's first byte.
const WINDOW: usize = 192;
/// The widest comma position of a `user_id`'s end the both-short path takes: two IDs of 11
/// raw bytes (8 digits wrapped in quotes).
const SHORT_C1: usize = 23;
/// In a row's word, beside `PURCHASE`: the `event_id` (or `click_id`) and the `user_id` are
/// spelled the long way, their 128 bits in the row and in `Batch::lows`.
const LONG_EVENT: u64 = 1 << 33;
const LONG_USER: u64 = 1 << 34;

/// One shape of the two IDs: `event_id` of `c0` raw bytes, `user_id` of `c1 - c0 - 1`.
///
/// The 32 bytes read end 16 after the first comma, so lane 0 holds the `event_id` up to the
/// comma and lane 1 the comma and the `user_id`. After the shuffle and the subtraction a lane
/// reads: bytes 8..16 the digits less `'0'` at the right (0 in front), byte 7 the first digit
/// less `'1'` when there are several (a leading zero then reads 0xFF), byte 6 the letter less
/// itself, bytes 5 and 4 the wrapping quotes less `'"'`.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct IdRow {
    ctrl: [u8; 32],
    base: [u8; 32],
}

/// Rows of `ID`: `c0 * 64 + c1`, both up to 64 (no comma).
const ID_ROWS: usize = 64 * 64 + 65;

/// One ID of `raw` bytes from byte `start` of its lane, into lane `o / 16`; `false` when
/// `raw` is no shape of an ID: a letter and 1 to 8 digits, or 7 or 8 digits wrapped in quotes
/// (a shorter wrapped ID is rare, and the row code's).
const fn id_lane(row: &mut IdRow, o: usize, letter: u8, start: usize, raw: usize) -> bool {
    let quoted = raw == 10 || raw == 11;
    if !(quoted || (raw >= 2 && raw <= 9)) {
        return false;
    }
    let q = quoted as usize;
    let digits = raw - 1 - 2 * q;
    let mut k = 0;
    while k < digits {
        row.ctrl[o + 16 - digits + k] = (start + q + 1 + k) as u8;
        row.base[o + 16 - digits + k] = b'0';
        k += 1;
    }
    row.ctrl[o + 6] = (start + q) as u8;
    row.base[o + 6] = letter;
    if digits > 1 {
        row.ctrl[o + 7] = (start + q + 1) as u8;
        row.base[o + 7] = b'1';
    }
    if quoted {
        row.ctrl[o + 5] = start as u8;
        row.base[o + 5] = b'"';
        row.ctrl[o + 4] = (start + raw - 1) as u8;
        row.base[o + 4] = b'"';
    }
    true
}

const fn id_rows(letter: u8) -> [IdRow; ID_ROWS] {
    // No shape: the letter's byte reads 0xFF.
    let mut bad = IdRow { ctrl: [0x80; 32], base: [0; 32] };
    bad.base[6] = 1;
    let mut table = [bad; ID_ROWS];
    let mut c0 = 2;
    while c0 <= 11 {
        let mut width = 2;
        while width <= 11 {
            let mut row = IdRow { ctrl: [0x80; 32], base: [0; 32] };
            if id_lane(&mut row, 0, letter, 16 - c0, c0) && id_lane(&mut row, 16, b'u', 1, width) {
                table[c0 * 64 + c0 + 1 + width] = row;
            }
            width += 1;
        }
        c0 += 1;
    }
    table
}

/// An event line's IDs.
static ID: [IdRow; ID_ROWS] = id_rows(b'e');
/// A click line's: the `click_id` where the `event_id` is.
static IDK: [IdRow; ID_ROWS] = id_rows(b'k');

/// One ID alone, in the 16 bytes that end at its comma: what a half of an `IdRow` is.
#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct OneRow {
    ctrl: [u8; 16],
    base: [u8; 16],
}

/// Rows of a `ONE_*` table, by the ID's raw width.
const fn one_rows(letter: u8) -> [OneRow; 16] {
    // No shape: the letter's byte reads 0xFF.
    let mut bad = OneRow { ctrl: [0x80; 16], base: [0; 16] };
    bad.base[6] = 1;
    let mut table = [bad; 16];
    let mut raw = 2;
    while raw <= 11 {
        let mut row = IdRow { ctrl: [0x80; 32], base: [0; 32] };
        if id_lane(&mut row, 0, letter, 16 - raw, raw) {
            let mut one = OneRow { ctrl: [0x80; 16], base: [0; 16] };
            let mut at = 0;
            while at < 16 {
                one.ctrl[at] = row.ctrl[at];
                one.base[at] = row.base[at];
                at += 1;
            }
            table[raw] = one;
        }
        raw += 1;
    }
    table
}

static ONE_E: [OneRow; 16] = one_rows(b'e');
static ONE_U: [OneRow; 16] = one_rows(b'u');
static ONE_K: [OneRow; 16] = one_rows(b'k');

/// One shape of the two timestamps: lane 0 the `ts` (the 16 bytes before its comma), lane 1
/// the `ingest_ts` (16 bytes of it from its fifth, or sixth when wrapped).
///
/// After the shuffle and the subtraction a lane of a clock time reads: byte 0 the constant 1,
/// bytes 1 and 2 the seconds' digits, byte 3 an offset's sign less `'-'` (so `+` is negative),
/// bytes 4 to 7 hours and minutes, bytes 8 to 11 an offset's hours and minutes, bytes 12 to 15
/// the literals less themselves. A lane of 13 digits reads them in bytes 3 to 15 and its
/// quotes in bytes 1 and 2. Then `w1`, `w2`, a pack and `w3` leave two numbers per lane, and
/// the value is the first times `kme` plus the second, with byte 3's sign, times `kmf`:
/// seconds and an offset's minutes, or the first 5 and the last 8 of 13 digits.
///
/// `Narrow` (`types::width`) multiplies 128 bits at a time: `w1` on each lane, then the two
/// lanes' words side by side as bytes (none is over 99), so that the rest is one register's
/// work: `nw2`, `nw3`, `nkme` and `nkmf` are `w2`, `w3`, `kme` and `kmf` for that register,
/// the `ts`'s half first, and `nlim` is `limv` for the numbers as words.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TsRow {
    ctrl: [u8; 32],
    base: [u8; 32],
    /// The largest value of each byte.
    lim: [u8; 32],
    w1: [i8; 32],
    w2: [i16; 16],
    w3: [i16; 16],
    /// The largest value of each number after `w2`: hours and minutes as minutes.
    limv: [i32; 8],
    kme: [i64; 4],
    kmf: [i64; 4],
    /// The bytes of the `ts`'s date text that are compared with the memo's.
    dmask: [u8; 16],
    /// Where that text is read from, after the second comma: 0, or 1 past a quote.
    tq: u64,
    /// The bits of the date that index the memo; 0 for a `ts` without a date.
    pmask: u64,
    /// The same for the `ingest_ts`: `PMASK` in every row.
    imask: u64,
    _gap: u64,
    nw2: [i8; 16],
    nlim: [i16; 8],
    nw3: [i16; 8],
    nkme: [i64; 2],
    nkmf: [i64; 2],
    _pad: [u64; 12],
}

const _: () = assert!(size_of::<TsRow>() == 512 && size_of::<IdRow>() == 64);

/// Rows of `TS`: `(c2 - c1) * 2 + wrapped ingest_ts`.
const TS_ROWS: usize = 65 * 2;
/// Month's last digit (its lowest bit), then the day's two (2 and 4 bits), of the four bytes
/// `M-DD`: the memo's index. Two dates of adjoining months never share one.
const PMASK: u64 = 0x0F03_0001;

const fn put(row: &mut TsRow, at: usize, source: usize, base: u8, lim: u8, weight: i8) {
    row.ctrl[at] = source as u8;
    row.base[at] = base;
    row.lim[at] = lim;
    row.w1[at] = weight;
}

/// A clock time into lane `lane`: `HH:MM:SS` and `Z`, or an offset when `f2`.
const fn time_lane(row: &mut TsRow, lane: usize, f2: bool, quoted: bool) {
    let o = lane * 16;
    // Byte 0 reads 1 (0 less 0xFF), so the word the sign is in is never zero.
    row.base[o] = 0xFF;
    row.lim[o] = 1;
    // Where the hours, the minutes, the second colon and the seconds are in the 16 bytes.
    let (h, m, colon, s) = match (f2, quoted) {
        (false, false) => (7, 10, 12, 13),
        (false, true) => (6, 9, 11, 12),
        (true, false) => (2, 5, 7, 8),
        (true, true) => (1, 4, 6, 7),
    };
    put(row, o + 1, s, b'0', 5, 10);
    put(row, o + 2, s + 1, b'0', 9, 1);
    put(row, o + 4, h, b'0', 2, 10);
    put(row, o + 5, h + 1, b'0', 9, 1);
    put(row, o + 6, m, b'0', 5, 10);
    put(row, o + 7, m + 1, b'0', 9, 1);
    put(row, o + 12, colon, b':', 0, 0);
    if f2 {
        let sign = s + 2;
        // The sign twice: where it gives the offset its direction (`+` reads 0xFE, negative:
        // the offset is taken off), and where it is checked (`+` or `-`: 0 or 2; a comma
        // would read 1, and a field holds none).
        put(row, o + 3, sign, b'-', 0xFF, 0);
        put(row, o + 13, sign, b'+', 2, 0);
        put(row, o + 8, sign + 1, b'0', 2, 10);
        put(row, o + 9, sign + 2, b'0', 9, 1);
        put(row, o + 14, sign + 3, b':', 0, 0);
        put(row, o + 10, sign + 4, b'0', 5, 10);
        put(row, o + 11, sign + 5, b'0', 9, 1);
    } else {
        put(row, o + 13, s + 2, b'Z', 0, 0);
    }
    if quoted {
        put(row, o + 15, 15, b'"', 0, 0);
    }
    // Words: seconds' tens, units, hours, minutes, offset hours, offset minutes.
    let w = lane * 8;
    row.w2[w] = 1;
    row.w2[w + 1] = 1;
    row.w2[w + 2] = 60;
    row.w2[w + 3] = 1;
    row.w2[w + 4] = 60;
    row.w2[w + 5] = 1;
    // Numbers: seconds, minutes of the day, minutes of the offset.
    let d = lane * 4;
    row.limv[d] = i32::MAX;
    row.limv[d + 1] = 23 * 60 + 59;
    row.limv[d + 2] = 23 * 60 + 59;
    row.limv[d + 3] = i32::MAX;
    row.w3[w] = 1;
    row.w3[w + 1] = 60;
    row.w3[w + 2] = 1;
    row.kme[lane * 2] = 1000;
    row.kmf[lane * 2] = 60_000;
}

/// 13 digits into lane 0.
const fn digits_lane(row: &mut TsRow, quoted: bool) {
    row.base[0] = 0xFF;
    row.lim[0] = 1;
    if quoted {
        put(row, 1, 1, b'"', 0, 0);
        put(row, 2, 15, b'"', 0, 0);
    }
    let mut k = 0;
    while k < 13 {
        let at = 3 + k;
        put(row, at, if quoted { 2 + k } else { 3 + k }, b'0', 9, if at == 3 || at % 2 == 1 { 1 } else { 10 });
        k += 1;
    }
    // Words: 0, the first digit, then six pairs. Numbers: the first digit, then three fours.
    row.w2[1] = 1;
    let mut w = 2;
    while w < 8 {
        row.w2[w] = 100;
        row.w2[w + 1] = 1;
        w += 2;
    }
    let mut d = 0;
    while d < 4 {
        row.limv[d] = i32::MAX;
        d += 1;
    }
    row.w3[0] = 10_000;
    row.w3[1] = 1;
    row.w3[2] = 10_000;
    row.w3[3] = 1;
    row.kme[0] = 100_000_000;
    row.kmf[0] = 1;
}

/// `Narrow`'s weights from the row's own (`TsRow`).
const fn narrow_weights(row: &mut TsRow) {
    let mut k = 0;
    while k < 8 {
        row.nw2[k] = row.w2[k] as i8;
        row.nw2[8 + k] = row.w2[8 + k] as i8;
        k += 1;
    }
    let mut k = 0;
    while k < 4 {
        row.nlim[k] = if row.limv[k] > i16::MAX as i32 { i16::MAX } else { row.limv[k] as i16 };
        row.nlim[4 + k] = if row.limv[4 + k] > i16::MAX as i32 { i16::MAX } else { row.limv[4 + k] as i16 };
        row.nw3[k] = row.w3[k];
        row.nw3[4 + k] = row.w3[8 + k];
        k += 1;
    }
    row.nkme = [row.kme[0], row.kme[2]];
    row.nkmf = [row.kmf[0], row.kmf[2]];
}

const fn ts_rows() -> [TsRow; TS_ROWS] {
    let blank = TsRow {
        ctrl: [0x80; 32],
        base: [0; 32],
        lim: [0; 32],
        w1: [0; 32],
        w2: [0; 16],
        w3: [0; 16],
        limv: [0; 8],
        kme: [0; 4],
        kmf: [0; 4],
        dmask: [0; 16],
        tq: 0,
        pmask: 0,
        imask: PMASK,
        _gap: 0,
        nw2: [0; 16],
        nlim: [0; 8],
        nw3: [0; 8],
        nkme: [0; 2],
        nkmf: [0; 2],
        _pad: [0; 12],
    };
    let mut table = [blank; TS_ROWS];
    let mut gap = 0;
    while gap <= 64 {
        let mut wrapped = 0;
        while wrapped < 2 {
            let mut row = blank;
            // `gap` is the width of the `ts` and its comma.
            let quoted = gap == 23 || gap == 28 || gap == 16;
            match gap {
                21 | 23 | 26 | 28 => {
                    time_lane(&mut row, 0, gap >= 26, quoted);
                    // The 16 bytes from the one before the date: a quote when wrapped (the
                    // comma otherwise, not compared), the date and its `T`, the first colon.
                    row.dmask = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0xFF, 0];
                    if quoted {
                        row.dmask[0] = 0xFF;
                    }
                    row.tq = quoted as u64;
                    row.pmask = PMASK;
                }
                14 | 16 => digits_lane(&mut row, quoted),
                // No shape of a `ts`: byte 1 reads 0xFF over a limit of 0.
                _ => row.base[1] = 1,
            }
            time_lane(&mut row, 1, false, wrapped == 1);
            narrow_weights(&mut row);
            table[gap * 2 + wrapped] = row;
            wrapped += 1;
        }
        gap += 1;
    }
    table
}

static TS: [TsRow; TS_ROWS] = ts_rows();

/// One type as it lies between its commas, by its second byte.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow {
    cand: [u8; 16],
    mask: [u8; 16],
    /// Its raw width.
    width: i64,
    /// `PURCHASE` or 0, as it goes into a row's word.
    class: u64,
    _pad: [u64; 2],
}

/// In a row's word: the type is `purchase`. The low half holds the residues.
const PURCHASE: u64 = 1 << 32;

const fn kind(table: &mut [KindRow; 256], word: &[u8], quoted: bool, purchase: bool) {
    let (mut cand, mut mask) = ([0u8; 16], [0u8; 16]);
    cand[0] = b',';
    let mut n = 1;
    if quoted {
        cand[n] = b'"';
        n += 1;
    }
    let mut k = 0;
    while k < word.len() {
        cand[n] = word[k];
        n += 1;
        k += 1;
    }
    if quoted {
        cand[n] = b'"';
        n += 1;
    }
    cand[n] = b',';
    n += 1;
    let mut k = 0;
    while k < n {
        mask[k] = 0xFF;
        k += 1;
    }
    table[cand[2] as usize] = KindRow { cand, mask, width: n as i64 - 2, class: if purchase { PURCHASE } else { 0 }, _pad: [0; 2] };
}

const fn kind_rows() -> [KindRow; 256] {
    let mut table = [KindRow { cand: [0; 16], mask: [0xFF; 16], width: 0, class: 0, _pad: [0; 2] }; 256];
    // No type has this second byte: the row's own index never matches its candidate.
    let mut byte = 0;
    while byte < 256 {
        table[byte].cand[2] = !(byte as u8);
        byte += 1;
    }
    kind(&mut table, b"view", false, false);
    kind(&mut table, b"cart", false, false);
    kind(&mut table, b"purchase", false, true);
    kind(&mut table, b"view", true, false);
    kind(&mut table, b"cart", true, false);
    kind(&mut table, b"purchase", true, true);
    table
}

static KIND: [KindRow; 256] = kind_rows();

/// The last two fields by their raw widths, in the 32 bytes before the newline (bit 31 is the
/// line's last byte): `product_id` of `z4` bytes, `price_cents` of `z5`.
#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct TailRow {
    /// The bits of the two fields.
    qmask: u32,
    /// Each field's first byte: a quote there says the field is wrapped.
    sel: u32,
    /// The quotes the two fields then hold, by which are wrapped; all ones for widths that
    /// cannot be (no product, or a wrapped field of one byte or of nothing).
    pat: [u32; 4],
    /// From the first byte of the `ingest_ts` (a wrapping quote counts twice) to the comma
    /// before the product, less the type's width, as the widths found from the end give it:
    /// the `ingest_ts`, two commas, and what lies from that comma to the newline taken off
    /// the whole.
    gap: i64,
}

/// Rows of `TAIL`: `z5 << 6 | z4`.
const TAIL_ROWS: usize = 33 * 64;

const fn tail_rows() -> [TailRow; TAIL_ROWS] {
    let bad = TailRow { qmask: 0, sel: 0, pat: [!0; 4], gap: 0xFFFF };
    let mut table = [bad; TAIL_ROWS];
    let mut z5 = 0;
    while z5 <= 30 {
        let mut z4 = 0;
        while z5 + z4 <= 30 {
            // The bits of the last comma and of the one before it.
            let (c5, c4) = (31 - z5, 30 - z5 - z4);
            let mut row = bad;
            row.qmask = !0u32 << (c4 + 1);
            row.sel = 1u32 << (c4 + 1);
            if z5 >= 1 {
                row.sel |= 1u32 << (c5 + 1);
            }
            let mut wrapped = 0;
            while wrapped < 4 {
                let (product, price) = (wrapped & 1 == 1, wrapped & 2 == 2);
                // A product there must be; a wrapped field has its two quotes.
                if (if product { z4 >= 3 } else { z4 >= 1 }) && (!price || z5 >= 2) {
                    let mut pat = 0u32;
                    if product {
                        pat |= 1u32 << (c4 + 1) | 1u32 << (c5 - 1);
                    }
                    if price {
                        pat |= 1u32 << (c5 + 1) | 1u32 << 31;
                    }
                    row.pat[wrapped] = pat;
                }
                wrapped += 1;
            }
            // From `ingest` in the kernel (the comma before the `ingest_ts`, or two bytes on
            // when it is wrapped) the comma after the type is 22 bytes and the type on; from
            // the newline it is `32 - c4` back.
            row.gap = 54 - c4 as i64;
            table[z5 << 6 | z4] = row;
            z4 += 1;
        }
        z5 += 1;
    }
    table
}

static TAIL: [TailRow; TAIL_ROWS] = tail_rows();

/// One date: the 16 bytes a field holds from the byte before it (a quote, the date, `T`, two
/// digits, a colon, a digit; only the fixed ones are compared), and its midnight.
#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct Day {
    text: [u8; 16],
    /// Milliseconds after `Ctx::origin`.
    ms: i64,
    _pad: i64,
}

/// The dates of the table (`Dates`) by `PMASK`'s bits of their text.
///
/// An entry no date of the table falls on holds a date that falls elsewhere, so no text can
/// match it there: a text equal to that date would have been looked up at that date's own
/// index. Entry 0 is such an entry (no date has day `00`); its midnight is `-origin`, which is
/// what a `ts` of 13 digits, looked up there with nothing compared, has to have added.
pub(super) struct Memo {
    days: [Day; 128],
    /// The earliest midnight of the table.
    first_ms: i64,
}

impl Memo {
    /// The run's memo: made by the first chunk parsed, read by every worker after.
    fn of<'a>(ctx: &Ctx, dates: &'a Dates) -> &'a Memo {
        dates.kept.get_or_init(|| Box::new(Memo::new(ctx, dates))).downcast_ref().expect("the kernel's memo")
    }

    fn index(text: &[u8; 11]) -> usize {
        usize::from(text[6] & 1) | usize::from(text[8] & 3) << 1 | usize::from(text[9] & 15) << 3
    }

    fn new(ctx: &Ctx, dates: &Dates) -> Memo {
        let mut table: Vec<([u8; 11], i64)> = Vec::new();
        dates.each(|text, ms| table.push((text, ms)));
        let entry = |&(text, ms): &([u8; 11], i64)| {
            let mut day = Day { text: [0; 16], ms, _pad: 0 };
            day.text[0] = b'"';
            day.text[1..12].copy_from_slice(&text);
            day.text[14] = b':';
            day
        };
        let mut days = [Day { text: [0; 16], ms: 0, _pad: 0 }; 128];
        for (at, day) in days.iter_mut().enumerate() {
            // A date of the table that falls elsewhere; there are always two that differ.
            let other = table.iter().find(|(text, _)| Memo::index(text) != at).expect("the date table holds two dates");
            *day = entry(other);
        }
        for date in &table {
            days[Memo::index(&date.0)] = entry(date);
        }
        days[0].ms = -ctx.origin;
        Memo { days, first_ms: table.iter().map(|&(_, ms)| ms).min().unwrap_or(0) }
    }
}

#[cfg(test)]
thread_local! {
    /// Lines this thread sent to the row code.
    static SLOW_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Rows read before they are finished; a multiple of 8.
const BATCH: usize = 64;

/// What the kernel leaves of a row.
#[repr(C, align(64))]
pub(super) struct Batch {
    /// The `event_id` (its digits, each less `'0'`, or the first word of a long one), the
    /// `ts`, the `user_id` the same way, the `ingest_ts`; the times in milliseconds after
    /// `Ctx::origin`. A click's row: the `click_id`'s raw width, the `ts`, the `user_id`, the
    /// campaign's number.
    rows: [[u64; 4]; BATCH],
    /// 0 in the low half for a row that is a record as it stands (but for a purchase's price
    /// and product: `PURCHASE`); anything else there sends the row's bytes to the row code.
    /// `LONG_EVENT` and `LONG_USER` say which of its IDs are long.
    class: [u64; BATCH],
    /// Where each row's line begins, and one more: where the last one ends.
    line: [*const u8; BATCH + 8],
    /// The second words of a row's long IDs: the event's, the user's.
    lows: [[u64; 2]; BATCH],
    /// The users' numbers, once `finish` has looked them up.
    users: [u32; BATCH + 16],
    /// Rows stored and not yet finished: 0 for a batch that is being filled.
    count: usize,
}

/// Of the two batches a chunk's rows go through in turn, the one that is not `batch`.
#[inline(always)]
unsafe fn other(pair: *mut Batch, batch: *mut Batch) -> *mut Batch {
    // SAFETY: `pair` is two batches, and `batch` one of them.
    if batch == pair { unsafe { pair.add(1) } } else { pair }
}

/// Records held before they are scattered; a group adds at most 8.
const HELD: usize = 128;

/// The short-keyed records made and not yet scattered: the record's three words.
pub(super) struct Held {
    key: [u64; HELD],
    user_ts: [u64; HELD],
    ingest_pay: [u64; HELD],
    len: usize,
    /// 64 less the lists' bits: a key's list is the top bits of its hash.
    shift: u32,
}

impl Held {
    fn new(plan: &Plan) -> Held {
        Held { key: [0; HELD], user_ts: [0; HELD], ingest_pay: [0; HELD], len: 0, shift: 64 - plan.part_bits }
    }

    /// A function of its own: inside the decode its loop had no registers left.
    ///
    /// A record's list is made here, one multiply a record in a general register. In
    /// `finish` it was a 512-bit multiply for eight keys (`mul_halves` says what that
    /// costs on the scorer's cores).
    #[inline(never)]
    fn flush(&mut self, events: &mut Chunks<Ev>) {
        let (key, user_ts, ingest_pay, shift) = (&self.key, &self.user_ts, &self.ingest_pay, self.shift);
        // SAFETY: `len <= HELD`, and every list here is the top bits of a hash.
        unsafe {
            events.extend(self.len, |at| {
                let key = *key.get_unchecked(at);
                ((types::hash::hash(key) >> shift) as usize, Ev { key, user_ts: *user_ts.get_unchecked(at), ingest_pay: *ingest_pay.get_unchecked(at) })
            })
        };
        self.len = 0;
    }
}

/// What `finish` computes with, the same for every row.
pub(super) struct Consts {
    event_lo: __m512i,
    ts_end: __m512i,
    first_ms: __m512i,
    ingest_add: __m512i,
}

impl Consts {
    fn new(ctx: &Ctx, memo: &Memo) -> Consts {
        // SAFETY: register arithmetic; AVX-512F is compiled in.
        #[allow(unused_unsafe)]
        unsafe {
            let all = |value: i64| _mm512_set1_epi64(value);
            Consts {
                event_lo: all(i64::from(ctx.event_lo)),
                ts_end: all(i64::from(ctx.ts_end)),
                first_ms: all(memo.first_ms),
                // `Ctx::ingest_rel` of the table's first midnight: whole seconds, as every
                // midnight is.
                ingest_add: all((memo.first_ms + ctx.origin).div_euclid(1000) - ctx.ingest_base()),
            }
        }
    }
}

/// The four numbers of each lane as words, twice: what `_mm256_packus_epi32` is, for numbers
/// that are words already. (The intrinsic comes out with a clamp before it, two more
/// instructions a line.)
#[inline(always)]
fn pack_words(parts: __m256i) -> __m256i {
    let packed: __m256i;
    // SAFETY: one AVX2 instruction on registers.
    unsafe { core::arch::asm!("vpackusdw {o}, {a}, {a}", o = lateout(ymm_reg) packed, a = in(ymm_reg) parts, options(pure, nomem, nostack, preserves_flags)) };
    packed
}

/// `_mm512_mul_epu32(x, by)` as two 256-bit multiplies, each written as the instruction it
/// is: given the intrinsic on the two halves, the compiler joins them into one 512-bit
/// multiply again. A 512-bit multiply is what Intel's server cores of the scorer's
/// generation (Skylake to Cascade Lake) answer by dropping to their lowest clock, the
/// "AVX-512 heavy" licence, for every instruction of both threads of the core and for
/// about a millisecond after the last one; a 256-bit multiply asks for the licence the line
/// kernel's own multiplies already hold.
#[inline(always)]
fn mul_halves(x: __m512i, by: __m256i) -> __m512i {
    let (low, high): (__m256i, __m256i);
    // SAFETY: two AVX2 instructions on registers; `low` is written before `b` and `by` are
    // read, so it is a register of its own (`out`, not `lateout`).
    unsafe {
        let (a, b) = (_mm512_castsi512_si256(x), _mm512_extracti64x4_epi64::<1>(x));
        core::arch::asm!("vpmuludq {l}, {a}, {m}", "vpmuludq {h}, {b}, {m}", a = in(ymm_reg) a, b = in(ymm_reg) b, m = in(ymm_reg) by, l = out(ymm_reg) low, h = lateout(ymm_reg) high, options(pure, nomem, nostack, preserves_flags));
        _mm512_inserti64x4::<1>(_mm512_castsi256_si512(low), high)
    }
}

/// `Consts` in 256-bit registers, for `finish_narrow`.
pub(super) struct NarrowConsts {
    event_lo: __m256i,
    ts_end: __m256i,
    first_ms: __m256i,
    ingest_add: __m256i,
}

impl NarrowConsts {
    fn new(ctx: &Ctx, memo: &Memo) -> NarrowConsts {
        // SAFETY: register arithmetic; AVX2 is compiled in.
        #[allow(unused_unsafe)]
        unsafe {
            let all = |value: i64| _mm256_set1_epi64x(value);
            NarrowConsts {
                event_lo: all(i64::from(ctx.event_lo)),
                ts_end: all(i64::from(ctx.ts_end)),
                first_ms: all(memo.first_ms),
                ingest_add: all((memo.first_ms + ctx.origin).div_euclid(1000) - ctx.ingest_base()),
            }
        }
    }
}

/// What the kernel's two widths (`types::width`) do in functions of their own: a batch's rows
/// into records (`finish`, `finish_clicks`), with the constants they keep in registers.
/// Everything else is one function for both, with `Width::NARROW` choosing an instruction
/// here and there.
pub(super) trait Kern: Width {
    type Consts;
    type ClickConsts: Copy;
    fn consts(ctx: &Ctx, memo: &Memo) -> Self::Consts;
    fn click_consts(ctx: &Ctx) -> Self::ClickConsts;
    /// # Safety
    /// As `finish`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn finish(batch: *mut Batch, base: *const u8, bytes: usize, consts: &Self::Consts, held: &mut Held, ctx: &Ctx, dates: &Dates, plan: &Plan, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>, slow: &mut usize);
    /// # Safety
    /// As `finish_clicks`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn finish_clicks(batch: *mut Batch, base: *const u8, bytes: usize, ts_end: Self::ClickConsts, campaigns: &Campaigns, ctx: &Ctx, dates: &Dates, plan: &Plan, clicks: &mut Chunks<Ck>, slow: &mut usize);
}

impl Kern for Wide {
    type Consts = Consts;
    type ClickConsts = __m512i;

    #[inline(always)]
    fn consts(ctx: &Ctx, memo: &Memo) -> Consts {
        Consts::new(ctx, memo)
    }

    #[inline(always)]
    fn click_consts(ctx: &Ctx) -> __m512i {
        // SAFETY: register arithmetic; AVX-512F is compiled in.
        #[allow(unused_unsafe)]
        unsafe {
            _mm512_set1_epi64(i64::from(ctx.ts_end))
        }
    }

    #[inline(always)]
    unsafe fn finish(batch: *mut Batch, base: *const u8, bytes: usize, consts: &Consts, held: &mut Held, ctx: &Ctx, dates: &Dates, plan: &Plan, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>, slow: &mut usize) {
        // SAFETY: the caller's contract.
        unsafe { finish(batch, base, bytes, consts, held, ctx, dates, plan, events, long_events, slow) }
    }

    #[inline(always)]
    unsafe fn finish_clicks(batch: *mut Batch, base: *const u8, bytes: usize, ts_end: __m512i, campaigns: &Campaigns, ctx: &Ctx, dates: &Dates, plan: &Plan, clicks: &mut Chunks<Ck>, slow: &mut usize) {
        // SAFETY: the caller's contract.
        unsafe { finish_clicks(batch, base, bytes, ts_end, campaigns, ctx, dates, plan, clicks, slow) }
    }
}

impl Kern for Narrow {
    type Consts = NarrowConsts;
    type ClickConsts = i64;

    #[inline(always)]
    fn consts(ctx: &Ctx, memo: &Memo) -> NarrowConsts {
        NarrowConsts::new(ctx, memo)
    }

    #[inline(always)]
    fn click_consts(ctx: &Ctx) -> i64 {
        i64::from(ctx.ts_end)
    }

    #[inline(always)]
    unsafe fn finish(batch: *mut Batch, base: *const u8, bytes: usize, consts: &NarrowConsts, held: &mut Held, ctx: &Ctx, dates: &Dates, plan: &Plan, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>, slow: &mut usize) {
        // SAFETY: the caller's contract.
        unsafe { finish_narrow(batch, base, bytes, consts, held, ctx, dates, plan, events, long_events, slow) }
    }

    #[inline(always)]
    unsafe fn finish_clicks(batch: *mut Batch, base: *const u8, bytes: usize, ts_end: i64, campaigns: &Campaigns, ctx: &Ctx, dates: &Dates, plan: &Plan, clicks: &mut Chunks<Ck>, slow: &mut usize) {
        // SAFETY: the caller's contract.
        unsafe { finish_clicks_narrow(batch, base, bytes, ts_end, campaigns, ctx, dates, plan, clicks, slow) }
    }
}

/// One 128-bit multiply `op` of `a` by the 16 bytes of a `TsRow`'s field `field`, written as
/// the instruction it is: of two on a register's halves the compiler makes one 256-bit
/// multiply, a licence-1 instruction (`types::width`). The field is read at its offset from
/// the row, not through a pointer of its own (one instruction a multiply less).
macro_rules! row_mul {
    ($op:literal, $a:expr, $row:expr, $field:ident) => {
        row_mul!($op, $a, $row, $field, 0)
    };
    ($op:literal, $a:expr, $row:expr, $field:ident, $more:expr) => {{
        let out: __m128i;
        // SAFETY: the row's field has 16 bytes at that offset; one AVX instruction.
        #[allow(unused_unsafe)]
        unsafe {
            core::arch::asm!(
                concat!($op, " {out:x}, {a:x}, xmmword ptr [{row} + {at}]"),
                a = in(zmm_reg) $a,
                row = in(reg) core::ptr::from_ref::<TsRow>($row),
                at = const std::mem::offset_of!(TsRow, $field) + $more,
                out = lateout(zmm_reg) out,
                options(pure, readonly, nostack, preserves_flags),
            )
        };
        out
    }};
}

/// `_mm_packus_epi16(a, b)` for words that are bytes already, as the one instruction it is:
/// the intrinsic comes out with a clamp of each operand before it, four more instructions
/// a line (as `pack_words` says of its own).
#[inline(always)]
fn pack_bytes(a: __m128i, b: __m128i) -> __m128i {
    let packed: __m128i;
    // SAFETY: one AVX instruction on registers.
    unsafe { core::arch::asm!("vpackuswb {o:x}, {a:x}, {b:x}", o = lateout(zmm_reg) packed, a = in(zmm_reg) a, b = in(zmm_reg) b, options(pure, nomem, nostack, preserves_flags)) };
    packed
}

/// `_mm256_mul_epu32(x, _mm256_broadcastsi128_si256(by))` as two 128-bit multiplies, as
/// `mul_halves` is two 256-bit ones.
#[inline(always)]
fn mul_quarters(x: __m256i, by: __m128i) -> __m256i {
    let (low, high): (__m128i, __m128i);
    // SAFETY: two AVX instructions on registers; `low` is written before `b` and `by` are
    // read, so it is a register of its own.
    unsafe {
        let (a, b) = (_mm256_castsi256_si128(x), _mm256_extracti128_si256::<1>(x));
        core::arch::asm!("vpmuludq {l:x}, {a:x}, {m:x}", "vpmuludq {h:x}, {b:x}, {m:x}", a = in(zmm_reg) a, b = in(zmm_reg) b, m = in(zmm_reg) by, l = out(zmm_reg) low, h = lateout(zmm_reg) high, options(pure, nomem, nostack, preserves_flags));
        _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(low), high)
    }
}

/// The bits of the 64 bytes at `p` that are `splat`'s byte: for `Wide` two 256-bit compares
/// that the compiler makes one 512-bit compare, for `Narrow` the two compares written as the
/// instructions they are.
///
/// # Safety
/// 64 bytes readable at `p`.
#[inline(always)]
unsafe fn first64<W: Width>(p: *const u8, splat: __m256i) -> u64 {
    // SAFETY: the caller's contract; AVX-512BW and VL are compiled in.
    unsafe {
        if W::NARROW {
            let bits: u64;
            core::arch::asm!(
                "vpcmpeqb {low}, {splat:y}, ymmword ptr [{p}]",
                "vpcmpeqb {high}, {splat:y}, ymmword ptr [{p} + 32]",
                "kunpckdq {low}, {high}, {low}",
                p = in(reg) p,
                splat = in(zmm_reg) splat,
                low = out(kreg) bits,
                high = out(kreg) _,
                options(pure, readonly, nostack, preserves_flags),
            );
            bits
        } else {
            _mm512_kunpackd(u64::from(_mm256_cmpeq_epi8_mask(_mm256_loadu_si256(p.add(32).cast()), splat)), u64::from(_mm256_cmpeq_epi8_mask(_mm256_loadu_si256(p.cast()), splat)))
        }
    }
}

/// A row's 32 bytes: the digits' halves of the IDs (`ids`' words 1 and 3) beside the times:
/// event, ts, user, ingest. `Wide`'s times are words 0 and 2 of `times`, `Narrow`'s words 0
/// and 1 (`tail`).
#[inline(always)]
fn beside<W: Width>(ids: __m256i, times: __m256i, lanes: &Lanes) -> __m256i {
    // SAFETY: register arithmetic; AVX-512VL is compiled in.
    #[allow(unused_unsafe)]
    unsafe {
        if W::NARROW {
            _mm256_permutex2var_epi64(ids, lanes.beside, times)
        } else {
            _mm256_castpd_si256(_mm256_shuffle_pd::<0b0101>(_mm256_castsi256_pd(ids), _mm256_castsi256_pd(times)))
        }
    }
}

/// The registers every line's checks use.
struct Lanes {
    comma: __m256i,
    newline: __m256i,
    quote: __m256i,
    /// Per half of the IDs: bytes 0..7 must be 0; byte 7 0..=8 or a letter less `'1'`; bytes
    /// 8..16 0..=9 or a letter less `'0'`.
    to_letter: __m256i,
    digit_max: __m256i,
    letter_max: __m256i,
    /// The bytes of an `ingest_ts`'s date text that are compared: the date, `T`, a colon.
    date_mask: __m128i,
    /// Where `beside` takes a narrow row's words from.
    beside: __m256i,
}

impl Lanes {
    #[inline(always)]
    fn new() -> Lanes {
        // SAFETY: register arithmetic.
        #[allow(unused_unsafe)]
        unsafe {
            let lane = |low: i8, seventh: i8, rest: i8| _mm256_setr_epi8(low, low, low, low, low, low, low, seventh, rest, rest, rest, rest, rest, rest, rest, rest, low, low, low, low, low, low, low, seventh, rest, rest, rest, rest, rest, rest, rest, rest);
            Lanes {
                comma: _mm256_set1_epi8(b',' as i8),
                newline: _mm256_set1_epi8(b'\n' as i8),
                quote: _mm256_set1_epi8(b'"' as i8),
                to_letter: lane(0, -48, -49),
                digit_max: lane(0, 8, 9),
                letter_max: lane(0, 25, 25),
                date_mask: _mm_setr_epi8(0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, 0),
                beside: _mm256_setr_epi64x(1, 4, 3, 5),
            }
        }
    }
}

/// 32 hex digits at `p` as 16 bytes, two digits a byte in their order, in two words
/// (`types::users::long_key_at`), and a mask of the bytes that are not one of `0-9a-f`.
///
/// # Safety
/// 32 bytes readable at `p`.
#[inline(always)]
unsafe fn hex32<W: Width>(p: *const u8) -> (u64, u64, u32) {
    // SAFETY: the caller's contract; register arithmetic.
    unsafe {
        let text = _mm256_loadu_si256(p.cast());
        let digit = _mm256_sub_epi8(text, _mm256_set1_epi8(b'0' as i8));
        let letter = _mm256_sub_epi8(text, _mm256_set1_epi8(b'a' as i8));
        let is_digit = _mm256_cmpeq_epi8(_mm256_min_epu8(digit, _mm256_set1_epi8(9)), digit);
        let is_letter = _mm256_cmpeq_epi8(_mm256_min_epu8(letter, _mm256_set1_epi8(5)), letter);
        let value = _mm256_blendv_epi8(_mm256_add_epi8(letter, _mm256_set1_epi8(10)), digit, is_digit);
        let bad = !(_mm256_movemask_epi8(_mm256_or_si256(is_digit, is_letter)) as u32);
        // A pair of digits a byte: the first sixteen times, the second once.
        let pairs = if W::NARROW { pairs_by_shifts(value) } else { _mm256_maddubs_epi16(value, _mm256_set1_epi16(0x0110)) };
        let bytes = _mm256_packus_epi16(pairs, pairs);
        (_mm256_extract_epi64::<0>(bytes) as u64, _mm256_extract_epi64::<2>(bytes) as u64, bad)
    }
}

/// One ID of `raw` bytes that ends at `comma`: its word (the digits of a short one, each
/// less `'0'`, or the first word of a long one), a long one's second word, a residue that
/// is zero when the ID is spelled the short or the long way with `letter`, and whether it
/// is a long one.
///
/// # Safety
/// The 16 bytes before `comma` are readable, and for `raw` of 33 or 35 the `raw` before it.
#[inline(always)]
unsafe fn one_id<W: Width>(comma: *const u8, raw: usize, table: &[OneRow; 16], letter: u8, lanes: &Lanes) -> (u64, u64, u64, bool) {
    // SAFETY: the caller's contract; a table row is picked by a width below 16.
    unsafe {
        if raw < 16 {
            let shape = table.get_unchecked(raw);
            let lane = _mm_sub_epi8(_mm_shuffle_epi8(_mm_loadu_si128(comma.sub(16).cast()), _mm_load_si128(shape.ctrl.as_ptr().cast())), _mm_load_si128(shape.base.as_ptr().cast()));
            let (to_letter, digit_max, letter_max) = (_mm256_castsi256_si128(lanes.to_letter), _mm256_castsi256_si128(lanes.digit_max), _mm256_castsi256_si128(lanes.letter_max));
            let bad = _mm_min_epu8(_mm_subs_epu8(_mm_add_epi8(lane, to_letter), letter_max), _mm_subs_epu8(lane, digit_max));
            (_mm_extract_epi64::<1>(lane) as u64, 0, (_mm_extract_epi64::<0>(bad) | _mm_extract_epi64::<1>(bad)) as u64, false)
        } else if raw == 33 || raw == 35 {
            let quoted = (raw - 33) / 2;
            let first = comma.sub(raw);
            let (hi, lo, bad) = hex32::<W>(first.add(quoted + 1));
            let mut wrong = u64::from(bad) | u64::from(*first.add(quoted) ^ letter);
            if quoted == 1 {
                wrong |= u64::from(*first ^ b'"') | u64::from(*comma.sub(1) ^ b'"');
            }
            (hi, lo, wrong, true)
        } else {
            (0, 0, 1, false)
        }
    }
}

/// What an event line's fields after the `user_id` give.
struct Tail {
    /// The `ts` and the `ingest_ts` in milliseconds after `Ctx::origin`: words 0 and 2
    /// (`Wide`), or 0 and 1 (`Narrow`).
    times: __m256i,
    /// Residues: zero for fields as the generator writes them.
    bad: __m256i,
    low: u32,
    /// `PURCHASE`, or 0.
    class: u64,
}

/// An event line from the comma after its `user_id`, at `t`, on: the comma after its `ts` is
/// `gap` bytes further and its newline at `end`.
///
/// # Safety
/// The 16 bytes before `t` and everything from there to 32 bytes past `end` are readable;
/// `end` is at least 45 bytes past `t`.
#[inline(always)]
unsafe fn tail<W: Width>(t: *const u8, gap: usize, end: *const u8, days: *const u8, lanes: &Lanes) -> Tail {
    if W::NARROW {
        // SAFETY: the caller's contract.
        return unsafe { tail_narrow(t, gap, end, days, lanes) };
    }
    // SAFETY: the caller's contract; table indexes are bounded by the counts they are made
    // of (a width is clamped, a count of zeros is at most 32).
    unsafe {
        // The last two fields, from the newline back.
        let tail_bytes = _mm256_loadu_si256(end.sub(32).cast());
        let tail_commas = _mm256_movemask_epi8(_mm256_cmpeq_epi8(tail_bytes, lanes.comma)) as u32;
        let tail_quotes = _mm256_movemask_epi8(_mm256_cmpeq_epi8(tail_bytes, lanes.quote)) as u32;
        let z5 = tail_commas.leading_zeros();
        let z4 = (tail_commas.wrapping_shl(z5) << 1).leading_zeros();
        let tail = &*TAIL.as_ptr().add((z5 << 6 | z4) as usize);
        let wrapped = _pext_u32(tail_quotes, tail.sel) as usize;
        let tail_bad = (tail_quotes & tail.qmask) ^ *tail.pat.get_unchecked(wrapped);

        // Both timestamps.
        let gap = gap.min(64);
        let ingest_q = usize::from(*t.add(gap + 1) == b'"');
        let ingest = t.add(gap + 2 * ingest_q);
        let row = &*TS.as_ptr().add(gap * 2 + ingest_q);
        let raw = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_loadu_si128(t.add(gap).sub(16).cast())), _mm_loadu_si128(ingest.add(5).cast()));
        let bytes = _mm256_sub_epi8(_mm256_shuffle_epi8(raw, _mm256_load_si256(row.ctrl.as_ptr().cast())), _mm256_load_si256(row.base.as_ptr().cast()));
        let words = _mm256_maddubs_epi16(bytes, _mm256_load_si256(row.w1.as_ptr().cast()));
        let parts = _mm256_madd_epi16(words, _mm256_load_si256(row.w2.as_ptr().cast()));
        let parts_bad = _mm256_cmpgt_epi32(parts, _mm256_load_si256(row.limv.as_ptr().cast()));
        let both = _mm256_madd_epi16(pack_words(parts), _mm256_load_si256(row.w3.as_ptr().cast()));
        let bytes_bad = _mm256_subs_epu8(bytes, _mm256_load_si256(row.lim.as_ptr().cast()));
        let major = _mm256_mul_epu32(both, _mm256_load_si256(row.kme.as_ptr().cast()));
        let minor = _mm256_mul_epi32(_mm256_sign_epi32(_mm256_srli_epi64::<32>(both), bytes), _mm256_load_si256(row.kmf.as_ptr().cast()));

        // Their dates.
        let ts_date = t.add(row.tq as usize);
        let ingest_date = t.add(gap + ingest_q);
        let ts_day = days.add(_pext_u64(u64::from(ts_date.add(7).cast::<u32>().read_unaligned()), row.pmask) as usize * 32);
        let ingest_day = days.add(_pext_u64(u64::from(ingest_date.add(7).cast::<u32>().read_unaligned()), row.imask) as usize * 32);
        // (text ^ day) & mask.
        let ts_date_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(row.dmask.as_ptr().cast()), _mm_loadu_si128(ts_date.cast()), _mm_load_si128(ts_day.cast()));
        let ingest_date_bad = _mm_ternarylogic_epi64::<0x60>(lanes.date_mask, _mm_loadu_si128(ingest_date.cast()), _mm_load_si128(ingest_day.cast()));
        let midnights = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_load_si128(ts_day.add(16).cast())), _mm_load_si128(ingest_day.add(16).cast()));
        let times = _mm256_add_epi64(_mm256_add_epi64(major, minor), midnights);

        // The type, from the comma the `ingest_ts`'s shape puts before it.
        let kind = &*KIND.as_ptr().add(usize::from(*ingest.add(23)));
        let kind_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(kind.mask.as_ptr().cast()), _mm_loadu_si128(ingest.add(21).cast()), _mm_load_si128(kind.cand.as_ptr().cast()));
        // The comma after the type, from the front and from the end.
        let apart = end.offset_from(ingest) as i64 - tail.gap - kind.width;

        let narrow = _mm_ternarylogic_epi64::<0xFE>(ts_date_bad, ingest_date_bad, kind_bad);
        let bad = _mm256_ternarylogic_epi64::<0xFE>(bytes_bad, parts_bad, _mm256_zextsi128_si256(narrow));
        let bad = _mm256_or_si256(bad, _mm256_cmpeq_epi8(tail_bytes, lanes.newline));
        Tail { times, bad, low: tail_bad | apart as u32, class: kind.class }
    }
}

/// `tail` with no multiply wider than 128 bits (`types::width`): `w1` on each timestamp's
/// half, then both halves' words as bytes in one register, which `nw2`, `nw3`, `nkme` and
/// `nkmf` take to the two values (`TsRow`). Three instructions a line more than `tail`'s
/// five 256-bit multiplies take, for six 128-bit ones.
///
/// # Safety
/// As `tail`.
#[inline(always)]
unsafe fn tail_narrow(t: *const u8, gap: usize, end: *const u8, days: *const u8, lanes: &Lanes) -> Tail {
    // SAFETY: as `tail`.
    unsafe {
        // The last two fields, from the newline back.
        let tail_bytes = _mm256_loadu_si256(end.sub(32).cast());
        let tail_commas = _mm256_movemask_epi8(_mm256_cmpeq_epi8(tail_bytes, lanes.comma)) as u32;
        let tail_quotes = _mm256_movemask_epi8(_mm256_cmpeq_epi8(tail_bytes, lanes.quote)) as u32;
        let z5 = tail_commas.leading_zeros();
        let z4 = (tail_commas.wrapping_shl(z5) << 1).leading_zeros();
        let tail = &*TAIL.as_ptr().add((z5 << 6 | z4) as usize);
        let wrapped = _pext_u32(tail_quotes, tail.sel) as usize;
        let tail_bad = (tail_quotes & tail.qmask) ^ *tail.pat.get_unchecked(wrapped);

        // Both timestamps.
        let gap = gap.min(64);
        let ingest_q = usize::from(*t.add(gap + 1) == b'"');
        let ingest = t.add(gap + 2 * ingest_q);
        let row = &*TS.as_ptr().add(gap * 2 + ingest_q);
        let raw = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_loadu_si128(t.add(gap).sub(16).cast())), _mm_loadu_si128(ingest.add(5).cast()));
        let bytes = _mm256_sub_epi8(_mm256_shuffle_epi8(raw, _mm256_load_si256(row.ctrl.as_ptr().cast())), _mm256_load_si256(row.base.as_ptr().cast()));
        let bytes_bad = _mm256_subs_epu8(bytes, _mm256_load_si256(row.lim.as_ptr().cast()));
        let (ts_half, ingest_half) = (_mm256_castsi256_si128(bytes), _mm256_extracti128_si256::<1>(bytes));
        // Words of digits and of literals' residues (none over 99 in a line `bytes_bad`
        // passes), both halves' as bytes.
        let words = pack_bytes(row_mul!("vpmaddubsw", ts_half, row, w1), row_mul!("vpmaddubsw", ingest_half, row, w1, 16));
        let parts = row_mul!("vpmaddubsw", words, row, nw2);
        let parts_bad = _mm_cmpgt_epi16(parts, _mm_load_si128(row.nlim.as_ptr().cast()));
        let both = row_mul!("vpmaddwd", parts, row, nw3);
        let major = row_mul!("vpmuludq", both, row, nkme);
        // Each half's sign in its first four bytes, as `tail` reads it.
        let minor = row_mul!("vpmuldq", _mm_sign_epi32(_mm_srli_epi64::<32>(both), _mm_unpacklo_epi64(ts_half, ingest_half)), row, nkmf);

        // Their dates.
        let ts_date = t.add(row.tq as usize);
        let ingest_date = t.add(gap + ingest_q);
        let ts_day = days.add(_pext_u64(u64::from(ts_date.add(7).cast::<u32>().read_unaligned()), row.pmask) as usize * 32);
        let ingest_day = days.add(_pext_u64(u64::from(ingest_date.add(7).cast::<u32>().read_unaligned()), row.imask) as usize * 32);
        // (text ^ day) & mask.
        let ts_date_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(row.dmask.as_ptr().cast()), _mm_loadu_si128(ts_date.cast()), _mm_load_si128(ts_day.cast()));
        let ingest_date_bad = _mm_ternarylogic_epi64::<0x60>(lanes.date_mask, _mm_loadu_si128(ingest_date.cast()), _mm_load_si128(ingest_day.cast()));
        let midnights = _mm_unpacklo_epi64(_mm_load_si128(ts_day.add(16).cast()), _mm_load_si128(ingest_day.add(16).cast()));
        let times = _mm_add_epi64(_mm_add_epi64(major, minor), midnights);

        // The type, from the comma the `ingest_ts`'s shape puts before it.
        let kind = &*KIND.as_ptr().add(usize::from(*ingest.add(23)));
        let kind_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(kind.mask.as_ptr().cast()), _mm_loadu_si128(ingest.add(21).cast()), _mm_load_si128(kind.cand.as_ptr().cast()));
        // The comma after the type, from the front and from the end.
        let apart = end.offset_from(ingest) as i64 - tail.gap - kind.width;

        let narrow = _mm_ternarylogic_epi64::<0xFE>(ts_date_bad, ingest_date_bad, kind_bad);
        let bad = _mm256_ternarylogic_epi64::<0xFE>(bytes_bad, _mm256_cmpeq_epi8(tail_bytes, lanes.newline), _mm256_zextsi128_si256(_mm_or_si128(narrow, parts_bad)));
        Tail { times: _mm256_castsi128_si256(times), bad, low: tail_bad | apart as u32, class: kind.class }
    }
}

/// One event chunk.
///
/// # Safety
/// `p, len` is a segment, and `lead` bytes before `p` are readable.
#[inline(never)]
pub(super) unsafe fn events<W: Kern>(p: *const u8, len: usize, lead: usize, ctx: &Ctx, dates: &Dates, plan: &Plan, out: &mut Out) {
    let Out { events, long_events, slow, .. } = out;
    let memo = Memo::of(ctx, dates);
    let consts = W::consts(ctx, memo);
    let lanes = Lanes::new();
    let mut held = Held::new(plan);
    // Two batches in turn: when one is full its users' lines of the directory are asked
    // for, and the one before it, asked for a batch ago, is finished. (Asked for and looked
    // up in one go, the first rows of a batch still waited for their lines: the directory
    // is far larger than the caches, and a line takes longer to come than 64 rows to ask
    // for.)
    // SAFETY: plain integers, bytes and pointers that are not read before they are written.
    let mut pair: [Batch; 2] = unsafe { std::mem::zeroed() };
    // SAFETY: a line the kernel runs on begins at `line < stop`, at least `WINDOW - PAD`
    // bytes before the segment's end, which is a newline: the newline it looks for from at
    // most 141 bytes in is inside the segment, and everything it reads lies between 16 bytes
    // before the line and `WINDOW` after its first, readable with the `PAD` bytes after the
    // segment. Table indexes are bounded by the counts they are made of.
    unsafe {
        let last = p.add(len);
        // The kernel reads up to 16 bytes before a line.
        let start = p.add(16usize.saturating_sub(lead).min(len));
        let stop = p.add((len + crate::PAD).saturating_sub(WINDOW));
        let days = memo.days.as_ptr().cast::<u8>();
        // The batch through one pointer: a row's class and its line are stored 8 bytes apart,
        // in step with its 32 bytes.
        let pair: *mut Batch = pair.as_mut_ptr();
        let mut batch = pair;
        let mut rows = batch.cast::<u8>();
        let mut class = rows.add(std::mem::offset_of!(Batch, class));
        let mut lows = rows.add(std::mem::offset_of!(Batch, lows));
        const _: () = assert!(std::mem::offset_of!(Batch, line) == std::mem::offset_of!(Batch, class) + 8 * BATCH && std::mem::offset_of!(Batch, rows) == 0);
        let mut line = p;
        let (ask_step, by_rows) = (plan.ask_step, plan.ask_step != usize::MAX);
        while line < last {
            if line >= start && line < stop {
                // Eight times the rows stored: a row is 32 bytes, its class and its line 8.
                let mut k = 0usize;
                loop {
                    // The first commas: the IDs' widths.
                    let commas = first64::<W>(line, lanes.comma);
                    let c0 = commas.trailing_zeros() as usize;
                    let rest = commas & commas.wrapping_sub(1);
                    let c1 = rest.trailing_zeros() as usize;
                    let (end, stored, word);
                    if c1 <= SHORT_C1 {
                        // Both IDs short: the line is 65 to 96 bytes with its newline,
                        // nearly always.
                        let ends = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(line.add(64).cast()), lanes.newline)) as u32;
                        if ends == 0 {
                            break;
                        }
                        end = line.add(64 + ends.trailing_zeros() as usize);
                        let c2 = (rest & rest.wrapping_sub(1)).trailing_zeros() as usize;
                        let shape = &*ID.as_ptr().add(c0 * 64 + c1);
                        let raw = _mm256_loadu_si256(line.add(c0).sub(16).cast());
                        let ids = _mm256_sub_epi8(_mm256_shuffle_epi8(raw, _mm256_load_si256(shape.ctrl.as_ptr().cast())), _mm256_load_si256(shape.base.as_ptr().cast()));
                        let ids_bad = _mm256_min_epu8(_mm256_subs_epu8(_mm256_add_epi8(ids, lanes.to_letter), lanes.letter_max), _mm256_subs_epu8(ids, lanes.digit_max));
                        let rest_of = tail::<W>(line.add(c1), c2 - c1, end, days, &lanes);
                        let bad = _mm256_or_si256(ids_bad, rest_of.bad);
                        let dirty = u32::from(_mm256_test_epi64_mask(bad, bad));
                        // The digits' halves of the IDs beside the times: event, ts, user,
                        // ingest.
                        stored = beside::<W>(ids, rest_of.times, &lanes);
                        word = u64::from(rest_of.low | dirty) | rest_of.class;
                    } else {
                        // A long ID, or two: each ID on its own, and the rest from the
                        // `user_id`'s comma, which may be past the first 64 bytes.
                        let c1 = if rest != 0 {
                            c1
                        } else {
                            64 + (_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(line.add(64).cast()), lanes.comma)) as u32).trailing_zeros() as usize
                        };
                        let t = line.add(c1);
                        let ends = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(t.add(45).cast()), lanes.newline)) as u32;
                        if ends == 0 {
                            break;
                        }
                        end = t.add(45 + ends.trailing_zeros() as usize);
                        let gap = 1 + (_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(t.add(1).cast()), lanes.comma)) as u32).trailing_zeros() as usize;
                        let event = one_id::<W>(line.add(c0), c0, &ONE_E, b'e', &lanes);
                        let user = one_id::<W>(t, c1.wrapping_sub(c0 + 1), &ONE_U, b'u', &lanes);
                        let rest_of = tail::<W>(t, gap, end, days, &lanes);
                        let dirty = u32::from(_mm256_test_epi64_mask(rest_of.bad, rest_of.bad)) | u32::from(event.2 | user.2 != 0);
                        stored = beside::<W>(_mm256_set_epi64x(user.0 as i64, 0, event.0 as i64, 0), rest_of.times, &lanes);
                        word = u64::from(rest_of.low | dirty) | rest_of.class | if event.3 { LONG_EVENT } else { 0 } | if user.3 { LONG_USER } else { 0 };
                        lows.add(2 * k).cast::<[u64; 2]>().write([event.1, user.1]);
                    }
                    _mm256_store_si256(rows.add(4 * k).cast(), stored);
                    class.add(k).cast::<u64>().write(word);
                    class.add(k + 8 * BATCH).cast::<*const u8>().write(line);
                    k += 8;
                    line = end.add(1);
                    // The directory's lines of the last eight rows' users: asked for as the
                    // rows are decoded, a few at a time, and not a batch's 64 at once.
                    // `SS_ASK=batch` asks for a batch's when it is full, as before: by rows
                    // it is 14 instructions a line more (2.5% of a run's) for 1% of the
                    // cycles at 60M events and nothing at 20M, on a machine with a core a
                    // worker; where two workers share a core the wait for a line is already
                    // the other's time, and which side wins there is the scorer's to say.
                    if k & ask_step == 0 {
                        ask_rows(batch, k / 8 - 8, k / 8, plan);
                    }
                    if k == 8 * BATCH {
                        (*batch).count = BATCH;
                        (*batch).line[BATCH] = line;
                        if !by_rows {
                            ask::<W>(&*batch, plan);
                        }
                        batch = other(pair, batch);
                        W::finish(batch, p, len, &consts, &mut held, ctx, dates, plan, events, long_events, slow);
                        (rows, class, lows) = (batch.cast::<u8>(), batch.cast::<u8>().add(std::mem::offset_of!(Batch, class)), batch.cast::<u8>().add(std::mem::offset_of!(Batch, lows)));
                        k = 0;
                    }
                    if line >= stop {
                        break;
                    }
                }
                // The rows so far end where this line begins.
                if k != 0 {
                    (*batch).count = k / 8;
                    (*batch).line[k / 8] = line;
                    if by_rows {
                        ask_rows(batch, (k / 8) & !7, k / 8, plan);
                    } else {
                        ask::<W>(&*batch, plan);
                    }
                    batch = other(pair, batch);
                    W::finish(batch, p, len, &consts, &mut held, ctx, dates, plan, events, long_events, slow);
                    (rows, class, lows) = (batch.cast::<u8>(), batch.cast::<u8>().add(std::mem::offset_of!(Batch, class)), batch.cast::<u8>().add(std::mem::offset_of!(Batch, lows)));
                }
            }
            if line < last {
                // A line of another length, or one of the segment's last or first.
                #[cfg(test)]
                SLOW_LINES.with(|count| count.set(count.get() + 1));
                *slow += 1;
                let at = line.offset_from(p) as usize;
                slow_line::<W>(p, len, at, ctx, dates, plan, events, long_events);
                line = p.add(line_end(p, len, at));
            }
        }
        // The batch that was asked for last.
        W::finish(other(pair, batch), p, len, &consts, &mut held, ctx, dates, plan, events, long_events, slow);
    }
    held.flush(events);
    // Streamed lines become visible to the threads that read the lists.
    events.fence();
    long_events.fence();
}

/// The rows of a batch (`Batch::count` of them, 0 after this) as records, and what is not
/// one through the row code.
///
/// # Safety
/// `base, bytes` is the segment the rows' lines are in, and the batch's `line` holds where
/// each row's line begins and where the last one ends.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn finish(
    batch: *mut Batch,
    base: *const u8,
    bytes: usize,
    consts: &Consts,
    held: &mut Held,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    events: &mut Chunks<Ev>,
    long_events: &mut Chunks<EvL>,
    slow: &mut usize,
) {
    // SAFETY: the batch is the caller's own. `first + 8 <= BATCH`; a lane past `rows` holds
    // an older row or zeros, and `live` keeps it out of the records and the row code.
    unsafe {
        let batch = &mut *batch;
        let rows = std::mem::take(&mut batch.count);
        look::<Wide>(batch, rows, plan);
        let all = |value: i64| _mm512_set1_epi64(value);
        // Two rows a register: event, ts, user, ingest, twice. From two registers, the four
        // events then the four `ts`; the four users then the four `ingest_ts`.
        let (even, odd) = (_mm512_setr_epi64(0, 4, 8, 12, 1, 5, 9, 13), _mm512_setr_epi64(2, 6, 10, 14, 3, 7, 11, 15));
        let mut first = 0;
        while first < rows {
            if held.len + 8 > HELD {
                held.flush(events);
            }
            let live: __mmask8 = if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 };
            let at = batch.rows.as_ptr().add(first).cast::<__m512i>();
            let (r0, r1, r2, r3) = (_mm512_load_si512(at), _mm512_load_si512(at.add(1)), _mm512_load_si512(at.add(2)), _mm512_load_si512(at.add(3)));
            let (low_a, low_b) = (_mm512_permutex2var_epi64(r0, even, r1), _mm512_permutex2var_epi64(r2, even, r3));
            let (high_a, high_b) = (_mm512_permutex2var_epi64(r0, odd, r1), _mm512_permutex2var_epi64(r2, odd, r3));
            let event = _mm512_shuffle_i64x2::<0b01_00_01_00>(low_a, low_b);
            let ts = _mm512_shuffle_i64x2::<0b11_10_11_10>(low_a, low_b);
            let ingest = _mm512_shuffle_i64x2::<0b11_10_11_10>(high_a, high_b);

            // `event_row`'s `valid` for a row the kernel took: the window, and not late.
            let valid = _mm512_cmpge_epi64_mask(ts, consts.event_lo) & _mm512_cmplt_epi64_mask(ts, consts.ts_end) & _mm512_cmple_epi64_mask(_mm512_sub_epi64(ingest, ts), all(DAY));
            // `Ctx::ingest_rel`: whole seconds since the table's first midnight (under 2^32
            // ms, so the product's high bits are the quotient by 1000), then from its base.
            let seconds = _mm512_srli_epi64::<38>(mul_halves(_mm512_sub_epi64(ingest, consts.first_ms), _mm256_set1_epi64x(274_877_907)));
            let ingest_rel = _mm512_add_epi64(seconds, consts.ingest_add);
            // The record's words: user | ts << 32 with no `ts` for a row that is not valid,
            // then the ingest (a purchase's `pay` is put above it by `others`).
            let place = _mm512_cvtepu32_epi64(_mm256_loadu_si256(batch.users.as_ptr().add(first).cast()));
            let user_ts = _mm512_or_si512(place, _mm512_slli_epi64::<32>(_mm512_maskz_mov_epi64(valid, ts)));
            // `short_key_at`: the digits and one. (Its list: `Held::flush`.)
            let key = _mm512_add_epi64(event, all(1));

            let class = _mm512_loadu_si512(batch.class.as_ptr().add(first).cast());
            // Nothing set but, perhaps, a long user: a record as it stands.
            let fast = live & _mm512_testn_epi64_mask(class, all(!(LONG_USER as i64)));
            // Compressed in a register and stored whole: the lanes past the fast ones are
            // overwritten by the next store.
            let to = held.len;
            _mm512_storeu_si512(held.key.as_mut_ptr().add(to).cast(), _mm512_maskz_compress_epi64(fast, key));
            _mm512_storeu_si512(held.user_ts.as_mut_ptr().add(to).cast(), _mm512_maskz_compress_epi64(fast, user_ts));
            _mm512_storeu_si512(held.ingest_pay.as_mut_ptr().add(to).cast(), _mm512_maskz_compress_epi64(fast, ingest_rel));
            held.len = to + fast.count_ones() as usize;

            if fast != live {
                others(batch, first, live & !fast, user_ts, ingest_rel, base, bytes, ctx, dates, plan, events, long_events, slow);
            }
            first += 8;
        }
    }
}

/// `finish` in 256-bit registers (`types::width::Narrow`): eight rows a step, as two groups
/// of four. The same records as `finish`'s.
///
/// # Safety
/// As `finish`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn finish_narrow(
    batch: *mut Batch,
    base: *const u8,
    bytes: usize,
    consts: &NarrowConsts,
    held: &mut Held,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    events: &mut Chunks<Ev>,
    long_events: &mut Chunks<EvL>,
    slow: &mut usize,
) {
    // SAFETY: as `finish`.
    unsafe {
        let batch = &mut *batch;
        let rows = std::mem::take(&mut batch.count);
        look::<Narrow>(batch, rows, plan);
        let all = |value: i64| _mm256_set1_epi64x(value);
        let mut first = 0;
        while first < rows {
            if held.len + 8 > HELD {
                held.flush(events);
            }
            let live = _bzhi_u32(0xFF, (rows - first) as u32) as __mmask8;
            // Nothing set but, perhaps, a long user: a record as it stands.
            let fast = live & clean_rows(batch.class.as_ptr().add(first));
            // A row a register: event, ts, user, ingest. Two rows' pairs of words, then their
            // halves: four events, four `ts`, four `ingest_ts`.
            let group = |at: usize| {
                let rows = batch.rows.as_ptr().add(at).cast::<__m256i>();
                let (r0, r1, r2, r3) = (_mm256_load_si256(rows), _mm256_load_si256(rows.add(1)), _mm256_load_si256(rows.add(2)), _mm256_load_si256(rows.add(3)));
                let (low_a, low_b) = (_mm256_unpacklo_epi64(r0, r1), _mm256_unpacklo_epi64(r2, r3));
                let (high_a, high_b) = (_mm256_unpackhi_epi64(r0, r1), _mm256_unpackhi_epi64(r2, r3));
                let event = _mm256_permute2x128_si256::<0x20>(low_a, low_b);
                let ts = _mm256_permute2x128_si256::<0x20>(high_a, high_b);
                let ingest = _mm256_permute2x128_si256::<0x31>(high_a, high_b);
                // As `finish`.
                let valid = _mm256_cmpge_epi64_mask(ts, consts.event_lo) & _mm256_cmplt_epi64_mask(ts, consts.ts_end) & _mm256_cmple_epi64_mask(_mm256_sub_epi64(ingest, ts), all(DAY));
                let seconds = _mm256_srli_epi64::<38>(mul_quarters(_mm256_sub_epi64(ingest, consts.first_ms), _mm_set1_epi64x(274_877_907)));
                let ingest_rel = _mm256_add_epi64(seconds, consts.ingest_add);
                let place = _mm256_cvtepu32_epi64(_mm_loadu_si128(batch.users.as_ptr().add(at).cast()));
                let user_ts = _mm256_or_si256(place, _mm256_slli_epi64::<32>(_mm256_maskz_mov_epi64(valid, ts)));
                (_mm256_add_epi64(event, all(1)), user_ts, ingest_rel)
            };
            let (key_a, user_ts_a, ingest_a) = group(first);
            let (key_b, user_ts_b, ingest_b) = group(first + 4);
            // Compressed in a register and stored whole, as in `finish`.
            let (fast_a, fast_b) = (fast & 0xF, fast >> 4);
            let to = held.len;
            _mm256_storeu_si256(held.key.as_mut_ptr().add(to).cast(), _mm256_maskz_compress_epi64(fast_a, key_a));
            _mm256_storeu_si256(held.user_ts.as_mut_ptr().add(to).cast(), _mm256_maskz_compress_epi64(fast_a, user_ts_a));
            _mm256_storeu_si256(held.ingest_pay.as_mut_ptr().add(to).cast(), _mm256_maskz_compress_epi64(fast_a, ingest_a));
            let to = to + fast_a.count_ones() as usize;
            _mm256_storeu_si256(held.key.as_mut_ptr().add(to).cast(), _mm256_maskz_compress_epi64(fast_b, key_b));
            _mm256_storeu_si256(held.user_ts.as_mut_ptr().add(to).cast(), _mm256_maskz_compress_epi64(fast_b, user_ts_b));
            _mm256_storeu_si256(held.ingest_pay.as_mut_ptr().add(to).cast(), _mm256_maskz_compress_epi64(fast_b, ingest_b));
            held.len = to + fast_b.count_ones() as usize;

            if fast != live {
                let (mut user_ts_of, mut ingest_of) = ([0u64; 8], [0u64; 8]);
                _mm256_storeu_si256(user_ts_of.as_mut_ptr().cast(), user_ts_a);
                _mm256_storeu_si256(user_ts_of.as_mut_ptr().add(4).cast(), user_ts_b);
                _mm256_storeu_si256(ingest_of.as_mut_ptr().cast(), ingest_a);
                _mm256_storeu_si256(ingest_of.as_mut_ptr().add(4).cast(), ingest_b);
                others_narrow(batch, first, live & !fast, &user_ts_of, &ingest_of, base, bytes, ctx, dates, plan, events, long_events, slow);
            }
            first += 8;
        }
    }
}

/// Of the eight classes at `class`, those with nothing set but perhaps a long user: two
/// 256-bit tests written as the instructions they are (`types::width`).
///
/// # Safety
/// 64 bytes readable at `class`.
#[inline(always)]
unsafe fn clean_rows(class: *const u64) -> __mmask8 {
    let clean: __mmask8;
    // SAFETY: the caller's contract; AVX-512F, VL and DQ are compiled in.
    unsafe {
        core::arch::asm!(
            "vptestnmq {c}, {mask:y}, ymmword ptr [{p}]",
            "vptestnmq {c2}, {mask:y}, ymmword ptr [{p} + 32]",
            "kshiftlb {c2}, {c2}, 4",
            "korb {c}, {c}, {c2}",
            p = in(reg) class,
            mask = in(zmm_reg) _mm256_set1_epi64x(!(LONG_USER as i64)),
            c = out(kreg) clean,
            c2 = out(kreg) _,
            options(pure, readonly, nostack, preserves_flags),
        )
    };
    clean
}

/// Asks for the directory's lines of the users of a batch's rows `from..to`.
///
/// # Safety
/// The rows are stored, `to <= BATCH`.
#[inline(always)]
unsafe fn ask_rows(batch: *const Batch, from: usize, to: usize, plan: &Plan) {
    let users = &plan.users;
    // SAFETY: the caller's contract.
    unsafe {
        for row in from..to {
            let class = *(*batch).class.get_unchecked(row);
            if class as u32 == 0 {
                let user = (*batch).rows.get_unchecked(row)[2];
                if class & LONG_USER != 0 { users.prefetch_long(user, (*batch).lows.get_unchecked(row)[1]) } else { users.prefetch_short(user + 1) }
            }
        }
    }
}

/// The first `rows` rows of a batch in two lists, by the table their user is in: the rows'
/// numbers at `short` and at `long`, and how many each got. A row for the row code is in
/// neither.
///
/// Eight rows a step, by masks: which table a row's user is in is a coin's toss weighted one
/// in eight, and row by row a jump on it was guessed wrong about that often, where the
/// lines are asked for and again where they are read. A list is walked in a loop of its
/// own instead.
///
/// # Safety
/// `short` and `long` have room for `BATCH + 8` numbers each.
#[inline(always)]
unsafe fn split(batch: &Batch, rows: usize, short: *mut u64, long: *mut u64) -> (usize, usize) {
    let rows = rows.min(BATCH);
    // SAFETY: register arithmetic, loads of eight classes that begin below `BATCH` at a
    // multiple of 8, and stores of eight numbers into a list at its length, which is at
    // most the rows before the step's (56 at the most).
    unsafe {
        let all = |value: i64| _mm512_set1_epi64(value);
        let (mut shorts, mut longs) = (0usize, 0usize);
        let mut number = _mm512_setr_epi64(0, 1, 2, 3, 4, 5, 6, 7);
        let mut first = 0;
        while first < rows {
            let live: __mmask8 = if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 };
            let class = _mm512_loadu_si512(batch.class.as_ptr().add(first).cast());
            let taken = live & _mm512_testn_epi64_mask(class, all(0xFFFF_FFFF));
            let wide = _mm512_test_epi64_mask(class, all(LONG_USER as i64));
            // Compressed in a register and stored whole, as in `finish`: the lanes past the
            // rows of a kind are overwritten by the next store.
            _mm512_storeu_si512(short.add(shorts).cast(), _mm512_maskz_compress_epi64(taken & !wide, number));
            _mm512_storeu_si512(long.add(longs).cast(), _mm512_maskz_compress_epi64(taken & wide, number));
            shorts += (taken & !wide).count_ones() as usize;
            longs += (taken & wide).count_ones() as usize;
            number = _mm512_add_epi64(number, all(8));
            first += 8;
        }
        (shorts, longs)
    }
}

/// `split` in 256-bit registers (`types::width::Narrow`), the rows' numbers as `u32`s: eight
/// classes tested as two halves, and eight numbers compressed in one register.
///
/// # Safety
/// As `split`.
#[inline(always)]
unsafe fn split_narrow(batch: &Batch, rows: usize, short: *mut u32, long: *mut u32) -> (usize, usize) {
    let rows = rows.min(BATCH);
    // SAFETY: as `split`.
    unsafe {
        let (mut shorts, mut longs) = (0usize, 0usize);
        let mut number = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
        let mut first = 0;
        while first < rows {
            let (taken, wide) = classes(batch.class.as_ptr().add(first), _bzhi_u32(0xFF, (rows - first) as u32) as u8);
            _mm256_storeu_si256(short.add(shorts).cast(), _mm256_maskz_compress_epi32(taken & !wide, number));
            _mm256_storeu_si256(long.add(longs).cast(), _mm256_maskz_compress_epi32(taken & wide, number));
            shorts += (taken & !wide).count_ones() as usize;
            longs += (taken & wide).count_ones() as usize;
            number = _mm256_add_epi32(number, _mm256_set1_epi32(8));
            first += 8;
        }
        (shorts, longs)
    }
}

/// Of the eight classes at `class`, those of `live`'s rows that are records with nothing
/// set but perhaps a long user (`split`'s `taken`), and those with a long user: two
/// compares of each half, written as the instructions they are (`types::width`).
///
/// # Safety
/// 64 bytes readable at `class`.
#[inline(always)]
unsafe fn classes(class: *const u64, live: __mmask8) -> (__mmask8, __mmask8) {
    let (taken, wide): (__mmask8, __mmask8);
    // SAFETY: the caller's contract; AVX-512F, VL and DQ are compiled in.
    unsafe {
        core::arch::asm!(
            "vptestnmq {t}, {low:y}, ymmword ptr [{p}]",
            "vptestnmq {t2}, {low:y}, ymmword ptr [{p} + 32]",
            "vptestmq {w}, {long:y}, ymmword ptr [{p}]",
            "vptestmq {w2}, {long:y}, ymmword ptr [{p} + 32]",
            "kshiftlb {t2}, {t2}, 4",
            "kshiftlb {w2}, {w2}, 4",
            "korb {t2}, {t2}, {t}",
            "korb {w}, {w}, {w2}",
            "kandb {t}, {t2}, {live}",
            p = in(reg) class,
            low = in(zmm_reg) _mm256_set1_epi64x(0xFFFF_FFFF),
            long = in(zmm_reg) _mm256_set1_epi64x(LONG_USER as i64),
            live = in(kreg) live,
            t = out(kreg) taken,
            t2 = out(kreg) _,
            w = out(kreg) wide,
            w2 = out(kreg) _,
            options(pure, readonly, nostack, preserves_flags),
        )
    };
    (taken, wide)
}

/// `split` or `split_narrow`, into lists of room for `BATCH + 8` words each.
///
/// # Safety
/// As `split`.
#[inline(always)]
unsafe fn split_rows<W: Width>(batch: &Batch, rows: usize, short: *mut u64, long: *mut u64) -> (usize, usize) {
    // SAFETY: the caller's contract; a list of `u32`s fits in the room for words.
    unsafe { if W::NARROW { split_narrow(batch, rows, short.cast(), long.cast()) } else { split(batch, rows, short, long) } }
}

/// Number `at` of a list `split_rows` made.
///
/// # Safety
/// `at` is below the list's length.
#[inline(always)]
unsafe fn row_of<W: Width>(list: *const u64, at: usize) -> usize {
    // SAFETY: the caller's contract.
    unsafe { if W::NARROW { *list.cast::<u32>().add(at) as usize } else { *list.add(at) as usize } }
}

/// Asks for the directory's lines of a batch's users (`Batch::count` rows).
#[inline(always)]
fn ask<W: Width>(batch: &Batch, plan: &Plan) {
    let users = &plan.users;
    let (mut short, mut long) = (std::mem::MaybeUninit::<[u64; BATCH + 8]>::uninit(), std::mem::MaybeUninit::<[u64; BATCH + 8]>::uninit());
    let (short, long) = (short.as_mut_ptr().cast::<u64>(), long.as_mut_ptr().cast::<u64>());
    // SAFETY: the lists have `BATCH + 8` numbers' room, and `split` wrote the first `shorts`
    // and `longs` of them: rows of the batch.
    unsafe {
        let (shorts, longs) = split_rows::<W>(batch, batch.count, short, long);
        for at in 0..shorts {
            let row = row_of::<W>(short, at);
            users.prefetch_short(batch.rows.get_unchecked(row)[2] + 1);
        }
        for at in 0..longs {
            let row = row_of::<W>(long, at);
            users.prefetch_long(batch.rows.get_unchecked(row)[2], batch.lows.get_unchecked(row)[1]);
        }
    }
}

/// The users' numbers of a batch's first `rows` rows, into `Batch::users`: the rows by
/// their user's table first (`split`), then a loop a table.
#[inline(always)]
fn look<W: Width>(batch: &mut Batch, rows: usize, plan: &Plan) {
    let users = &plan.users;
    let rows = rows.min(BATCH);
    let (mut short, mut long) = (std::mem::MaybeUninit::<[u64; BATCH + 8]>::uninit(), std::mem::MaybeUninit::<[u64; BATCH + 8]>::uninit());
    let (short, long) = (short.as_mut_ptr().cast::<u64>(), long.as_mut_ptr().cast::<u64>());
    // SAFETY: as `ask`; a list's numbers are rows below `rows`.
    unsafe {
        let (shorts, longs) = split_rows::<W>(batch, rows, short, long);
        // A row for the row code is in neither list and has no user here: its bytes are
        // read again.
        batch.users[..rows].fill(NO_USER);
        for at in 0..shorts {
            let row = row_of::<W>(short, at);
            *batch.users.get_unchecked_mut(row) = users.short::<W>(batch.rows.get_unchecked(row)[2] + 1);
        }
        for at in 0..longs {
            let row = row_of::<W>(long, at);
            *batch.users.get_unchecked_mut(row) = users.long(batch.rows.get_unchecked(row)[2], batch.lows.get_unchecked(row)[1]);
        }
    }
}

/// The rows of a group that are not short-keyed records as they stand: the purchases, whose
/// price and product are read here, the rows with a long `event_id`, and the rows for the
/// row code.
///
/// # Safety
/// As `finish`; `rest` are rows of the group at `first`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn others(
    batch: &Batch,
    first: usize,
    rest: __mmask8,
    user_ts: __m512i,
    ingest_rel: __m512i,
    base: *const u8,
    bytes: usize,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    events: &mut Chunks<Ev>,
    long_events: &mut Chunks<EvL>,
    slow: &mut usize,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let (mut user_ts_of, mut ingest_of) = ([0u64; 8], [0u64; 8]);
        _mm512_storeu_si512(user_ts_of.as_mut_ptr().cast(), user_ts);
        _mm512_storeu_si512(ingest_of.as_mut_ptr().cast(), ingest_rel);
        let mut left = rest;
        while left != 0 {
            let lane = left.trailing_zeros() as usize;
            left &= left - 1;
            other_row::<Wide>(batch, first + lane, user_ts_of[lane], ingest_of[lane], base, bytes, ctx, dates, plan, events, long_events, slow);
        }
    }
}

/// `others` for eight rows of `finish_narrow`, their records' words in memory.
///
/// # Safety
/// As `others`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn others_narrow(
    batch: &Batch,
    first: usize,
    rest: __mmask8,
    user_ts_of: &[u64; 8],
    ingest_of: &[u64; 8],
    base: *const u8,
    bytes: usize,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    events: &mut Chunks<Ev>,
    long_events: &mut Chunks<EvL>,
    slow: &mut usize,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let mut left = rest;
        while left != 0 {
            let lane = left.trailing_zeros() as usize;
            left &= left - 1;
            other_row::<Narrow>(batch, first + lane, user_ts_of[lane], ingest_of[lane], base, bytes, ctx, dates, plan, events, long_events, slow);
        }
    }
}

/// One row of `others`, with the record's words `finish` made for it.
///
/// # Safety
/// As `others`; `row` is one of the batch's rows.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn other_row<W: Width>(
    batch: &Batch,
    row: usize,
    mut user_ts: u64,
    mut ingest_pay: u64,
    base: *const u8,
    bytes: usize,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    events: &mut Chunks<Ev>,
    long_events: &mut Chunks<EvL>,
    slow: &mut usize,
) {
    // SAFETY: the caller's contract; a list is the top bits of a hash.
    unsafe {
        let class = batch.class[row];
        if class as u32 != 0 {
            // The bytes from the row's line to the next row's, which are whole lines
            // (more than one when a short line hid in them).
            let (mut at, to) = (batch.line[row].offset_from(base) as usize, batch.line[row + 1].offset_from(base) as usize);
            while at < to {
                #[cfg(test)]
                SLOW_LINES.with(|count| count.set(count.get() + 1));
                *slow += 1;
                slow_line::<W>(base, bytes, at, ctx, dates, plan, events, long_events);
                at = line_end(base, bytes, at);
            }
            return;
        }
        if class & PURCHASE != 0 {
            // `event_row`: the row is valid when its price is, too.
            let (pay, price_ok) = purchase(batch.line[row + 1], ctx);
            if !price_ok {
                user_ts &= 0xFFFF_FFFF;
            }
            ingest_pay |= u64::from(pay) << 32;
        }
        if class & LONG_EVENT != 0 {
            let (hi, lo) = (batch.rows[row][0], batch.lows[row][0]);
            long_events.push_at(plan.long_part(hi, lo), EvL { hi, lo, user_ts, ingest_pay });
        } else {
            let key = batch.rows[row][0] + 1;
            events.push_at(plan.part(key), Ev { key, user_ts, ingest_pay });
        }
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

/// A row through the row code: the line at `at` tokenized again, then `event_row`.
///
/// # Safety
/// `p, len` is a segment and `at < len` a line's first byte in it.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn slow_line<W: Width>(p: *const u8, len: usize, at: usize, ctx: &Ctx, dates: &Dates, plan: &Plan, events: &mut Chunks<Ev>, long_events: &mut Chunks<EvL>) {
    // SAFETY: one line of the segment.
    unsafe {
        let bytes = line_end(p, len, at) - at;
        Scalar::lines::<7>(p.add(at), bytes, &mut Scratch::new(), |fields| event_row::<W>(ctx, dates, plan, fields, events, long_events));
    }
}

/// A click row through the row code.
///
/// # Safety
/// As `slow_line`.
#[cold]
#[inline(never)]
unsafe fn slow_click<W: Width>(p: *const u8, len: usize, at: usize, ctx: &Ctx, dates: &Dates, plan: &Plan, clicks: &mut Chunks<Ck>) {
    // SAFETY: one line of the segment.
    unsafe {
        let bytes = line_end(p, len, at) - at;
        Scalar::lines::<4>(p.add(at), bytes, &mut Scratch::new(), |fields| click_row::<W>(ctx, dates, plan, fields, clicks));
    }
}

/// The `pay` of the purchase whose line ends before `next`, and whether its price is valid,
/// as `event_row` computes them.
///
/// # Safety
/// The line is one the kernel took: its last two fields lie between its last two commas and
/// the newline at `next - 1`, each wrapped in quotes when it begins with one, and both
/// commas are in the 32 bytes before the newline.
#[inline(always)]
unsafe fn purchase(next: *const u8, ctx: &Ctx) -> (u32, bool) {
    // SAFETY: the caller's contract; a field's bytes have 16 readable after them.
    unsafe {
        let end = next.sub(1);
        let commas = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(end.sub(32).cast()), _mm256_set1_epi8(b',' as i8))) as u32;
        let z5 = commas.leading_zeros() as usize;
        let z4 = (commas.wrapping_shl(z5 as u32) << 1).leading_zeros() as usize;
        let (c5, c4) = (end.sub(z5 + 1), end.sub(z5 + z4 + 2));
        let (mut product, mut product_len) = (c4.add(1), z4);
        if *product == b'"' {
            (product, product_len) = (product.add(1), product_len - 2);
        }
        let (mut price, mut price_len) = (c5.add(1), z5);
        if price_len != 0 && *price == b'"' {
            (price, price_len) = (price.add(1), price_len - 2);
        }
        let (ok, cents) = price_cents(price, price_len);
        let category = ctx.products.get(pack_at(product, product_len)).unwrap_or(ctx.unknown);
        (cents | u32::from(category) << PAY_CATEGORY_SHIFT, ok)
    }
}

/// One shape of a click line's campaign: `c_` and 1 to 4 digits, wrapped in quotes or not, in
/// the 16 bytes from its first.
///
/// After the shuffle and the subtraction: bytes 0 to 3 the digits at the right, byte 4 the
/// first digit less `'1'` when there are several, bytes 5 to 8 `c`, `_` and the quotes less
/// themselves.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct CampRow {
    ctrl: [u8; 16],
    base: [u8; 16],
    lim: [u8; 16],
    w1: [i8; 16],
}

/// Rows of `CAMP`: the campaign's width and its comma, twice, plus one when its first byte is
/// a quote.
const CAMP_ROWS: usize = 65 * 2;

const fn camp_rows() -> [CampRow; CAMP_ROWS] {
    // No shape: the `c`'s byte reads 0xFF over a limit of 0.
    let mut bad = CampRow { ctrl: [0x80; 16], base: [0; 16], lim: [0; 16], w1: [0; 16] };
    bad.base[5] = 1;
    let mut table = [bad; CAMP_ROWS];
    let mut digits = 1;
    while digits <= 4 {
        let mut quoted = 0;
        while quoted < 2 {
            let mut row = CampRow { ctrl: [0x80; 16], base: [0; 16], lim: [0; 16], w1: [0; 16] };
            let mut k = 0;
            while k < digits {
                let at = 4 - digits + k;
                row.ctrl[at] = (quoted + 2 + k) as u8;
                row.base[at] = b'0';
                row.lim[at] = 9;
                k += 1;
            }
            row.w1 = [10, 1, 10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            if digits > 1 {
                row.ctrl[4] = (quoted + 2) as u8;
                row.base[4] = b'1';
                row.lim[4] = 8;
            }
            row.ctrl[5] = quoted as u8;
            row.base[5] = b'c';
            row.ctrl[6] = (quoted + 1) as u8;
            row.base[6] = b'_';
            if quoted == 1 {
                row.ctrl[7] = 0;
                row.base[7] = b'"';
                row.ctrl[8] = (digits + 3) as u8;
                row.base[8] = b'"';
            }
            // The width with its comma: `c_`, the digits, the quotes.
            table[(digits + 2 + 2 * quoted + 1) * 2 + quoted] = row;
            quoted += 1;
        }
        digits += 1;
    }
    table
}

static CAMP: [CampRow; CAMP_ROWS] = camp_rows();

/// What a click line's fields after the `user_id` give: the `ts` and the campaign's number,
/// each in a register's low word, and their residues.
struct ClickTail {
    ts: __m128i,
    campaign: __m128i,
    bad: __m128i,
}

/// A click line from the comma after its `user_id`, at `t`, on: the comma after its campaign
/// is `gap` bytes further and its newline at `end`.
///
/// # Safety
/// Everything from 16 bytes before `t` to 16 past `end` is readable, and `end` is past `t`.
#[inline(always)]
unsafe fn click_tail(t: *const u8, gap: usize, end: *const u8, days: *const u8, hundreds: __m128i) -> ClickTail {
    // SAFETY: the caller's contract; table indexes are clamped widths.
    unsafe {
        let gap = gap.min(64);
        // The `ts`: the 16 bytes before the newline.
        let width = (end.offset_from(t) as usize).wrapping_sub(gap).min(64);
        let row = &*TS.as_ptr().add(width * 2);
        let raw = _mm_loadu_si128(end.sub(16).cast());
        let bytes = _mm_sub_epi8(_mm_shuffle_epi8(raw, _mm_load_si128(row.ctrl.as_ptr().cast())), _mm_load_si128(row.base.as_ptr().cast()));
        let words = _mm_maddubs_epi16(bytes, _mm_load_si128(row.w1.as_ptr().cast()));
        let parts = _mm_madd_epi16(words, _mm_load_si128(row.w2.as_ptr().cast()));
        let parts_bad = _mm_cmpgt_epi32(parts, _mm_load_si128(row.limv.as_ptr().cast()));
        let both = _mm_madd_epi16(pack_words_128(parts), _mm_load_si128(row.w3.as_ptr().cast()));
        let bytes_bad = _mm_subs_epu8(bytes, _mm_load_si128(row.lim.as_ptr().cast()));
        let major = _mm_mul_epu32(both, _mm_load_si128(row.kme.as_ptr().cast()));
        let minor = _mm_mul_epi32(_mm_sign_epi32(_mm_srli_epi64::<32>(both), bytes), _mm_load_si128(row.kmf.as_ptr().cast()));
        let ts_date = t.add(gap + row.tq as usize);
        let ts_day = days.add(_pext_u64(u64::from(ts_date.add(7).cast::<u32>().read_unaligned()), row.pmask) as usize * 32);
        let date_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(row.dmask.as_ptr().cast()), _mm_loadu_si128(ts_date.cast()), _mm_load_si128(ts_day.cast()));
        let ts = _mm_add_epi64(_mm_add_epi64(major, minor), _mm_load_si128(ts_day.add(16).cast()));

        // The campaign's number.
        let wrapped = usize::from(*t.add(1) == b'"');
        let shape = &*CAMP.as_ptr().add(gap * 2 + wrapped);
        let raw = _mm_loadu_si128(t.add(1).cast());
        let bytes = _mm_sub_epi8(_mm_shuffle_epi8(raw, _mm_load_si128(shape.ctrl.as_ptr().cast())), _mm_load_si128(shape.base.as_ptr().cast()));
        let campaign_bad = _mm_subs_epu8(bytes, _mm_load_si128(shape.lim.as_ptr().cast()));
        let campaign = _mm_madd_epi16(_mm_maddubs_epi16(bytes, _mm_load_si128(shape.w1.as_ptr().cast())), hundreds);

        let narrow = _mm_ternarylogic_epi64::<0xFE>(bytes_bad, parts_bad, date_bad);
        ClickTail { ts, campaign, bad: _mm_or_si128(narrow, campaign_bad) }
    }
}

/// One click chunk, as `events`: a line whose checks all leave zero is a click as it stands
/// once its campaign is one of the table and its `ts` in the window. A line with a
/// well-formed `ts` outside the window is no click whatever else it holds, as in
/// `click_row`; every other line goes to `click_row` itself.
///
/// # Safety
/// `p, len` is a segment, and `lead` bytes before `p` are readable.
#[inline(never)]
pub(super) unsafe fn clicks<W: Kern>(p: *const u8, len: usize, lead: usize, ctx: &Ctx, dates: &Dates, campaigns: &Campaigns, plan: &Plan, out: &mut Out) {
    let Out { clicks, slow, .. } = out;
    let memo = Memo::of(ctx, dates);
    let lanes = Lanes::new();
    // Two batches in turn, as `events`.
    // SAFETY: plain integers, bytes and pointers that are not read before they are written.
    let mut pair: [Batch; 2] = unsafe { std::mem::zeroed() };
    // SAFETY: as `events`: a line the kernel runs on begins at least `WINDOW - PAD` bytes
    // before the segment's end, and its newline is the first from its first byte (or from
    // its `user_id`'s comma, with every byte before that matched), so the line is the
    // segment's.
    unsafe {
        let last = p.add(len);
        let start = p.add(16usize.saturating_sub(lead).min(len));
        let stop = p.add((len + crate::PAD).saturating_sub(WINDOW));
        let ts_end = W::click_consts(ctx);
        let hundreds = _mm_setr_epi16(100, 1, 0, 0, 0, 0, 0, 0);
        let days = memo.days.as_ptr().cast::<u8>();
        let pair: *mut Batch = pair.as_mut_ptr();
        let mut batch = pair;
        let mut rows = batch.cast::<u8>();
        let mut class = rows.add(std::mem::offset_of!(Batch, class));
        let mut lows = rows.add(std::mem::offset_of!(Batch, lows));
        let mut line = p;
        while line < last {
            if line >= start && line < stop {
                let mut k = 0usize;
                loop {
                    let commas = first64::<W>(line, lanes.comma);
                    let c0 = commas.trailing_zeros() as usize;
                    let rest = commas & commas.wrapping_sub(1);
                    let c1 = rest.trailing_zeros() as usize;
                    let (end, stored, word);
                    if c1 <= SHORT_C1 {
                        // Both IDs short: the newline is in the line's first 64 bytes.
                        let ends = first64::<W>(line, lanes.newline);
                        if ends == 0 {
                            break;
                        }
                        end = line.add(ends.trailing_zeros() as usize);
                        let c2 = (rest & rest.wrapping_sub(1)).trailing_zeros() as usize;
                        let shape = &*IDK.as_ptr().add(c0 * 64 + c1);
                        let raw = _mm256_loadu_si256(line.add(c0).sub(16).cast());
                        let ids = _mm256_sub_epi8(_mm256_shuffle_epi8(raw, _mm256_load_si256(shape.ctrl.as_ptr().cast())), _mm256_load_si256(shape.base.as_ptr().cast()));
                        let ids_bad = _mm256_min_epu8(_mm256_subs_epu8(_mm256_add_epi8(ids, lanes.to_letter), lanes.letter_max), _mm256_subs_epu8(ids, lanes.digit_max));
                        let rest_of = click_tail(line.add(c1), c2.wrapping_sub(c1), end, days, hundreds);
                        let bad = _mm256_or_si256(ids_bad, _mm256_zextsi128_si256(rest_of.bad));
                        // The line's commas are those three: the `ts` is the last field, and
                        // where an offset's sign is, a comma would pass for one.
                        let stray = (commas & _blsmsk_u64(ends)).count_ones() ^ 3;
                        let dirty = u64::from(_mm256_test_epi64_mask(bad, bad)) | u64::from(stray);
                        // The click's raw width, the `ts`, the user's digits, the campaign.
                        let user = _mm256_extract_epi64::<3>(ids);
                        stored = _mm256_set_epi64x(_mm_cvtsi128_si64(rest_of.campaign), user, _mm_cvtsi128_si64(rest_of.ts), c0 as i64);
                        word = dirty;
                    } else {
                        let c1 = if rest != 0 {
                            c1
                        } else {
                            64 + (_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(line.add(64).cast()), lanes.comma)) as u32).trailing_zeros() as usize
                        };
                        let t = line.add(c1);
                        // The campaign's comma and the newline, in the 64 bytes after the
                        // `user_id`'s.
                        let after = first64::<W>(t.add(1), lanes.comma);
                        let ends = first64::<W>(t.add(1), lanes.newline);
                        if ends == 0 {
                            break;
                        }
                        end = t.add(1 + ends.trailing_zeros() as usize);
                        let gap = 1 + after.trailing_zeros() as usize;
                        let click = one_id::<W>(line.add(c0), c0, &ONE_K, b'k', &lanes);
                        let user = one_id::<W>(t, c1.wrapping_sub(c0 + 1), &ONE_U, b'u', &lanes);
                        let rest_of = click_tail(t, gap, end, days, hundreds);
                        // One comma between the `user_id`'s and the newline: the campaign's.
                        let stray = (after & _blsmsk_u64(ends)).count_ones() ^ 1;
                        let dirty = u64::from(_mm_test_epi64_mask(rest_of.bad, rest_of.bad)) | u64::from(stray) | u64::from(click.2 | user.2 != 0);
                        stored = _mm256_set_epi64x(_mm_cvtsi128_si64(rest_of.campaign), user.0 as i64, _mm_cvtsi128_si64(rest_of.ts), c0 as i64);
                        word = dirty | if user.3 { LONG_USER } else { 0 };
                        lows.add(2 * k).cast::<[u64; 2]>().write([0, user.1]);
                    }
                    _mm256_store_si256(rows.add(4 * k).cast(), stored);
                    class.add(k).cast::<u64>().write(word);
                    class.add(k + 8 * BATCH).cast::<*const u8>().write(line);
                    k += 8;
                    line = end.add(1);
                    if k == 8 * BATCH {
                        (*batch).count = BATCH;
                        (*batch).line[BATCH] = line;
                        ask::<W>(&*batch, plan);
                        batch = other(pair, batch);
                        W::finish_clicks(batch, p, len, ts_end, campaigns, ctx, dates, plan, clicks, slow);
                        (rows, class, lows) = (batch.cast::<u8>(), batch.cast::<u8>().add(std::mem::offset_of!(Batch, class)), batch.cast::<u8>().add(std::mem::offset_of!(Batch, lows)));
                        k = 0;
                    }
                    if line >= stop {
                        break;
                    }
                }
                if k != 0 {
                    (*batch).count = k / 8;
                    (*batch).line[k / 8] = line;
                    ask::<W>(&*batch, plan);
                    batch = other(pair, batch);
                    W::finish_clicks(batch, p, len, ts_end, campaigns, ctx, dates, plan, clicks, slow);
                    (rows, class, lows) = (batch.cast::<u8>(), batch.cast::<u8>().add(std::mem::offset_of!(Batch, class)), batch.cast::<u8>().add(std::mem::offset_of!(Batch, lows)));
                }
            }
            if line < last {
                // A line of another length, or one of the segment's last or first.
                #[cfg(test)]
                SLOW_LINES.with(|count| count.set(count.get() + 1));
                *slow += 1;
                let at = line.offset_from(p) as usize;
                slow_click::<W>(p, len, at, ctx, dates, plan, clicks);
                line = p.add(line_end(p, len, at));
            }
        }
        // The batch that was asked for last.
        W::finish_clicks(other(pair, batch), p, len, ts_end, campaigns, ctx, dates, plan, clicks, slow);
    }
    clicks.fence();
}

/// `pack_words` for one lane.
#[inline(always)]
fn pack_words_128(parts: __m128i) -> __m128i {
    let packed: __m128i;
    // SAFETY: one AVX instruction on registers.
    unsafe { core::arch::asm!("vpackusdw {o}, {a}, {a}", o = lateout(xmm_reg) packed, a = in(xmm_reg) parts, options(pure, nomem, nostack, preserves_flags)) };
    packed
}

/// The click rows of a batch as records, and what is not one through the row code.
///
/// # Safety
/// As `finish`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn finish_clicks(
    batch: *mut Batch,
    base: *const u8,
    bytes: usize,
    ts_end: __m512i,
    campaigns: &Campaigns,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    clicks: &mut Chunks<Ck>,
    slow: &mut usize,
) {
    // SAFETY: as `finish`; the campaigns' table is read only at numbers below its count.
    unsafe {
        let batch = &mut *batch;
        let rows = std::mem::take(&mut batch.count);
        look::<Wide>(batch, rows, plan);
        // Bytes of a `click_id` that are kept (`click_id_at`).
        let kept = if plan.whole_clicks { usize::MAX } else { 9 };
        let all = |value: i64| _mm512_set1_epi64(value);
        let (even, odd) = (_mm512_setr_epi64(0, 4, 8, 12, 1, 5, 9, 13), _mm512_setr_epi64(2, 6, 10, 14, 3, 7, 11, 15));
        let mut first = 0;
        while first < rows {
            let live: __mmask8 = if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 };
            let at = batch.rows.as_ptr().add(first).cast::<__m512i>();
            let (r0, r1, r2, r3) = (_mm512_load_si512(at), _mm512_load_si512(at.add(1)), _mm512_load_si512(at.add(2)), _mm512_load_si512(at.add(3)));
            let (low_a, low_b) = (_mm512_permutex2var_epi64(r0, even, r1), _mm512_permutex2var_epi64(r2, even, r3));
            let (high_a, high_b) = (_mm512_permutex2var_epi64(r0, odd, r1), _mm512_permutex2var_epi64(r2, odd, r3));
            let ts = _mm512_shuffle_i64x2::<0b11_10_11_10>(low_a, low_b);
            let number = _mm512_and_si512(_mm512_shuffle_i64x2::<0b11_10_11_10>(high_a, high_b), all(0xFFFF_FFFF));

            // The campaign of that number, where the table has one.
            let listed = _mm512_cmplt_epu64_mask(number, all(Campaigns::NUMBERS as i64)) & live;
            let index = _mm512_and_si512(_mm512_mask_i64gather_epi64::<4>(all(-1), listed, number, campaigns.numbers.as_ptr().cast()), all(0xFFFF_FFFF));
            // `click_row`'s window.
            let inside = _mm512_cmpge_epi64_mask(ts, _mm512_setzero_si512()) & _mm512_cmplt_epi64_mask(ts, ts_end);
            let class = _mm512_loadu_si512(batch.class.as_ptr().add(first).cast());
            let clean = live & _mm512_testn_epi64_mask(class, all(!(LONG_USER as i64)));
            let fast = clean & inside & _mm512_cmplt_epu64_mask(index, all(i64::from(Campaigns::NONE)));
            // No click whatever the rest says: a well-formed `ts` outside the window.
            let dropped = clean & !inside;

            let (mut ts_of, mut index_of) = ([0u64; 8], [0u64; 8]);
            _mm512_storeu_si512(ts_of.as_mut_ptr().cast(), ts);
            _mm512_storeu_si512(index_of.as_mut_ptr().cast(), index);
            let mut left = fast;
            while left != 0 {
                let lane = left.trailing_zeros() as usize;
                left &= left - 1;
                let row = first + lane;
                // The `click_id`'s bytes: at the line's first, inside its quotes if any. Of
                // an ID spelled the long way, the first 9 (`click_id_at`): the kernel took the
                // row, so an ID of that length is one.
                let raw = batch.rows[row][0] as usize;
                let quoted = usize::from(*batch.line[row] == b'"');
                let click = pack_at(batch.line[row].add(quoted), (raw - 2 * quoted).min(kept));
                clicks.push_at(0, Ck { click, user_ts: u64::from(batch.users[row]) | ts_of[lane] << 32, campaign: index_of[lane] });
            }

            // Every other row, through the row code: the bytes from the row's line to the
            // next row's, which are whole lines (more than one when the commas that placed
            // the row were a later line's).
            let mut left = live & !fast & !dropped;
            while left != 0 {
                let row = first + left.trailing_zeros() as usize;
                left &= left - 1;
                let (mut at, to) = (batch.line[row].offset_from(base) as usize, batch.line[row + 1].offset_from(base) as usize);
                while at < to {
                    #[cfg(test)]
                    SLOW_LINES.with(|count| count.set(count.get() + 1));
                    *slow += 1;
                    slow_click::<Wide>(base, bytes, at, ctx, dates, plan, clicks);
                    at = line_end(base, bytes, at);
                }
            }
            first += 8;
        }
    }
}

/// `finish_clicks` with no 512-bit instruction (`types::width::Narrow`): eight rows a step,
/// which of them are clean by two 256-bit tests, and each clean row's window and campaign
/// on its own where `finish_clicks` compares and gathers eight in a register.
///
/// # Safety
/// As `finish_clicks`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn finish_clicks_narrow(
    batch: *mut Batch,
    base: *const u8,
    bytes: usize,
    ts_end: i64,
    campaigns: &Campaigns,
    ctx: &Ctx,
    dates: &Dates,
    plan: &Plan,
    clicks: &mut Chunks<Ck>,
    slow: &mut usize,
) {
    // SAFETY: as `finish_clicks`.
    unsafe {
        let batch = &mut *batch;
        let rows = std::mem::take(&mut batch.count);
        look::<Narrow>(batch, rows, plan);
        let kept = if plan.whole_clicks { usize::MAX } else { 9 };
        let mut first = 0;
        while first < rows {
            let live = _bzhi_u32(0xFF, (rows - first) as u32) as __mmask8;
            let clean = live & clean_rows(batch.class.as_ptr().add(first));
            // The clicks, and the rows whose campaign the table has not: the row code's. A
            // clean row with its `ts` outside the window is no click (`finish_clicks`).
            let mut slow_rows = live & !clean;
            let mut left = clean;
            while left != 0 {
                let lane = left.trailing_zeros() as usize;
                left &= left - 1;
                let row = first + lane;
                let [raw, ts, _, number] = batch.rows[row];
                if !(0..ts_end).contains(&(ts as i64)) {
                    continue;
                }
                let number = number & 0xFFFF_FFFF;
                let index = if number < Campaigns::NUMBERS as u64 { *campaigns.numbers.get_unchecked(number as usize) } else { Campaigns::NONE };
                if index == Campaigns::NONE {
                    slow_rows |= 1 << lane;
                    continue;
                }
                // As `finish_clicks`.
                let quoted = usize::from(*batch.line[row] == b'"');
                let click = pack_at(batch.line[row].add(quoted), (raw as usize - 2 * quoted).min(kept));
                clicks.push_at(0, Ck { click, user_ts: u64::from(batch.users[row]) | ts << 32, campaign: u64::from(index) });
            }

            let mut left = slow_rows;
            while left != 0 {
                let row = first + left.trailing_zeros() as usize;
                left &= left - 1;
                let (mut at, to) = (batch.line[row].offset_from(base) as usize, batch.line[row + 1].offset_from(base) as usize);
                while at < to {
                    #[cfg(test)]
                    SLOW_LINES.with(|count| count.set(count.get() + 1));
                    *slow += 1;
                    slow_click::<Narrow>(base, bytes, at, ctx, dates, plan, clicks);
                    at = line_end(base, bytes, at);
                }
            }
            first += 8;
        }
    }
}
