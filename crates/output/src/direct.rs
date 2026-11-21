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
//! `campaign_daily` is a few thousand rows a part, all PLAIN: the two names with their lengths,
//! the day, the four counters as they lie. Through arrow-rs the table was a millisecond of a
//! run of twenty at 1M events, for 28,000 rows.
//!
//! The headers and the footer are Thrift's compact protocol (`Compact`), field numbers from
//! `parquet.thrift`. `tests` reads the files back with arrow-rs and compares them with the
//! rows and with `out-arrow`'s file; `scripts/check_parquet.py` does it with pyarrow, DuckDB
//! and Polars.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use types::Ctx;
use types::id::{self, Id, SLOW};
use types::rows::{CampaignDaily, NO_CATEGORY, UserDaily};

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
/// run of that entry's index for the rows that share it.
fn names(person: &[Id]) -> (Vec<u8>, Vec<u8>, usize) {
    // 13 bytes a name at most while packed; slow-path IDs grow the buffer.
    let mut dictionary: Vec<u8> = Vec::with_capacity(person.len() / 2 * 13 + 64);
    let mut runs: Vec<u32> = Vec::with_capacity(person.len() / 2 + 16);
    let mut text = Vec::new();
    let mut at = 0;
    while at < person.len() {
        let id = person[at];
        let mut end = at + 1;
        while end < person.len() && person[end] == id {
            end += 1;
        }
        if id & SLOW != 0 {
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

pub fn user_daily(folder: &Path, worker: usize, ctx: &Ctx, rows: UserDaily) -> Result<(), String> {
    let path = folder.join(format!("part-{worker}.parquet"));
    write(&path, ctx, &rows).map_err(|error| format!("{}: {error}", path.display()))
}

fn write(path: &Path, ctx: &Ctx, table: &UserDaily) -> Result<(), String> {
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
        let (dictionary, values, entries) = names(person);
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
            user_daily(&direct, 3, &ctx, table).unwrap();
            crate::arrow::user_daily(&arrow, 3, &ctx, copy).unwrap();
            let got = read(&direct.join("part-3.parquet"));
            assert_eq!(got.len(), want.len(), "case {case}");
            assert!(got == want, "case {case}: the rows read back differ");
            assert!(read(&arrow.join("part-3.parquet")) == want, "case {case}: the arrow writer's rows differ");
            std::fs::remove_dir_all(direct).unwrap();
            std::fs::remove_dir_all(arrow).unwrap();
        }
    }
}
