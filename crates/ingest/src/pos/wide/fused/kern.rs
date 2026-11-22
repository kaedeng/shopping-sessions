//! Slot `pos-kernel`: an event line turned into numbers where it is read, by tables.
//!
//! `pos-fused` copies a line's raw bytes as words and decodes them eight rows at a time. Here
//! the line's layout picks a few table rows, and those rows do the work in 256-bit registers:
//!
//! - The commas of the line's first 64 bytes give the first three fields; the newline is looked
//!   for in the 32 bytes from byte 64 (an event line is 65 to 96 bytes with it, nearly always),
//!   and the 32 bytes before it hold the last three fields, found from their end.
//! - The widths of `event_id` and `user_id` pick a row of `ID`: a shuffle that lines both IDs'
//!   digits up at the right, the letter, the quotes and the first digit beside them, and what
//!   to subtract so that every byte that is as it should be reads 0 or a digit's value.
//! - The width of `ts` and whether `ingest_ts` is wrapped pick a row of `TS`: the same for both
//!   timestamps' digits and literals, with the weights that turn them into seconds (or the
//!   13 digits of F3 into a number) in three multiply-adds.
//! - A date is looked up in a memo of the run's few dates by three of its bytes and compared
//!   whole (`Memo`); the type by its second byte (`KIND`); the last two fields' quotes by the
//!   two widths (`TAIL`).
//!
//! Nothing branches on what a line holds. Every check leaves a residue that is zero for a line
//! as the generator writes one; the residues are OR-ed into one word a row, and the rows go 64
//! at a time through `finish`, which unscrambles both IDs, makes the records of the rows whose
//! word is zero, reads price and product for the purchases among them, and sends every other
//! row's bytes through the row code (`slow_line`). So the kernel never has to be right about
//! an unusual line, only to recognise one: a line it takes is well-formed, with both IDs
//! spelled the generator's way, a `ts` that is F1, F2 or 13-digit F3, an F1 `ingest_ts`, a
//! date of the table in each, a type of the three, and a `product_id`.
//!
//! Why a taken line is exactly one well-formed line: bytes `0..=c4` (the first five fields and
//! their commas) are matched byte for byte, so none of them is a newline, a stray quote or an
//! extra comma; the last two fields lie between the last two commas before the newline, the
//! one found from the end must be the one the type's width gives from the front, and the 32
//! bytes before the newline hold no other newline.


use super::*;

/// The scrambled values of an ID are 40 bits.
const MASK40: u64 = scramble::MASK;

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

const fn id_rows() -> [IdRow; ID_ROWS] {
    // No shape: the letter's byte reads 0xFF.
    let mut bad = IdRow { ctrl: [0x80; 32], base: [0; 32] };
    bad.base[6] = 1;
    let mut table = [bad; ID_ROWS];
    let mut c0 = 2;
    while c0 <= 11 {
        let mut width = 2;
        while width <= 11 {
            let mut row = IdRow { ctrl: [0x80; 32], base: [0; 32] };
            if id_lane(&mut row, 0, b'e', 16 - c0, c0) && id_lane(&mut row, 16, b'u', 1, width) {
                table[c0 * 64 + c0 + 1 + width] = row;
            }
            width += 1;
        }
        c0 += 1;
    }
    table
}

static ID: [IdRow; ID_ROWS] = id_rows();

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
    _pad: [u64; 23],
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
        _pad: [0; 23],
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
struct Memo {
    days: [Day; 128],
    /// The earliest midnight of the table.
    first_ms: i64,
}

impl Memo {
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
struct Batch {
    /// The digits of the `event_id` (8 bytes, each less `'0'`), the `ts`, the digits of the
    /// `user_id`, the `ingest_ts`; the times in milliseconds after `Ctx::origin`.
    rows: [[u64; 4]; BATCH],
    /// 0 for a row that is a record as it stands, `PURCHASE` for one that is but for its price
    /// and product; anything else sends the row's bytes to the row code.
    class: [u64; BATCH],
    /// Where each row's line begins, and one more: where the last one ends.
    line: [*const u8; BATCH + 8],
}

/// What `finish` computes with, the same for every row.
struct Consts {
    event_key: __m512i,
    user_key: __m512i,
    limit: __m512i,
    persons: __m512i,
    event_lo: __m512i,
    ts_end: __m512i,
    first_ms: __m512i,
    ingest_add: __m512i,
    slot_mask: __m512i,
    shift: __m128i,
}

impl Consts {
    fn new(ctx: &Ctx, plan: &PosPlan, memo: &Memo) -> Consts {
        // SAFETY: register arithmetic; AVX-512F is compiled in.
        #[allow(unused_unsafe)]
        unsafe {
            let all = |value: i64| _mm512_set1_epi64(value);
            Consts {
                event_key: all((plan.event_key & MASK40) as i64),
                user_key: all((plan.user_key & MASK40) as i64),
                limit: all(plan.events() as i64),
                persons: all(i64::from(plan.persons)),
                event_lo: all(i64::from(ctx.event_lo)),
                ts_end: all(i64::from(ctx.ts_end)),
                first_ms: all(memo.first_ms),
                // `Ctx::ingest_rel` of the table's first midnight: whole seconds, as every
                // midnight is.
                ingest_add: all((memo.first_ms + ctx.origin).div_euclid(1000) - ctx.ingest_base()),
                slot_mask: all((1i64 << plan.shift) - 1),
                shift: _mm_cvtsi32_si128(plan.shift as i32),
            }
        }
    }
}

/// Eight times 8 base 36 digits (each byte a digit less `'0'`, the first in the low byte) as
/// their values.
#[inline(always)]
fn base36(digits: __m512i) -> __m512i {
    // SAFETY: register arithmetic.
    #[allow(unused_unsafe)]
    unsafe {
        // A letter less `'0'` is 49 to 74: 39 less is its value, and for a digit the sum wraps
        // far above it.
        let value = _mm512_min_epu8(digits, _mm512_add_epi8(digits, _mm512_set1_epi8(-39)));
        let pairs = _mm512_maddubs_epi16(value, _mm512_set1_epi16(0x0124));
        let quads = _mm512_madd_epi16(pairs, _mm512_set1_epi32(0x0001_0510));
        _mm512_add_epi64(_mm512_mul_epu32(quads, _mm512_set1_epi64(1_679_616)), _mm512_srli_epi64::<32>(quads))
    }
}

/// `scramble::number` of eight values.
#[inline(always)]
fn numbers(scrambled: __m512i, key: __m512i) -> __m512i {
    // SAFETY: register arithmetic; AVX-512 DQ is compiled in.
    #[allow(unused_unsafe)]
    unsafe {
        let mask = _mm512_set1_epi64(MASK40 as i64);
        let m = _mm512_and_si512(_mm512_mullo_epi64(scrambled, _mm512_set1_epi64(scramble::INV_B as i64)), mask);
        let m = _mm512_xor_si512(m, _mm512_xor_si512(_mm512_srli_epi64::<17>(m), _mm512_srli_epi64::<34>(m)));
        _mm512_and_si512(_mm512_mullo_epi64(_mm512_xor_si512(m, key), _mm512_set1_epi64(scramble::INV_A as i64)), mask)
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

/// One event chunk.
///
/// # Safety
/// `p, len` is a segment, and `lead` bytes before `p` are readable.
#[inline(never)]
pub(crate) unsafe fn events<M: Mode>(p: *const u8, len: usize, lead: usize, ctx: &Ctx, dates: &Dates, plan: &PosPlan, out: &mut PosOut) {
    let PosOut { events, pays, odd_events, odd_users, .. } = out;
    let limit = plan.events() as u64;
    let memo = Memo::new(ctx, dates);
    let consts = Consts::new(ctx, plan, &memo);
    let mut held = Held::new();
    // SAFETY: plain integers, bytes and pointers that are not read before they are written.
    let mut batch: Batch = unsafe { std::mem::zeroed() };
    // SAFETY: a line the kernel runs on begins at `line < stop`, at least 65 bytes before the
    // segment's end, so the newline it finds from byte 64 on is inside the segment, and
    // everything it reads lies between 16 bytes before the line and 128 after its first,
    // readable with the `PAD` bytes after the segment. Table indexes are bounded by the counts
    // they are made of (a comma's position is at most 64).
    unsafe {
        let last = p.add(len);
        // The kernel reads up to 16 bytes before a line, and 128 from it.
        let start = p.add(16usize.saturating_sub(lead).min(len));
        let stop = p.add((len + crate::PAD).saturating_sub(EVENT_WINDOW));
        let (comma, newline, quote) = (_mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b'"' as i8));
        // Per lane of the IDs: bytes 0..7 must be 0; byte 7 0..=8 or a letter less `'1'`;
        // bytes 8..16 0..=9 or a letter less `'0'`.
        let lane = |low: i8, seventh: i8, rest: i8| _mm256_setr_epi8(low, low, low, low, low, low, low, seventh, rest, rest, rest, rest, rest, rest, rest, rest, low, low, low, low, low, low, low, seventh, rest, rest, rest, rest, rest, rest, rest, rest);
        let (to_letter, digit_max, letter_max) = (lane(0, -48, -49), lane(0, 8, 9), lane(0, 25, 25));
        // The bytes of an `ingest_ts`'s date text that are compared: the date, `T`, a colon.
        let date_mask = _mm_setr_epi8(0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, 0);
        let days = memo.days.as_ptr().cast::<u8>();
        // The batch through one pointer: a row's class and its line are stored 8 bytes apart,
        // in step with its 32 bytes.
        let batch: *mut Batch = &raw mut batch;
        let rows = batch.cast::<u8>();
        let class = rows.add(std::mem::offset_of!(Batch, class));
        const _: () = assert!(std::mem::offset_of!(Batch, line) == std::mem::offset_of!(Batch, class) + 8 * BATCH && std::mem::offset_of!(Batch, rows) == 0);
        let mut line = p;
        while line < last {
            if line >= start && line < stop {
                // Eight times the rows stored: a row is 32 bytes, its class and its line 8.
                let mut k = 0usize;
                loop {
                    let ends = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(line.add(64).cast()), newline)) as u32;
                    if ends == 0 {
                        break;
                    }
                    let end = line.add(64 + ends.trailing_zeros() as usize);

                    // The first three commas.
                    let commas = _mm512_kunpackd(
                        u64::from(_mm256_cmpeq_epi8_mask(_mm256_loadu_si256(line.add(32).cast()), comma)),
                        u64::from(_mm256_cmpeq_epi8_mask(_mm256_loadu_si256(line.cast()), comma)),
                    );
                    let c0 = commas.trailing_zeros() as usize;
                    let rest = commas & commas.wrapping_sub(1);
                    let c1 = rest.trailing_zeros() as usize;
                    let rest = rest & rest.wrapping_sub(1);
                    let c2 = rest.trailing_zeros() as usize;

                    // The last two fields, from the newline back.
                    let tail_bytes = _mm256_loadu_si256(end.sub(32).cast());
                    let tail_commas = _mm256_movemask_epi8(_mm256_cmpeq_epi8(tail_bytes, comma)) as u32;
                    let tail_quotes = _mm256_movemask_epi8(_mm256_cmpeq_epi8(tail_bytes, quote)) as u32;
                    let z5 = tail_commas.leading_zeros();
                    let z4 = (tail_commas.wrapping_shl(z5) << 1).leading_zeros();
                    let tail = &*TAIL.as_ptr().add((z5 << 6 | z4) as usize);
                    let wrapped = _pext_u32(tail_quotes, tail.sel) as usize;
                    let tail_bad = (tail_quotes & tail.qmask) ^ *tail.pat.get_unchecked(wrapped);

                    // Both IDs.
                    let shape = &*ID.as_ptr().add(c0 * 64 + c1);
                    let raw = _mm256_loadu_si256(line.add(c0).sub(16).cast());
                    let ids = _mm256_sub_epi8(_mm256_shuffle_epi8(raw, _mm256_load_si256(shape.ctrl.as_ptr().cast())), _mm256_load_si256(shape.base.as_ptr().cast()));
                    let ids_bad = _mm256_min_epu8(_mm256_subs_epu8(_mm256_add_epi8(ids, to_letter), letter_max), _mm256_subs_epu8(ids, digit_max));

                    // Both timestamps.
                    let ingest_q = usize::from(*line.add(c2 + 1) == b'"');
                    let ingest = line.add(c2 + 2 * ingest_q);
                    let row = &*TS.as_ptr().add((c2 - c1) * 2 + ingest_q);
                    let raw = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_loadu_si128(line.add(c2).sub(16).cast())), _mm_loadu_si128(ingest.add(5).cast()));
                    let bytes = _mm256_sub_epi8(_mm256_shuffle_epi8(raw, _mm256_load_si256(row.ctrl.as_ptr().cast())), _mm256_load_si256(row.base.as_ptr().cast()));
                    let words = _mm256_maddubs_epi16(bytes, _mm256_load_si256(row.w1.as_ptr().cast()));
                    let parts = _mm256_madd_epi16(words, _mm256_load_si256(row.w2.as_ptr().cast()));
                    let parts_bad = _mm256_cmpgt_epi32(parts, _mm256_load_si256(row.limv.as_ptr().cast()));
                    let both = _mm256_madd_epi16(pack_words(parts), _mm256_load_si256(row.w3.as_ptr().cast()));
                    let bytes_bad = _mm256_subs_epu8(bytes, _mm256_load_si256(row.lim.as_ptr().cast()));
                    let major = _mm256_mul_epu32(both, _mm256_load_si256(row.kme.as_ptr().cast()));
                    let minor = _mm256_mul_epi32(_mm256_sign_epi32(_mm256_srli_epi64::<32>(both), bytes), _mm256_load_si256(row.kmf.as_ptr().cast()));

                    // Their dates.
                    let ts_date = line.add(c1 + row.tq as usize);
                    let ingest_date = line.add(c2 + ingest_q);
                    let ts_day = days.add(_pext_u64(u64::from(ts_date.add(7).cast::<u32>().read_unaligned()), row.pmask) as usize * 32);
                    let ingest_day = days.add(_pext_u64(u64::from(ingest_date.add(7).cast::<u32>().read_unaligned()), row.imask) as usize * 32);
                    // (text ^ day) & mask.
                    let ts_date_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(row.dmask.as_ptr().cast()), _mm_loadu_si128(ts_date.cast()), _mm_load_si128(ts_day.cast()));
                    let ingest_date_bad = _mm_ternarylogic_epi64::<0x60>(date_mask, _mm_loadu_si128(ingest_date.cast()), _mm_load_si128(ingest_day.cast()));
                    let midnights = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_load_si128(ts_day.add(16).cast())), _mm_load_si128(ingest_day.add(16).cast()));
                    let times = _mm256_add_epi64(_mm256_add_epi64(major, minor), midnights);

                    // The type, from the comma the `ingest_ts`'s shape puts before it.
                    let kind = &*KIND.as_ptr().add(usize::from(*ingest.add(23)));
                    let kind_bad = _mm_ternarylogic_epi64::<0x60>(_mm_load_si128(kind.mask.as_ptr().cast()), _mm_loadu_si128(ingest.add(21).cast()), _mm_load_si128(kind.cand.as_ptr().cast()));
                    // The comma after the type, from the front and from the end.
                    let apart = end.offset_from(ingest) as i64 - tail.gap - kind.width;

                    let narrow = _mm_ternarylogic_epi64::<0xFE>(ts_date_bad, ingest_date_bad, kind_bad);
                    let bad = _mm256_ternarylogic_epi64::<0xFE>(ids_bad, bytes_bad, parts_bad);
                    let bad = _mm256_ternarylogic_epi64::<0xFE>(bad, _mm256_cmpeq_epi8(tail_bytes, newline), _mm256_zextsi128_si256(narrow));
                    let dirty = u32::from(_mm256_test_epi64_mask(bad, bad));

                    // The digits' halves of the IDs beside the times: event, ts, user, ingest.
                    let stored = _mm256_castpd_si256(_mm256_shuffle_pd::<0b0101>(_mm256_castsi256_pd(ids), _mm256_castsi256_pd(times)));
                    _mm256_store_si256(rows.add(4 * k).cast(), stored);
                    class.add(k).cast::<u64>().write(u64::from(tail_bad | apart as u32 | dirty) | kind.class);
                    class.add(k + 8 * BATCH).cast::<*const u8>().write(line);
                    k += 8;
                    line = end.add(1);
                    if k == 8 * BATCH {
                        finish::<M>(batch, BATCH, line, p, len, &consts, &mut held, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                        k = 0;
                    }
                    if line >= stop {
                        break;
                    }
                }
                // The rows so far end where this line begins.
                if k != 0 {
                    finish::<M>(batch, k / 8, line, p, len, &consts, &mut held, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                }
            }
            if line < last {
                // A line of another length, or one of the segment's last or first.
                if held.len >= HELD {
                    held.flush::<M>(events);
                }
                #[cfg(test)]
                SLOW_LINES.with(|count| count.set(count.get() + 1));
                let at = line.offset_from(p) as usize;
                slow_line::<M>(p, len, at, &mut held, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                line = p.add(line_end(p, len, at));
            }
        }
    }
    held.flush::<M>(events);
    // Streamed lines become visible to the placing threads.
    events.fence();
    pays.fence();
}

/// The rows of a batch as records, and what is not one through the row code.
///
/// # Safety
/// `base, bytes` is the segment the rows' lines are in, `rows <= BATCH` rows are stored and
/// `next` is where the last one's line ends.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn finish<M: Mode>(
    batch: *mut Batch,
    rows: usize,
    next: *const u8,
    base: *const u8,
    bytes: usize,
    consts: &Consts,
    held: &mut Held,
    ctx: &Ctx,
    dates: &Dates,
    plan: &PosPlan,
    limit: u64,
    events: &mut Parts<PosEvent>,
    pays: &mut Parts<PosPay>,
    odd_events: &mut Vec<EventRec>,
    odd_users: &mut Vec<Id>,
) {
    let mut first = 0;
    // SAFETY: the batch is the caller's own. `first + 8 <= BATCH`; a lane past `rows` holds
    // an older row or zeros, and `live` keeps it out of the records and the row code.
    unsafe {
        let batch = &mut *batch;
        batch.line[rows] = next;
        let all = |value: i64| _mm512_set1_epi64(value);
        // Two rows a register: event, ts, user, ingest, twice. From two registers, the four
        // events then the four `ts`; the four users then the four `ingest_ts`.
        let (even, odd) = (_mm512_setr_epi64(0, 4, 8, 12, 1, 5, 9, 13), _mm512_setr_epi64(2, 6, 10, 14, 3, 7, 11, 15));
        while first < rows {
            if held.len + 8 > HELD {
                held.flush::<M>(events);
            }
            let live: __mmask8 = if rows - first >= 8 { 0xFF } else { (1u8 << (rows - first)) - 1 };
            let at = batch.rows.as_ptr().add(first).cast::<__m512i>();
            let (r0, r1, r2, r3) = (_mm512_load_si512(at), _mm512_load_si512(at.add(1)), _mm512_load_si512(at.add(2)), _mm512_load_si512(at.add(3)));
            let (low_a, low_b) = (_mm512_permutex2var_epi64(r0, even, r1), _mm512_permutex2var_epi64(r2, even, r3));
            let (high_a, high_b) = (_mm512_permutex2var_epi64(r0, odd, r1), _mm512_permutex2var_epi64(r2, odd, r3));
            let event = base36(_mm512_shuffle_i64x2::<0b01_00_01_00>(low_a, low_b));
            let ts = _mm512_shuffle_i64x2::<0b11_10_11_10>(low_a, low_b);
            let user = base36(_mm512_shuffle_i64x2::<0b01_00_01_00>(high_a, high_b));
            let ingest = _mm512_shuffle_i64x2::<0b11_10_11_10>(high_a, high_b);

            let (number, u) = (numbers(event, consts.event_key), numbers(user, consts.user_key));
            let mask = all(MASK40 as i64);
            let inside = _mm512_cmple_epu64_mask(event, mask)
                & _mm512_cmplt_epu64_mask(number, consts.limit)
                & _mm512_cmple_epu64_mask(user, mask)
                & _mm512_cmplt_epu64_mask(_mm512_srli_epi64::<{ ALIAS_BITS }>(u), consts.persons);
            // `event_row`'s `valid` for such a row: the window, and not late.
            let valid = _mm512_cmpge_epi64_mask(ts, consts.event_lo)
                & _mm512_cmplt_epi64_mask(ts, consts.ts_end)
                & _mm512_cmple_epi64_mask(_mm512_sub_epi64(ingest, ts), all(DAY));
            // `Ctx::ingest_rel`: whole seconds since the table's first midnight (under 2^32
            // ms, so the product's high bits are the quotient by 1000), then from its base.
            let seconds = _mm512_srli_epi64::<38>(_mm512_mul_epu32(_mm512_sub_epi64(ingest, consts.first_ms), all(274_877_907)));
            let ingest_rel = _mm512_add_epi64(seconds, consts.ingest_add);
            // `PosEvent::new`: u | ts << 32, then ingest | slot << 32 | flags << 48.
            let low = _mm512_or_si512(_mm512_and_si512(u, all(0xFFFF_FFFF)), _mm512_slli_epi64::<32>(ts));
            let plain = _mm512_or_si512(ingest_rel, _mm512_slli_epi64::<32>(_mm512_and_si512(number, consts.slot_mask)));
            let high = _mm512_mask_or_epi64(plain, valid, plain, all(i64::from(POS_VALID) << 48));
            let list = _mm512_srl_epi64(number, consts.shift);

            let class = _mm512_loadu_si512(batch.class.as_ptr().add(first).cast());
            let fast = inside & live & _mm512_cmpeq_epi64_mask(class, _mm512_setzero_si512());
            // Compressed in a register and stored whole: the lanes past the fast ones are
            // overwritten by the next store.
            let to = held.len;
            _mm512_storeu_si512(held.lists.as_mut_ptr().add(to).cast(), _mm512_maskz_compress_epi64(fast, list));
            _mm512_storeu_si512(held.low.as_mut_ptr().add(to).cast(), _mm512_maskz_compress_epi64(fast, low));
            _mm512_storeu_si512(held.high.as_mut_ptr().add(to).cast(), _mm512_maskz_compress_epi64(fast, high));
            held.len = to + fast.count_ones() as usize;

            if fast != live {
                let buys = inside & live & _mm512_cmpeq_epi64_mask(class, all(PURCHASE as i64));
                others::<M>(batch, first, live & !fast, buys, valid, list, low, plain, base, bytes, held, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
            }
            first += 8;
        }
    }
}

/// The rows of a group that are not records as they stand: the purchases (`buys`), whose
/// price and product are read here, and the rows for the row code.
///
/// # Safety
/// As `finish`; `rest` and `buys` are rows of the group at `first`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn others<M: Mode>(
    batch: &Batch,
    first: usize,
    rest: __mmask8,
    buys: __mmask8,
    valid: __mmask8,
    list: __m512i,
    low: __m512i,
    plain: __m512i,
    base: *const u8,
    bytes: usize,
    held: &mut Held,
    ctx: &Ctx,
    dates: &Dates,
    plan: &PosPlan,
    limit: u64,
    events: &mut Parts<PosEvent>,
    pays: &mut Parts<PosPay>,
    odd_events: &mut Vec<EventRec>,
    odd_users: &mut Vec<Id>,
) {
    // SAFETY: the caller's contract.
    unsafe {
        if buys != 0 {
            let (mut lists, mut lows, mut plains) = ([0u64; 8], [0u64; 8], [0u64; 8]);
            _mm512_storeu_si512(lists.as_mut_ptr().cast(), list);
            _mm512_storeu_si512(lows.as_mut_ptr().cast(), low);
            _mm512_storeu_si512(plains.as_mut_ptr().cast(), plain);
            let mut left = buys;
            while left != 0 {
                let lane = left.trailing_zeros() as usize;
                left &= left - 1;
                let (pay, price_ok) = purchase(batch.line[first + lane + 1], ctx);
                // `event_row`: the row is valid when its price is, too, and a valid purchase
                // leaves its `pay` beside its record.
                let valid = (valid >> lane & 1 == 1) & price_ok;
                let high = plains[lane] | u64::from(POS_VALID * u16::from(valid)) << 48;
                if held.len >= HELD {
                    held.flush::<M>(events);
                }
                held.put::<M>(lists[lane] as usize, PosEvent::from_words(lows[lane], high), events);
                if valid & (pay != 0) {
                    pays.push(lists[lane] as usize, PosPay { slot: (plains[lane] >> 32) as u32, ingest: plains[lane] as u32, pay });
                }
            }
        }
        // Everything else: the bytes from the row's line to the next row's, which are whole
        // lines (more than one when a short line hid in them).
        let mut left = rest & !buys;
        while left != 0 {
            let row = first + left.trailing_zeros() as usize;
            left &= left - 1;
            let (mut at, to) = (batch.line[row].offset_from(base) as usize, batch.line[row + 1].offset_from(base) as usize);
            while at < to {
                if held.len >= HELD {
                    held.flush::<M>(events);
                }
                #[cfg(test)]
                SLOW_LINES.with(|count| count.set(count.get() + 1));
                slow_line::<M>(base, bytes, at, held, ctx, dates, plan, limit, events, pays, odd_events, odd_users);
                at = line_end(base, bytes, at);
            }
        }
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

    const EVENT_KEY: u64 = 0x5A_1234_5678;
    const USER_KEY: u64 = 0x3C_8765_4321;

    /// An event line much as the generator writes one in October 2026, then perhaps spoiled.
    fn line(rng: &mut Rng) -> Vec<u8> {
        let id = |rng: &mut Rng, letter: u8, key: u64, most: u64| {
            let number = match rng.below(10) {
                0 => rng.next() & MASK40,
                1 => rng.below(40),
                _ => rng.below(most),
            };
            let mut text = Vec::new();
            types::id::unpack(scramble::id_of(number, key, letter), &mut text);
            String::from_utf8(text).unwrap()
        };
        let day = 1 + rng.below(31);
        let clock = |rng: &mut Rng| match rng.below(12) {
            0 => format!("{:02}:{:02}:{:02}", rng.below(25), rng.below(61), rng.below(61)),
            _ => format!("{:02}:{:02}:{:02}", rng.below(24), rng.below(60), rng.below(60)),
        };
        let at = clock(rng);
        let ts = match rng.below(16) {
            0..=7 => format!("2026-10-{day:02}T{at}Z"),
            8 | 9 => format!("2026-{}-{day:02}T{at}{}{:02}:{:02}", rng.pick(&["10", "10", "09", "11"]), rng.pick(&["+", "-"]), rng.below(25), rng.below(61)),
            10..=13 => format!("17{:011}", 91_000_000_000 + rng.below(2_500_000_000)),
            14 => format!("{}", rng.below(2_000_000_000_000)),
            _ => rng.pick(&["", "0", "2026-10-03", "2026-10-03T10:00:00", "1791071890000.0", "2026-10-03t10:00:00Z", "0001791071890000", "2026-10-03T10:00:00+0200", "2026-10-03T10:00:00 02:00"]).to_string(),
        };
        let ingest = match rng.below(12) {
            0 => rng.pick(&["", "2026-10-03 10:00:00Z", "1791071890000", "2026-10-03T10:00:00z", "2027-01-01T00:00:00Z"]).to_string(),
            _ => format!("2026-{}-{:02}T{}Z", rng.pick(&["10", "10", "10", "09", "11"]), 1 + rng.below(31), clock(rng)),
        };
        let fields = [
            match rng.below(14) {
                0 => rng.pick(&["", "E12ab", "e", "x1234567", "e123456789ab", "e0123", "ezzzzzzzz", "e12\u{0}45"]).to_string(),
                _ => id(rng, b'e', EVENT_KEY, 70_000),
            },
            match rng.below(14) {
                0 => rng.pick(&["", "U12ab", "u", "anonymous-user-17", "u0123", "u00"]).to_string(),
                _ => id(rng, b'u', USER_KEY, 4096 * 512),
            },
            ts,
            ingest,
            rng.pick(&["view", "view", "view", "cart", "purchase", "purchase", "refund", "", "VIEW", "views", "purchases"]).to_string(),
            rng.pick(&["p1", "p123", "p4711", "", "p99999", "p12345"]).to_string(),
            rng.pick(&["", "", "100", "2599", "0", "12a", "0250", "10000000", "10000001", "123456789"]).to_string(),
        ];
        let mut line = Vec::new();
        for (at, field) in fields.iter().enumerate() {
            if at > 0 {
                line.push(b',');
            }
            if rng.below(5) == 0 {
                line.push(b'"');
                line.extend_from_slice(field.as_bytes());
                line.push(b'"');
            } else {
                line.extend_from_slice(field.as_bytes());
            }
        }
        // Spoiled: a byte replaced, inserted or removed, mostly by one that matters.
        for _ in 0..[0, 0, 0, 0, 0, 1, 1, 2, 5][rng.below(9) as usize] {
            let at = rng.below(line.len() as u64 + 1) as usize;
            let byte = b"\"\",,\n a0Z:+-T\0\xff"[rng.below(15) as usize];
            match rng.below(3) {
                0 if at < line.len() => line[at] = byte,
                1 if at < line.len() => drop(line.remove(at)),
                _ => line.insert(at, byte),
            }
        }
        line
    }

    /// What a parse left, in an order that does not depend on the order of the rows.
    fn digest(out: &PosOut) -> (Vec<(usize, u64, u64, Id)>, Vec<(usize, u32, u32, u32)>, Vec<(Id, Id, u32, u32, u32, u32)>) {
        let mut events = Vec::new();
        let mut pays = Vec::new();
        for list in 0..out.events.lists() {
            for rec in out.events.pieces(list).into_iter().flatten() {
                let (low, high) = rec.words();
                // An odd user's index depends on the order of the rows: its ID instead.
                let odd = rec.flags() & POS_ODD_USER != 0;
                events.push((list, if odd { low >> 32 } else { low }, high, if odd { out.odd_users[rec.u() as usize] } else { 0 }));
            }
            for pay in out.pays.pieces(list).into_iter().flatten() {
                pays.push((list, pay.slot, pay.ingest, pay.pay));
            }
        }
        let mut odd: Vec<_> = out.odd_events.iter().map(|rec| (rec.id, rec.user, rec.ts, rec.ingest, rec.pay, rec.flags)).collect();
        events.sort_unstable();
        pays.sort_unstable();
        odd.sort_unstable();
        (events, pays, odd)
    }

    #[test]
    fn kernel_rows_are_the_row_codes() {
        let products: Vec<(Id, &[u8])> = vec![(types::id::pack(b"p1"), b"shoes"), (types::id::pack(b"p4711"), b"books"), (types::id::pack(b"p12345"), b"garden")];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut taken, mut lines) = (0usize, 0usize);
        for (start, end) in [("2026-10-08T00:00:00Z", "2026-10-22T00:00:00Z"), ("2026-10-05T00:00:00Z", "2026-11-02T00:00:00Z"), ("2026-09-28T12:00:00Z", "2026-10-12T12:00:00Z")] {
            let ctx = Ctx::new(parse_f1(start.as_bytes()).unwrap(), parse_f1(end.as_bytes()).unwrap(), &products, &[]).unwrap();
            let dates = Dates::new(&ctx);
            let shape = PosShape { shift: 12, parts: 20, persons: 4096, click_shift: 6, click_parts: 64 };
            let plan = PosPlan { user_key: USER_KEY, event_key: EVENT_KEY, shift: 12, parts: 20, persons: 4096, click_shift: 6, click_parts: 64, wide: false, fused: false, kernel: true };
            for round in 0..300 {
                // A segment: the byte before it, whole lines, `PAD` bytes after it.
                let lead = [0usize, 1, 15, 16, 64][round % 5];
                let mut buf = vec![b'x'; lead];
                buf.push(b'\n');
                let from = buf.len();
                for _ in 0..200 {
                    buf.extend(line(&mut rng));
                    buf.push(b'\n');
                }
                let len = buf.len() - from;
                lines += buf[from..].iter().filter(|&&b| b == b'\n').count();
                // What follows a segment: anything.
                buf.extend((0..crate::PAD).map(|at| b"\n,\"e1"[at % 5]));
                let new_out = || {
                    let (events, pays) = event_lists(1, &shape, None);
                    PosOut::new(events, pays, click_lists(1, &shape, 1000, 10, None), 1)
                };
                let (mut got, mut want) = (new_out(), new_out());
                // SAFETY: the buffer is a segment with `lead + 1` bytes before it.
                unsafe {
                    let p = buf.as_ptr().add(from);
                    events::<Plain>(p, len, lead + 1, &ctx, &dates, &plan, &mut got);
                    let PosOut { events, pays, odd_events, odd_users, scratch, .. } = &mut want;
                    let mut pending = Pending::new();
                    Scalar::lines::<7>(p, len, scratch, |fields| {
                        event_row::<Plain, Pending<PosEvent>>(&ctx, &dates, &plan, plan.events() as u64, fields, &mut pending, events, pays, odd_events, odd_users)
                    });
                    pending.flush::<Plain>(events);
                }
                let (got, want) = (digest(&got), digest(&want));
                taken += got.0.len();
                if got != want {
                    let text = String::from_utf8_lossy(&buf[from..from + len]);
                    for (at, (a, b)) in got.0.iter().zip(&want.0).enumerate() {
                        assert_eq!(a, b, "record {at} of round {round}");
                    }
                    panic!("round {round}: {} against {} records, {} against {} pays, {} against {} odd\n{text}", got.0.len(), want.0.len(), got.1.len(), want.1.len(), got.2.len(), want.2.len());
                }
            }
        }
        assert!(taken * 3 > lines, "{taken} records of {lines} lines");
    }

    /// How many of the generated-looking lines the kernel takes itself: nearly all.
    #[test]
    fn kernel_takes_ordinary_lines() {
        let ctx = Ctx::new(parse_f1(b"2026-10-08T00:00:00Z").unwrap(), parse_f1(b"2026-10-22T00:00:00Z").unwrap(), &[], &[]).unwrap();
        let dates = Dates::new(&ctx);
        let memo = Memo::new(&ctx, &dates);
        let mut rng = Rng(0xD6E8_FEB8_6659_FD93);
        let mut buf = vec![b'\n'; 64];
        let from = buf.len();
        let mut count = 0;
        while count < 4000 {
            let user = 512 * rng.below(4000) + rng.below(4);
            let (mut event_id, mut user_id) = (Vec::new(), Vec::new());
            types::id::unpack(scramble::id_of(rng.below(1 << 30), EVENT_KEY, b'e'), &mut event_id);
            types::id::unpack(scramble::id_of(user, USER_KEY, b'u'), &mut user_id);
            let ts = match rng.below(3) {
                0 => format!("2026-10-{:02}T{:02}:{:02}:{:02}Z", 8 + rng.below(14), rng.below(24), rng.below(60), rng.below(60)),
                1 => format!("2026-10-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}", 8 + rng.below(14), rng.below(24), rng.below(60), rng.below(60), rng.pick(&["+", "-"]), rng.below(13), rng.pick(&["00", "30"])),
                _ => format!("{}", ctx.window_start + rng.below(14 * 86_400_000) as i64),
            };
            let fields = [
                String::from_utf8(event_id).unwrap(),
                String::from_utf8(user_id).unwrap(),
                ts,
                format!("2026-10-{:02}T{:02}:{:02}:{:02}Z", 8 + rng.below(14), rng.below(24), rng.below(60), rng.below(60)),
                rng.pick(&["view", "view", "cart", "purchase"]).to_string(),
                format!("p{}", 1 + rng.below(50_000)),
                rng.pick(&["", "", "100", "25999"]).to_string(),
            ];
            let mut text = Vec::new();
            for (at, field) in fields.iter().enumerate() {
                if at > 0 {
                    text.push(b',');
                }
                if rng.below(5) == 0 {
                    text.push(b'"');
                    text.extend_from_slice(field.as_bytes());
                    text.push(b'"');
                } else {
                    text.extend_from_slice(field.as_bytes());
                }
            }
            // The kernel's lengths: 65 to 96 bytes with the newline.
            if (64..96).contains(&text.len()) {
                buf.extend(text);
                buf.push(b'\n');
                count += 1;
            }
        }
        let len = buf.len() - from;
        buf.extend([0u8; crate::PAD]);
        let shape = PosShape { shift: 16, parts: 1 << 14, persons: 4096, click_shift: 6, click_parts: 64 };
        let plan = PosPlan { user_key: USER_KEY, event_key: EVENT_KEY, shift: 16, parts: 1 << 14, persons: 4096, click_shift: 6, click_parts: 64, wide: false, fused: false, kernel: true };
        let (events_lists, pays) = event_lists(1, &shape, None);
        let mut out = PosOut::new(events_lists, pays, click_lists(1, &shape, 1000, 10, None), 1);
        let _ = &memo;
        // SAFETY: a segment with 64 bytes before it.
        unsafe { events::<Plain>(buf.as_ptr().add(from), len, 64, &ctx, &dates, &plan, &mut out) };
        assert_eq!(out.events.len(), 4000);
        assert!(out.odd_events.is_empty() && out.odd_users.is_empty());
        // The row code was not needed for more than the segment's last lines.
        assert!(SLOW_LINES.with(|count| count.get()) < 8, "{} lines went to the row code", SLOW_LINES.with(|count| count.get()));
    }
}
