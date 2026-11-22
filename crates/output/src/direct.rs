//! Slot `out-direct`: both tables as Parquet written by hand, one row group, one page a
//! column, uncompressed, no statistics. A `user_daily` file says the same as `out-arrow`'s; it
//! is shorter and takes far less work:
//!
//! - `user_id` is a dictionary column. A worker's rows come person by person, so a name goes
//!   into the dictionary once, when it changes, and the column's values are one run a person:
//!   17 bytes a person where the plain column is 13 bytes a row (the rules accept any string
//!   encoding);
//! - `day`, `sessions`, `events` and `revenue_cents` are PLAIN, which is the vectors' own
//!   bytes on a little-endian machine: each goes to the file with one `write`, uncopied;
//! - `top_category` is its definition levels, a bit a row, and the names of the rows that have
//!   one.
//!
//! Through arrow-rs every value passed through an Arrow array, then a page buffer, then the
//! file: about 200 MB of buffers for 100 MB of file at 20M events, most of it fresh memory.
//!
//! Rows that come narrow (`types::rows::UserDailyNarrow`, what `sessions::dense_walk` leaves)
//! are written by `user_daily_narrow`: the same table in half the bytes again, the day and
//! the counts as one or two bytes a row behind dictionaries that map a number to itself,
//! and every column handed to one `writev` from where it lies. At 20M events the eight files
//! are 41 MB where they were 81, and the stage 7 ms where it was 13.
//!
//! `campaign_daily` is a few thousand rows a part, all PLAIN: the two names with their lengths,
//! the day, the four counters as they lie. Through arrow-rs the table was a millisecond of a
//! run of twenty at 1M events, for 28,000 rows.
//!
//! The headers and the footer are Thrift's compact protocol (`Compact`), field numbers from
//! `parquet.thrift`. `tests` reads the files back with arrow-rs and compares them with the
//! rows and with `out-arrow`'s file; `scripts/check_parquet.py` does it with pyarrow, DuckDB
//! and Polars.

use std::fs::File;
use std::io::{IoSlice, Write};
use std::path::Path;

use types::Ctx;
use types::id::{self, Id, SLOW};
use types::rows::{CampaignDaily, EVENTS_HELD, NO_CATEGORY, NarrowPiece, UserDaily};

#[cfg(not(target_endian = "little"))]
compile_error!("out-direct writes the integer columns as the vectors' bytes: little-endian only");

/// Thrift compact protocol type numbers.
const I16: u8 = 4;
const I32: u8 = 5;
const I64: u8 = 6;
const BINARY: u8 = 8;
const LIST: u8 = 9;
const STRUCT: u8 = 12;

/// `parquet.thrift`: `Type`, `Encoding`, `PageType`, `ConvertedType`, `FieldRepetitionType`.
const INT32: i32 = 1;
const INT64: i32 = 2;
const BYTE_ARRAY: i32 = 6;
const PLAIN: i32 = 0;
const RLE: i32 = 3;
const RLE_DICTIONARY: i32 = 8;
const DATA_PAGE: i32 = 0;
const DICTIONARY_PAGE: i32 = 2;
const UTF8: i32 = 0;
const DATE: i32 = 6;
const REQUIRED: i32 = 0;
const OPTIONAL: i32 = 1;

/// Thrift's compact protocol, as much of it as Parquet's page headers and footer use.
struct Compact {
    bytes: Vec<u8>,
    /// The last field number written in each struct that is open.
    last: Vec<i16>,
}

impl Compact {
    fn new() -> Compact {
        Compact { bytes: Vec::new(), last: vec![0] }
    }

    fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.bytes.push(value as u8);
    }

    fn zigzag(&mut self, value: i64) {
        self.varint(((value << 1) ^ (value >> 63)) as u64);
    }

    /// A field's header: its number as a step from the struct's last where that fits 4 bits.
    fn field(&mut self, id: i16, kind: u8) {
        let last = self.last.last_mut().expect("a struct is open");
        let step = id - *last;
        *last = id;
        if (1..=15).contains(&step) {
            self.bytes.push((step as u8) << 4 | kind);
        } else {
            self.bytes.push(kind);
            self.zigzag(i64::from(id));
        }
    }

    fn i16(&mut self, id: i16, value: i16) {
        self.field(id, I16);
        self.zigzag(i64::from(value));
    }

    fn i32(&mut self, id: i16, value: i32) {
        self.field(id, I32);
        self.zigzag(i64::from(value));
    }

    fn i64(&mut self, id: i16, value: i64) {
        self.field(id, I64);
        self.zigzag(value);
    }

    fn text(&mut self, value: &[u8]) {
        self.varint(value.len() as u64);
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, id: i16, value: &[u8]) {
        self.field(id, BINARY);
        self.text(value);
    }

    /// A list field of `len` elements of `kind`; the caller writes the elements.
    fn list(&mut self, id: i16, kind: u8, len: usize) {
        self.field(id, LIST);
        if len < 15 {
            self.bytes.push((len as u8) << 4 | kind);
        } else {
            self.bytes.push(0xF0 | kind);
            self.varint(len as u64);
        }
    }

    /// Opens a struct that is a field; `close` ends it.
    fn open(&mut self, id: i16) {
        self.field(id, STRUCT);
        self.last.push(0);
    }

    /// Opens a struct that is a list's element.
    fn element(&mut self) {
        self.last.push(0);
    }

    fn close(&mut self) {
        self.bytes.push(0);
        self.last.pop();
    }
}

/// A `PageHeader` for an uncompressed page of `size` bytes and `values` values.
fn page_header(kind: i32, size: usize, values: usize, encoding: i32) -> Result<Vec<u8>, String> {
    let (size, values) = (i32::try_from(size).map_err(|_| "a page over 2 GiB")?, i32::try_from(values).map_err(|_| "over 2^31 rows in a page")?);
    let mut t = Compact::new();
    t.i32(1, kind);
    t.i32(2, size);
    t.i32(3, size);
    if kind == DICTIONARY_PAGE {
        // DictionaryPageHeader: num_values, encoding.
        t.open(7);
        t.i32(1, values);
        t.i32(2, PLAIN);
    } else {
        // DataPageHeader: num_values, encoding, and the two level encodings.
        t.open(5);
        t.i32(1, values);
        t.i32(2, encoding);
        t.i32(3, RLE);
        t.i32(4, RLE);
    }
    t.close();
    t.bytes.push(0);
    Ok(t.bytes)
}

/// One column chunk as the footer describes it.
struct Chunk {
    name: &'static str,
    kind: i32,
    encodings: &'static [i32],
    /// The file offsets of its dictionary page, if any, and of its data page.
    dictionary: Option<u64>,
    data: u64,
    /// Bytes of its pages, headers included.
    size: u64,
}

/// `(name, physical type, repetition, converted type)` of a column.
type Column = (&'static str, i32, i32, Option<i32>);

/// The columns of `user_daily`, in file order.
const COLUMNS: [Column; 6] = [
    ("user_id", BYTE_ARRAY, REQUIRED, Some(UTF8)),
    ("day", INT32, REQUIRED, Some(DATE)),
    ("sessions", INT64, REQUIRED, None),
    ("events", INT64, REQUIRED, None),
    ("revenue_cents", INT64, REQUIRED, None),
    ("top_category", BYTE_ARRAY, OPTIONAL, Some(UTF8)),
];

/// The columns of `campaign_daily`, in file order.
const CAMPAIGN_COLUMNS: [Column; 7] = [
    ("campaign_id", BYTE_ARRAY, REQUIRED, Some(UTF8)),
    ("channel", BYTE_ARRAY, REQUIRED, Some(UTF8)),
    ("day", INT32, REQUIRED, Some(DATE)),
    ("sessions", INT64, REQUIRED, None),
    ("purchasing_sessions", INT64, REQUIRED, None),
    ("revenue_cents", INT64, REQUIRED, None),
    ("assisted_sessions", INT64, REQUIRED, None),
];

/// The `FileMetaData` for `rows` rows of `columns` in `chunks` (none for an empty file: the
/// schema alone).
fn footer(columns: &[Column], rows: usize, chunks: &[Chunk]) -> Vec<u8> {
    let mut t = Compact::new();
    t.i32(1, 1);
    // The schema: the root, then a leaf a column.
    t.list(2, STRUCT, columns.len() + 1);
    t.element();
    t.i32(3, REQUIRED);
    t.string(4, b"arrow_schema");
    t.i32(5, columns.len() as i32);
    t.close();
    for &(name, kind, repetition, converted) in columns {
        t.element();
        t.i32(1, kind);
        t.i32(3, repetition);
        t.string(4, name.as_bytes());
        if let Some(converted) = converted {
            t.i32(6, converted);
            // LogicalType, a union: STRING is its field 1, DATE its field 6, each an empty struct.
            t.open(10);
            t.open(if converted == UTF8 { 1 } else { 6 });
            t.close();
            t.close();
        }
        t.close();
    }
    t.i64(3, rows as i64);
    t.list(4, STRUCT, usize::from(!chunks.is_empty()));
    if let Some(first) = chunks.first() {
        let bytes: u64 = chunks.iter().map(|chunk| chunk.size).sum();
        t.element();
        t.list(1, STRUCT, chunks.len());
        for chunk in chunks {
            // ColumnChunk: file_offset (0, as arrow-rs writes it), then its ColumnMetaData.
            t.element();
            t.i64(2, 0);
            t.open(3);
            t.i32(1, chunk.kind);
            t.list(2, I32, chunk.encodings.len());
            for &encoding in chunk.encodings {
                t.zigzag(i64::from(encoding));
            }
            t.list(3, BINARY, 1);
            t.text(chunk.name.as_bytes());
            t.i32(4, 0);
            t.i64(5, rows as i64);
            t.i64(6, chunk.size as i64);
            t.i64(7, chunk.size as i64);
            t.i64(9, chunk.data as i64);
            if let Some(dictionary) = chunk.dictionary {
                t.i64(11, dictionary as i64);
            }
            t.close();
            t.close();
        }
        t.i64(2, bytes as i64);
        t.i64(3, rows as i64);
        t.i64(5, first.dictionary.unwrap_or(first.data) as i64);
        t.i64(6, bytes as i64);
        t.i16(7, 0);
        t.close();
    }
    t.string(6, b"shopping-sessions out-direct");
    // ColumnOrder, a union whose field 1 is TypeDefinedOrder, an empty struct.
    t.list(7, STRUCT, columns.len());
    for _ in columns {
        t.element();
        t.open(1);
        t.close();
        t.close();
    }
    t.bytes.push(0);
    t.bytes
}

/// The `user_id` column: (dictionary page body, data page body, dictionary entries). A name
/// enters the dictionary whenever it differs from the row before, and the values are one RLE
/// run of that entry's index for the rows that share it. With `text_of`, a row's `Id` is not
/// a name but the caller's key for one, and `text_of` appends the dictionary's entries for
/// a list of keys: each name's length in four bytes, then its bytes.
fn names(person: &[Id], text_of: Option<Names>) -> (Vec<u8>, Vec<u8>, usize) {
    // 13 bytes a name at most while packed; slow-path IDs grow the buffer.
    let mut dictionary: Vec<u8> = Vec::with_capacity(person.len() / 2 * 13 + 64);
    let mut runs: Vec<u32> = Vec::with_capacity(person.len() / 2 + 16);
    let mut keys: Vec<Id> = Vec::new();
    let mut text = Vec::new();
    let mut at = 0;
    while at < person.len() {
        let id = person[at];
        let mut end = at + 1;
        while end < person.len() && person[end] == id {
            end += 1;
        }
        if text_of.is_some() {
            // All at once after the loop: the caller reads each name from tables far larger
            // than the caches, and with every key in hand it can ask for them ahead.
            keys.push(id);
        } else if id & SLOW != 0 {
            text.clear();
            id::unpack(id, &mut text);
            dictionary.extend_from_slice(&(text.len() as u32).to_le_bytes());
            dictionary.extend_from_slice(&text);
        } else {
            let (word, ninth, len) = id::spread(id);
            dictionary.extend_from_slice(&(len as u32).to_le_bytes());
            let mut bytes = [0u8; 9];
            bytes[..8].copy_from_slice(&word.to_le_bytes());
            bytes[8] = ninth;
            dictionary.extend_from_slice(&bytes[..len]);
        }
        runs.push((end - at) as u32);
        at = end;
    }
    if let Some(text_of) = text_of {
        text_of(&keys, &mut dictionary);
    }
    // The values: the indices' bit width, then for each entry a run header (rows << 1) and
    // the index in whole bytes.
    let width = (usize::BITS - runs.len().saturating_sub(1).leading_zeros()).max(1) as usize;
    let index_bytes = width.div_ceil(8);
    let mut values = Compact { bytes: Vec::with_capacity(runs.len() * (index_bytes + 2) + 1), last: Vec::new() };
    values.bytes.push(width as u8);
    for (index, &rows) in runs.iter().enumerate() {
        values.varint(u64::from(rows) << 1);
        values.bytes.extend_from_slice(&(index as u64).to_le_bytes()[..index_bytes]);
    }
    (dictionary, values.bytes, runs.len())
}

/// The `top_category` column's page body: the definition levels (their length, then one
/// bit-packed run of a bit a row), then the names of the rows that have a category.
fn categories(top_category: &[u8], ctx: &Ctx) -> Vec<u8> {
    let rows = top_category.len();
    let groups = rows.div_ceil(8);
    let mut levels = Compact { bytes: Vec::with_capacity(groups + 8), last: Vec::new() };
    levels.varint((groups as u64) << 1 | 1);
    let from = levels.bytes.len();
    levels.bytes.resize(from + groups, 0);
    let mut out = Vec::with_capacity(4 + levels.bytes.len() + rows * 4);
    let mut names = Vec::new();
    for (row, &category) in top_category.iter().enumerate() {
        if category != NO_CATEGORY {
            levels.bytes[from + row / 8] |= 1 << (row % 8);
            let name = &ctx.categories[category as usize];
            names.extend_from_slice(&(name.len() as u32).to_le_bytes());
            names.extend_from_slice(name);
        }
    }
    out.extend_from_slice(&(levels.bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&levels.bytes);
    out.extend_from_slice(&names);
    out
}

/// A vector's bytes: a PLAIN page of a fixed-width column.
fn bytes_of<T: Copy>(values: &[T]) -> &[u8] {
    // SAFETY: plain integers, no padding, and any byte is a valid `u8`.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values)) }
}

pub fn user_daily(folder: &Path, worker: usize, ctx: &Ctx, rows: UserDaily, text_of: Option<Names>) -> Result<(), String> {
    let path = folder.join(format!("part-{worker}.parquet"));
    write(&path, ctx, &rows, text_of).map_err(|error| format!("{}: {error}", path.display()))
}

/// Appends, for each key of a list, the name it stands for as a dictionary entry: its
/// length in four little-endian bytes, then its bytes (`crate::user_daily_named`).
pub type Names<'a> = &'a (dyn Fn(&[Id], &mut Vec<u8>) + Sync);

fn write(path: &Path, ctx: &Ctx, table: &UserDaily, text_of: Option<Names>) -> Result<(), String> {
    let io = |error: std::io::Error| error.to_string();
    let UserDaily { person, day, sessions, events, revenue_cents, top_category } = table;
    let rows = person.len();
    let mut file = File::create(path).map_err(io)?;
    let mut chunks: Vec<Chunk> = Vec::new();
    // What is not yet written, and where the file will be once it is.
    let mut pending: Vec<u8> = b"PAR1".to_vec();
    let mut offset = 4u64;
    if rows > 0 {
        // `user_id`: the dictionary page, then the data page of runs.
        let (dictionary, values, entries) = names(person, text_of);
        let dictionary_at = offset;
        let header = page_header(DICTIONARY_PAGE, dictionary.len(), entries, PLAIN)?;
        offset += (header.len() + dictionary.len()) as u64;
        pending.extend_from_slice(&header);
        pending.extend_from_slice(&dictionary);
        let data_at = offset;
        let header = page_header(DATA_PAGE, values.len(), rows, RLE_DICTIONARY)?;
        offset += (header.len() + values.len()) as u64;
        pending.extend_from_slice(&header);
        pending.extend_from_slice(&values);
        chunks.push(Chunk { name: "user_id", kind: BYTE_ARRAY, encodings: &[PLAIN, RLE, RLE_DICTIONARY], dictionary: Some(dictionary_at), data: data_at, size: offset - dictionary_at });

        // The four integer columns: a header, then the vector's own bytes.
        let plain: [(&'static str, i32, &[u8]); 4] =
            [("day", INT32, bytes_of(day)), ("sessions", INT64, bytes_of(sessions)), ("events", INT64, bytes_of(events)), ("revenue_cents", INT64, bytes_of(revenue_cents))];
        for (name, kind, body) in plain {
            let header = page_header(DATA_PAGE, body.len(), rows, PLAIN)?;
            chunks.push(Chunk { name, kind, encodings: &[PLAIN, RLE], dictionary: None, data: offset, size: (header.len() + body.len()) as u64 });
            offset += (header.len() + body.len()) as u64;
            pending.extend_from_slice(&header);
            file.write_all(&pending).map_err(io)?;
            pending.clear();
            file.write_all(body).map_err(io)?;
        }

        let body = categories(top_category, ctx);
        let header = page_header(DATA_PAGE, body.len(), rows, PLAIN)?;
        chunks.push(Chunk { name: "top_category", kind: BYTE_ARRAY, encodings: &[PLAIN, RLE], dictionary: None, data: offset, size: (header.len() + body.len()) as u64 });
        pending.extend_from_slice(&header);
        pending.extend_from_slice(&body);
    }
    let footer = footer(&COLUMNS, rows, &chunks);
    pending.extend_from_slice(&footer);
    pending.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    pending.extend_from_slice(b"PAR1");
    file.write_all(&pending).map_err(io)
}

/// A file as the pieces it is written from, in order: the small ones (page headers, a
/// dictionary of a few entries, the footer) are stretches of one buffer, the large ones the
/// columns' own memory. One `writev` takes them all, so nothing large is copied before the
/// kernel's copy, and a file is one call where `write` was five.
struct Pieces<'a> {
    small: Vec<u8>,
    list: Vec<Piece<'a>>,
    /// Where the file is once the pieces so far are written.
    offset: u64,
}

enum Piece<'a> {
    /// `small[from..to]`.
    Small(usize, usize),
    Large(&'a [u8]),
}

impl<'a> Pieces<'a> {
    fn new() -> Pieces<'a> {
        Pieces { small: Vec::with_capacity(32 << 10), list: Vec::with_capacity(48), offset: 0 }
    }

    fn small(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let from = self.small.len();
        self.small.extend_from_slice(bytes);
        self.offset += bytes.len() as u64;
        // One piece with the stretch before it, when that is the piece before.
        match self.list.last_mut() {
            Some(Piece::Small(_, to)) if *to == from => *to = self.small.len(),
            _ => self.list.push(Piece::Small(from, self.small.len())),
        }
    }

    fn large(&mut self, bytes: &'a [u8]) {
        if !bytes.is_empty() {
            self.list.push(Piece::Large(bytes));
            self.offset += bytes.len() as u64;
        }
    }

    fn write(&self, file: &mut File) -> std::io::Result<()> {
        let mut slices: Vec<IoSlice> = self
            .list
            .iter()
            .map(|piece| {
                IoSlice::new(match piece {
                    Piece::Small(from, to) => &self.small[*from..*to],
                    Piece::Large(bytes) => bytes,
                })
            })
            .collect();
        let mut left = &mut slices[..];
        while !left.is_empty() {
            match file.write_vectored(left) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(written) => IoSlice::advance_slices(&mut left, written),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// A PLAIN page of a fixed-width column: a header, then the column's bytes.
fn plain<'a>(file: &mut Pieces<'a>, chunks: &mut Vec<Chunk>, name: &'static str, kind: i32, body: &'a [u8], rows: usize) -> Result<(), String> {
    let at = file.offset;
    file.small(&page_header(DATA_PAGE, body.len(), rows, PLAIN)?);
    file.large(body);
    chunks.push(Chunk { name, kind, encodings: &[PLAIN, RLE], dictionary: None, data: at, size: file.offset - at });
    Ok(())
}

/// A column of `kind` whose values are `entries[index]` for a row's `index`, the indices
/// `width` bytes each (1 or 2) in `indices`: a dictionary page of the entries, then a data
/// page whose values are one bit-packed run at `8 * width` bits an index. Bit-packed at a
/// whole number of bytes, the run is the indices' own bytes (and zeros up to a group of
/// eight), so the column goes to the file as it lies.
#[allow(clippy::too_many_arguments)]
fn indexed<'a>(file: &mut Pieces<'a>, chunks: &mut Vec<Chunk>, name: &'static str, kind: i32, entries: impl Iterator<Item = i64>, indices: &'a [u8], width: usize, rows: usize) -> Result<(), String> {
    debug_assert_eq!(indices.len(), rows * width);
    let mut dictionary: Vec<u8> = Vec::new();
    let mut count = 0;
    for entry in entries {
        if kind == INT32 {
            dictionary.extend_from_slice(&(entry as i32).to_le_bytes());
        } else {
            dictionary.extend_from_slice(&entry.to_le_bytes());
        }
        count += 1;
    }
    let at = file.offset;
    file.small(&page_header(DICTIONARY_PAGE, dictionary.len(), count, PLAIN)?);
    file.small(&dictionary);
    let data = file.offset;
    let groups = rows.div_ceil(8);
    let mut lead = Compact { bytes: vec![(8 * width) as u8], last: Vec::new() };
    lead.varint((groups as u64) << 1 | 1);
    let packed = groups * 8 * width;
    file.small(&page_header(DATA_PAGE, lead.bytes.len() + packed, rows, RLE_DICTIONARY)?);
    file.small(&lead.bytes);
    file.large(indices);
    file.small(&[0u8; 16][..packed - indices.len()]);
    chunks.push(Chunk { name, kind, encodings: &[PLAIN, RLE, RLE_DICTIONARY], dictionary: Some(at), data, size: file.offset - at });
    Ok(())
}

/// Appends the dictionary entry of a name that is an `Id`: its length in four bytes, then
/// its bytes. `text` is scratch.
fn entry(id: Id, dictionary: &mut Vec<u8>, text: &mut Vec<u8>) {
    if id & SLOW != 0 {
        text.clear();
        id::unpack(id, text);
        dictionary.extend_from_slice(&(text.len() as u32).to_le_bytes());
        dictionary.extend_from_slice(text);
    } else {
        let (word, ninth, len) = id::spread(id);
        dictionary.extend_from_slice(&(len as u32).to_le_bytes());
        let mut bytes = [0u8; 9];
        bytes[..8].copy_from_slice(&word.to_le_bytes());
        bytes[8] = ninth;
        dictionary.extend_from_slice(&bytes[..len]);
    }
}

/// The `user_id` column of rows that name a person once (`NarrowPiece`, whose first row is
/// the table's row `from`): the dictionary page's body, an entry a run, and the data
/// page's, a run of rows an entry.
fn names_of_runs(person: &[Id], first: &[u32], from: usize, rows: usize, text_of: Option<Names>) -> (Vec<u8>, Vec<u8>) {
    let mut dictionary: Vec<u8> = Vec::new();
    match text_of {
        Some(text_of) => text_of(person, &mut dictionary),
        None => {
            dictionary.reserve(person.len() * 13 + 64);
            let mut text = Vec::new();
            for &id in person {
                entry(id, &mut dictionary, &mut text);
            }
        }
    }
    let runs = person.len();
    let width = (usize::BITS - runs.saturating_sub(1).leading_zeros()).max(1) as usize;
    let index_bytes = width.div_ceil(8);
    assert!(index_bytes <= 4, "a file's persons are numbered in 32 bits");
    // A run is its header, `rows << 1` in up to five bytes, and its index: stored a word at
    // a time, the header's one byte and the index together for a run of under 64 rows.
    let mut values: Vec<u8> = Vec::with_capacity(1 + runs * (5 + index_bytes) + 8);
    values.push(width as u8);
    // SAFETY: a run takes at most `5 + index_bytes` bytes of the room made above, and a
    // word stored at its last byte ends inside the 8 bytes past all of them.
    unsafe {
        let start = values.as_mut_ptr();
        let mut at = start.add(1);
        for run in 0..runs {
            let end = first.get(run + 1).map_or((from + rows) as u64, |&next| u64::from(next));
            let len = end - u64::from(first[run]);
            debug_assert!(len > 0 && len <= u64::from(u32::MAX));
            if len < 64 {
                at.cast::<u64>().write_unaligned(((run as u64) << 8 | len << 1).to_le());
                at = at.add(1 + index_bytes);
            } else {
                let mut header = len << 1;
                while header >= 0x80 {
                    *at = header as u8 | 0x80;
                    at = at.add(1);
                    header >>= 7;
                }
                *at = header as u8;
                at.add(1).cast::<u64>().write_unaligned((run as u64).to_le());
                at = at.add(1 + index_bytes);
            }
        }
        values.set_len(at.offset_from(start) as usize);
    }
    (dictionary, values)
}

/// The `top_category` column of `rows` rows as its pieces: the definition levels (their
/// length, then one bit-packed run of a bit a row), and the categories of the rows that
/// have one, a byte each in row order, with zeros up to a group of eight; and how many
/// those are. Each row is stored and kept if it has a category: no branch on what a row is.
fn categories_packed(top_category: &[u8]) -> (Vec<u8>, Vec<u8>, usize) {
    let rows = top_category.len();
    let groups = rows.div_ceil(8);
    let mut levels = Compact { bytes: Vec::with_capacity(groups + 16), last: Vec::new() };
    levels.bytes.extend_from_slice(&[0; 4]);
    levels.varint((groups as u64) << 1 | 1);
    let length = (levels.bytes.len() - 4 + groups) as u32;
    levels.bytes[..4].copy_from_slice(&length.to_le_bytes());
    let mut indices: Vec<u8> = Vec::with_capacity(rows + 16);
    let mut named = 0;
    // SAFETY: at most `rows` bytes are stored into room for `rows + 16`, and the zeros
    // after them end inside it.
    unsafe {
        let to = indices.as_mut_ptr();
        for group in top_category.chunks(8) {
            let mut bits = 0u8;
            for (bit, &category) in group.iter().enumerate() {
                let has = category != NO_CATEGORY;
                bits |= u8::from(has) << bit;
                *to.add(named) = category;
                named += usize::from(has);
            }
            levels.bytes.push(bits);
        }
        to.add(named).write_bytes(0, 8);
        indices.set_len(named.div_ceil(8) * 8);
    }
    (levels.bytes, indices, named)
}

/// `user_daily` for rows left narrow: the same table in half the bytes, and the columns
/// written from where they lie.
///
/// - `user_id` as `write` has it, an entry and a run a person; the table says where a
///   person's rows begin, so no row is compared with the one before.
/// - `day`, `sessions` and `events` are dictionary columns whose dictionary maps a number to
///   itself (a day to `day0` and it), and whose indices are bit-packed at 8, 8 and 16 bits:
///   the table's columns as they lie, 4 bytes a row where the PLAIN columns are 20. The
///   physical types stay INT32 and INT64, as in every other file of the table. A row with
///   more events than the dictionary's identity part has an entry of its own after it
///   (`rows::EVENTS_HELD`); a column with more such rows than indices are left, or with a
///   row past `rows::SESSIONS_HELD`, is written PLAIN.
/// - `revenue_cents` is PLAIN, the vector's bytes.
/// - `top_category` is a dictionary column too: its definition levels, then a byte for each
///   row that has a category, where the PLAIN page spelled the name out for each.
pub fn user_daily_narrow(folder: &Path, part: usize, ctx: &Ctx, rows: NarrowPiece, text_of: Option<Names>) -> Result<(), String> {
    let path = folder.join(format!("part-{part}.parquet"));
    if rows.is_empty() {
        return write(&path, ctx, &UserDaily::default(), None).map_err(|error| format!("{}: {error}", path.display()));
    }
    write_narrow(&path, ctx, rows, text_of).map_err(|error| format!("{}: {error}", path.display()))
}

fn write_narrow(path: &Path, ctx: &Ctx, table: NarrowPiece, text_of: Option<Names>) -> Result<(), String> {
    let io = |error: std::io::Error| error.to_string();
    let rows = table.len();
    let mut file = File::create(path).map_err(io)?;
    let NarrowPiece { person, first, from, day, sessions, events, revenue_cents, top_category, wide_sessions, wide_events } = table;
    // A row with many events is given the dictionary entry after the identity part's and
    // those of the rows before it, while indices of 16 bits last.
    let events_indexed = wide_events.len() <= (1 << 16) - EVENTS_HELD as usize;
    if events_indexed {
        for (entry, &(row, _)) in wide_events.iter().enumerate() {
            events[row as usize - from] = (EVENTS_HELD as usize + entry) as u16;
        }
    }
    let events = &*events;
    let sessions_indexed = wide_sessions.is_empty();
    // The columns that do not fit their narrow form, whole.
    let patched = |narrow: &mut dyn Iterator<Item = i64>, wide: &[(u32, u32)]| {
        let mut whole: Vec<i64> = narrow.collect();
        for &(row, count) in wide {
            whole[row as usize - from] = i64::from(count);
        }
        whole
    };
    let sessions_plain = (!sessions_indexed).then(|| patched(&mut sessions.iter().map(|&count| i64::from(count)), wide_sessions));
    let events_plain = (!events_indexed).then(|| patched(&mut events.iter().map(|&count| i64::from(count)), wide_events));
    let (dictionary, values) = names_of_runs(person, first, from, rows, text_of);
    let (levels, categories, named) = categories_packed(top_category);

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut pieces = Pieces::new();
    pieces.small(b"PAR1");
    // `user_id`: the dictionary page, then the data page of runs.
    let at = pieces.offset;
    pieces.small(&page_header(DICTIONARY_PAGE, dictionary.len(), person.len(), PLAIN)?);
    pieces.large(&dictionary);
    let data = pieces.offset;
    pieces.small(&page_header(DATA_PAGE, values.len(), rows, RLE_DICTIONARY)?);
    pieces.large(&values);
    chunks.push(Chunk { name: "user_id", kind: BYTE_ARRAY, encodings: &[PLAIN, RLE, RLE_DICTIONARY], dictionary: Some(at), data, size: pieces.offset - at });

    let last_day = i64::from(day.iter().copied().max().unwrap_or(0));
    indexed(&mut pieces, &mut chunks, "day", INT32, (0..=last_day).map(|day| i64::from(ctx.day0) + day), day, 1, rows)?;
    match &sessions_plain {
        Some(whole) => plain(&mut pieces, &mut chunks, "sessions", INT64, bytes_of(whole), rows)?,
        None => indexed(&mut pieces, &mut chunks, "sessions", INT64, 0..=i64::from(sessions.iter().copied().max().unwrap_or(0)), sessions, 1, rows)?,
    }
    match &events_plain {
        Some(whole) => plain(&mut pieces, &mut chunks, "events", INT64, bytes_of(whole), rows)?,
        None => {
            // The identity part no longer than the counts ask for, then the wide rows' own.
            let most = events.iter().copied().max().unwrap_or(0);
            let identity = if wide_events.is_empty() { i64::from(most) + 1 } else { i64::from(EVENTS_HELD) };
            indexed(&mut pieces, &mut chunks, "events", INT64, (0..identity).chain(wide_events.iter().map(|&(_, count)| i64::from(count))), bytes_of(events), 2, rows)?
        }
    }
    plain(&mut pieces, &mut chunks, "revenue_cents", INT64, bytes_of(revenue_cents), rows)?;

    // `top_category`: with no row that has one, the levels alone, as a PLAIN page.
    let at = pieces.offset;
    if named == 0 {
        pieces.small(&page_header(DATA_PAGE, levels.len(), rows, PLAIN)?);
        pieces.large(&levels);
        chunks.push(Chunk { name: "top_category", kind: BYTE_ARRAY, encodings: &[PLAIN, RLE], dictionary: None, data: at, size: pieces.offset - at });
    } else {
        let mut names: Vec<u8> = Vec::new();
        for name in &ctx.categories {
            names.extend_from_slice(&(name.len() as u32).to_le_bytes());
            names.extend_from_slice(name);
        }
        pieces.small(&page_header(DICTIONARY_PAGE, names.len(), ctx.categories.len(), PLAIN)?);
        pieces.small(&names);
        let data = pieces.offset;
        let mut lead = Compact { bytes: vec![8], last: Vec::new() };
        lead.varint((categories.len() as u64 / 8) << 1 | 1);
        pieces.small(&page_header(DATA_PAGE, levels.len() + lead.bytes.len() + categories.len(), rows, RLE_DICTIONARY)?);
        pieces.large(&levels);
        pieces.small(&lead.bytes);
        pieces.large(&categories);
        chunks.push(Chunk { name: "top_category", kind: BYTE_ARRAY, encodings: &[PLAIN, RLE, RLE_DICTIONARY], dictionary: Some(at), data, size: pieces.offset - at });
    }
    let footer = footer(&COLUMNS, rows, &chunks);
    pieces.small(&footer);
    pieces.small(&(footer.len() as u32).to_le_bytes());
    pieces.small(b"PAR1");
    pieces.write(&mut file).map_err(io)
}

/// Writes `rows` to `folder/part-{part}.parquet`: the whole file built in one buffer and
/// written with one `write`.
pub fn campaign_daily(folder: &Path, part: usize, ctx: &Ctx, rows: &CampaignDaily) -> Result<(), String> {
    let path = folder.join(format!("part-{part}.parquet"));
    let file = campaign_file(ctx, rows).map_err(|error| format!("{}: {error}", path.display()))?;
    File::create(&path).and_then(|mut to| to.write_all(&file)).map_err(|error| format!("{}: {error}", path.display()))
}

/// A `campaign_daily` file's bytes.
fn campaign_file(ctx: &Ctx, table: &CampaignDaily) -> Result<Vec<u8>, String> {
    let CampaignDaily { campaign, day, sessions, purchasing_sessions, revenue_cents, assisted_sessions } = table;
    let rows = campaign.len();
    // A name is its length and its bytes; the longest sizes the two name pages.
    let longest = ctx.campaigns.iter().map(|(id, channel)| id.len().max(channel.len())).max().unwrap_or(0);
    let mut file: Vec<u8> = Vec::with_capacity(rows * (2 * (4 + longest) + 4 + 4 * 8) + 1024);
    file.extend_from_slice(b"PAR1");
    let mut chunks: Vec<Chunk> = Vec::new();
    if rows > 0 {
        let mut page = |file: &mut Vec<u8>, name: &'static str, kind: i32, body: &[u8]| -> Result<(), String> {
            let header = page_header(DATA_PAGE, body.len(), rows, PLAIN)?;
            chunks.push(Chunk { name, kind, encodings: &[PLAIN, RLE], dictionary: None, data: file.len() as u64, size: (header.len() + body.len()) as u64 });
            file.extend_from_slice(&header);
            file.extend_from_slice(body);
            Ok(())
        };
        let mut names: Vec<u8> = Vec::with_capacity(rows * (4 + longest));
        for (column, which) in [("campaign_id", 0), ("channel", 1)] {
            names.clear();
            for &at in campaign {
                let (id, channel) = &ctx.campaigns[at as usize];
                let name = if which == 0 { id } else { channel };
                names.extend_from_slice(&(name.len() as u32).to_le_bytes());
                names.extend_from_slice(name);
            }
            page(&mut file, column, BYTE_ARRAY, &names)?;
        }
        page(&mut file, "day", INT32, bytes_of(day))?;
        page(&mut file, "sessions", INT64, bytes_of(sessions))?;
        page(&mut file, "purchasing_sessions", INT64, bytes_of(purchasing_sessions))?;
        page(&mut file, "revenue_cents", INT64, bytes_of(revenue_cents))?;
        page(&mut file, "assisted_sessions", INT64, bytes_of(assisted_sessions))?;
    }
    let footer = footer(&CAMPAIGN_COLUMNS, rows, &chunks);
    file.extend_from_slice(&footer);
    file.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    Ok(file)
}

#[cfg(all(test, feature = "out-arrow"))]
mod tests {
    use std::path::PathBuf;

    use arrow_array::cast::AsArray;
    use arrow_array::types::{Date32Type, Int64Type};
    use arrow_array::{Array, RecordBatch};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use types::rows::UserDailyNarrow;
    use types::time::parse_f1;

    use super::*;

    /// `(user_id, day, sessions, events, revenue_cents, top_category)` of every row of a file.
    type Row = (String, i32, i64, i64, i64, Option<String>);

    fn read(path: &Path) -> Vec<Row> {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap().build().unwrap();
        let mut rows = Vec::new();
        for batch in reader {
            let batch: RecordBatch = batch.unwrap();
            let schema = batch.schema();
            let names: Vec<&str> = schema.fields().iter().map(|field| field.name().as_str()).collect();
            assert_eq!(names, ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"]);
            assert!(!schema.field(0).is_nullable() && schema.field(5).is_nullable());
            let text = |column: usize, row: usize| {
                let column = arrow_array::cast::as_string_array(batch.column(column));
                (!column.is_null(row)).then(|| column.value(row).to_string())
            };
            let int = |column: usize, row: usize| batch.column(column).as_primitive::<Int64Type>().value(row);
            for row in 0..batch.num_rows() {
                let day = batch.column(1).as_primitive::<Date32Type>().value(row);
                rows.push((text(0, row).unwrap(), day, int(2, row), int(3, row), int(4, row), text(5, row)));
            }
        }
        rows
    }

    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("ss-out-direct-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn files_read_back_as_the_rows_and_as_the_arrow_writers() {
        let products = [(id::pack(b"p1"), b"shoes".as_slice()), (id::pack(b"p2"), b"books"), (id::pack(b"p3"), b"garden tools")];
        let ctx = Ctx::new(parse_f1(b"2026-10-05T00:00:00Z").unwrap(), parse_f1(b"2026-11-02T00:00:00Z").unwrap(), &products, &[]).unwrap();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // No rows; one row; one person; then many persons, with names of every length, a
        // slow-path name among them, runs of 1 to 40 days, and sizes around the widths' edges.
        for (case, persons) in [0usize, 1, 1, 2, 3, 255, 256, 257, 5000, 70_000].into_iter().enumerate() {
            let mut table = UserDaily::default();
            let mut want: Vec<Row> = Vec::new();
            for person in 0..persons {
                let name = match person % 9 {
                    0 => format!("u{}", next() % 36),
                    1 => "a person with a very long name, \u{e9}\u{4e16}".to_string() + &person.to_string(),
                    2 => format!("u{:08}", next() % 100_000_000),
                    _ => format!("u{}", next() % 1_000_000_000),
                };
                let days = if case == 1 { 1 } else { 1 + next() % if person % 50 == 0 { 40 } else { 4 } };
                for day in 0..days {
                    let category = match next() % 6 {
                        0 => NO_CATEGORY,
                        other => (other % ctx.categories.len() as u64) as u8,
                    };
                    let (sessions, events, revenue) = (1 + (next() % 9) as i64, 1 + (next() % 500) as i64, if category == NO_CATEGORY { 0 } else { (next() % 5_000_000_000) as i64 });
                    table.person.push(id::pack(name.as_bytes()));
                    table.day.push(ctx.day0 + day as i32);
                    table.sessions.push(sessions);
                    table.events.push(events);
                    table.revenue_cents.push(revenue);
                    table.top_category.push(category);
                    let top = (category != NO_CATEGORY).then(|| String::from_utf8(ctx.categories[category as usize].clone()).unwrap());
                    want.push((name.clone(), ctx.day0 + day as i32, sessions, events, revenue, top));
                }
            }
            let (direct, arrow) = (folder(&format!("direct-{case}")), folder(&format!("arrow-{case}")));
            let copy = UserDaily {
                person: table.person.clone(),
                day: table.day.clone(),
                sessions: table.sessions.clone(),
                events: table.events.clone(),
                revenue_cents: table.revenue_cents.clone(),
                top_category: table.top_category.clone(),
            };
            user_daily(&direct, 3, &ctx, table, None).unwrap();
            crate::arrow::user_daily(&arrow, 3, &ctx, copy).unwrap();
            let got = read(&direct.join("part-3.parquet"));
            assert_eq!(got.len(), want.len(), "case {case}");
            assert!(got == want, "case {case}: the rows read back differ");
            assert!(read(&arrow.join("part-3.parquet")) == want, "case {case}: the arrow writer's rows differ");
            std::fs::remove_dir_all(direct).unwrap();
            std::fs::remove_dir_all(arrow).unwrap();
        }
    }

    /// A narrow table of `persons` persons and the rows it stands for. `wide` is the share
    /// of rows (one in so many) with more events than the narrow column holds; `busy`
    /// whether a row has more sessions than its column holds; `buying` whether any row has
    /// a category.
    fn narrow(ctx: &Ctx, persons: usize, wide: u64, busy: bool, buying: bool, seed: u64) -> (UserDailyNarrow, Vec<Row>) {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut table, mut want) = (UserDailyNarrow::default(), Vec::new());
        for person in 0..persons {
            let name = match person % 9 {
                0 => format!("u{}", next() % 36),
                1 => "a person with a very long name, \u{e9}\u{4e16}".to_string() + &person.to_string(),
                _ => format!("u{}", next() % 1_000_000_000),
            };
            table.person.push(id::pack(name.as_bytes()));
            table.first.push(table.day.len() as u32);
            // A run of 100 rows now and then: a run's header of two bytes.
            let days = if person % 97 == 5 { 100 } else { 1 + next() % 4 };
            for day in 0..days {
                let row = table.day.len() as u32;
                let category = if buying && next() % 5 == 0 { (next() % ctx.categories.len() as u64) as u8 } else { NO_CATEGORY };
                let sessions = if busy && next() % 50 == 0 { 255 + (next() % 3) as u32 } else { 1 + (next() % 22) as u32 };
                let events = if next() % wide == 0 { EVENTS_HELD + (next() % 100_000) as u32 } else { 1 + (next() % (EVENTS_HELD as u64 - 1)) as u32 };
                let revenue = if category == NO_CATEGORY { 0 } else { (next() % 5_000_000_000) as i64 };
                table.day.push((day % 200) as u8);
                table.sessions.push(sessions.min(255) as u8);
                if sessions >= 255 {
                    table.wide_sessions.push((row, sessions));
                }
                table.events.push(events.min(0xFFFF) as u16);
                if events >= EVENTS_HELD {
                    table.wide_events.push((row, events));
                }
                table.revenue_cents.push(revenue);
                table.top_category.push(category);
                let top = (category != NO_CATEGORY).then(|| String::from_utf8(ctx.categories[category as usize].clone()).unwrap());
                want.push((name.clone(), ctx.day0 + (day % 200) as i32, i64::from(sessions), i64::from(events), revenue, top));
            }
        }
        (table, want)
    }

    #[test]
    fn narrow_files_read_back_as_the_rows() {
        let products = [(id::pack(b"p1"), b"shoes".as_slice()), (id::pack(b"p2"), b"books"), (id::pack(b"p3"), b"garden tools")];
        let ctx = Ctx::new(parse_f1(b"2026-10-05T00:00:00Z").unwrap(), parse_f1(b"2026-11-02T00:00:00Z").unwrap(), &products, &[]).unwrap();
        // Sizes around a group of eight and the index widths' edges; no wide row, a few,
        // and more than indices are left for; a row with too many sessions; no category.
        let cases: [(usize, u64, bool, bool); 14] = [
            (0, 1000, false, true),
            (1, 1000, false, true),
            (2, 1000, false, false),
            (3, 7, false, true),
            (5, 1000, true, true),
            (255, 1000, false, true),
            (256, 3, false, true),
            (257, 1000, false, true),
            (5000, 1000, false, true),
            (5000, 2, true, false),
            (70_000, 1000, false, true),
            (70_000, 50, false, true),
            (70_000, 2, false, true),
            (300, 1, true, true),
        ];
        for (case, (persons, wide, busy, buying)) in cases.into_iter().enumerate() {
            // As one file, and as the files of three stretches read one after another.
            for count in [1, 3] {
                let (mut table, want) = narrow(&ctx, persons, wide, busy, buying, 0x9E37_79B9_7F4A_7C15 ^ case as u64);
                let whole = table.widen(ctx.day0);
                let (direct, arrow) = (folder(&format!("narrow-{case}-{count}")), folder(&format!("narrow-arrow-{case}-{count}")));
                let mut got = Vec::new();
                for (part, piece) in table.pieces(count).into_iter().enumerate() {
                    user_daily_narrow(&direct, part, &ctx, piece, None).unwrap();
                    got.extend(read(&direct.join(format!("part-{part}.parquet"))));
                }
                crate::arrow::user_daily(&arrow, 5, &ctx, whole).unwrap();
                assert_eq!(got.len(), want.len(), "case {case} in {count}");
                assert!(got == want, "case {case} in {count}: the rows read back differ");
                assert!(read(&arrow.join("part-5.parquet")) == want, "case {case}: the rows made whole differ");
                std::fs::remove_dir_all(direct).unwrap();
                std::fs::remove_dir_all(arrow).unwrap();
            }
        }
    }

    #[test]
    fn narrow_names_come_from_the_caller() {
        let products = [(id::pack(b"p1"), b"shoes".as_slice())];
        let ctx = Ctx::new(parse_f1(b"2026-10-05T00:00:00Z").unwrap(), parse_f1(b"2026-11-02T00:00:00Z").unwrap(), &products, &[]).unwrap();
        let (mut table, want) = narrow(&ctx, 3000, 100, false, true, 77);
        // The persons as keys: the caller's names are the keys spelled out.
        let name = |key: Id| format!("person {key} of the caller's");
        for (run, person) in table.person.iter_mut().enumerate() {
            *person = run as Id * 7 + 1;
        }
        let mut run = 0;
        let want: Vec<Row> = want
            .into_iter()
            .enumerate()
            .map(|(row, (_, day, sessions, events, revenue, top))| {
                while run + 1 < table.first.len() && table.first[run + 1] as usize <= row {
                    run += 1;
                }
                (name(table.person[run]), day, sessions, events, revenue, top)
            })
            .collect();
        let folder = folder("narrow-named");
        let text_of = |keys: &[Id], out: &mut Vec<u8>| {
            for &key in keys {
                let text = name(key);
                out.extend_from_slice(&(text.len() as u32).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
        };
        user_daily_narrow(&folder, 0, &ctx, table.pieces(1).pop().unwrap(), Some(&text_of)).unwrap();
        assert!(read(&folder.join("part-0.parquet")) == want, "the rows read back differ");
        std::fs::remove_dir_all(folder).unwrap();
    }
}
