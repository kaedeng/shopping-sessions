//! Stage 1: the files become records. Reader, chunking, tokenizer, timestamps and the per-row
//! parse loop all live in this crate (decisions: Inputs first, CSV parser, Hot path, IDs,
//! Purchases, Clicks, Timestamps, SIMD).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's).

#[cfg(feature = "pipe-positional")]
pub mod pos;
mod tok;
mod ts;
#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::fs::File;
use std::path::{Path, PathBuf};

use memchr::{memchr, memrchr};
use types::hash::hash;
use types::id::{pack, pack_at};
use types::parts::{Combine, Mode, Parts, Plain};
use types::record::{ClickRec, EventRec, MAX_PRICE, PAY_CATEGORY_SHIFT, VALID};
use types::time::{DAY, SECOND, parse_f1};
use types::{Ctx, Id, env};

use tok::{Field, Scalar, Scratch, Tok};
use ts::{Dates, ZEROS, is8, parse8, rd};

#[cfg(not(feature = "reader-mmap"))]
compile_error!("ingest has one reader: enable `reader-mmap`");
#[cfg(not(feature = "ts-table"))]
compile_error!("ingest has one timestamp parser: enable `ts-table`");
#[cfg(not(any(feature = "tok-mask", feature = "tok-scalar")))]
compile_error!("ingest needs a tokenizer: enable `tok-mask` or `tok-scalar`");

/// Readable bytes after every segment: a 64-byte block load at its last byte, and a field's
/// widest read (27 bytes for a `ts`, 16 for `pack_at`) at its last field.
const PAD: usize = 64;

/// Reads `manifest.json`, `products.csv` and `campaigns.csv` under `data` into a `Ctx`.
pub fn load_ctx(data: &Path) -> Result<Ctx, String> {
    let read = |name: &str| {
        let path = data.join(name);
        std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
    };
    let manifest: serde_json::Value =
        serde_json::from_slice(&read("manifest.json")?).map_err(|error| format!("manifest.json: {error}"))?;
    let instant = |key: &str| {
        manifest[key]
            .as_str()
            .and_then(|text| parse_f1(text.as_bytes()))
            .ok_or_else(|| format!("manifest.json: {key} is not a YYYY-MM-DDTHH:MM:SSZ string"))
    };
    let (start, end) = (instant("window_start")?, instant("window_end")?);
    let (products, campaigns) = (rows_of(&read("products.csv")?), rows_of(&read("campaigns.csv")?));
    let products: Vec<(Id, &[u8])> = pairs(&products).into_iter().map(|(id, category)| (pack(id), category)).collect();
    Ctx::new(start, end, &products, &pairs(&campaigns))
}

/// A file's lines after the header, padded like a segment (see `padded`).
fn rows_of(file: &[u8]) -> Vec<u8> {
    padded(memchr(b'\n', file).map_or(&[][..], |header| &file[header + 1..]))
}

/// `\n`, the bytes, a `\n` if they lack a final one, then `PAD` zeros: segment `1..len - PAD`.
fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut own = Vec::with_capacity(bytes.len() + PAD + 2);
    own.push(b'\n');
    own.extend_from_slice(bytes);
    if bytes.last().is_some_and(|&last| last != b'\n') {
        own.push(b'\n');
    }
    own.resize(own.len() + PAD, 0);
    own
}

/// The `(key, value)` rows with a non-empty key of a padded two-column file.
fn pairs(buf: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut rows = Vec::new();
    let bytes = |field: Field| {
        let at = field.p as usize - buf.as_ptr() as usize;
        &buf[at..at + field.len]
    };
    // SAFETY: `padded` made the buffer a segment.
    unsafe {
        Scalar::lines::<2>(buf.as_ptr().add(1), buf.len() - 1 - PAD, &mut Scratch::new(), |&[key, value]| {
            if key.len > 0 {
                rows.push((bytes(key), bytes(value)));
            }
        })
    };
    rows
}

/// The whole inputs stage: `load_ctx` on the calling thread while another maps the files and
/// plans the chunks (`Ingest::open`'s other half), since neither needs the other until the end.
/// One after the other they took 6 and 8 ms at 20M events on the x86 box, with every worker idle.
pub fn open(data: &Path) -> Result<(Ctx, Ingest), String> {
    let chunk = chunk_bytes();
    std::thread::scope(|scope| {
        let files = scope.spawn(|| Files::map(data, chunk));
        let ctx = load_ctx(data);
        let files = files.join().expect("the mapping thread panicked");
        let ctx = ctx?;
        let ingest = Ingest::with_files(files?, &ctx)?;
        Ok((ctx, ingest))
    })
}

/// `SS_CHUNK_KB` in bytes.
fn chunk_bytes() -> usize {
    env::number("SS_CHUNK_KB", 1024).max(1) << 10
}

/// One worker's parse output. A worker owns one for the whole parse.
pub struct ParseOut {
    /// `1 << ctx.part_bits` dedup partitions: a record goes to
    /// list `(types::hash::hash(rec.id) >> (64 - ctx.part_bits)) as usize`, carved out of one
    /// allocation per worker (`types::parts`).
    /// Every well-formed event line with a non-empty `event_id` is here, valid or not.
    pub parts: Parts<EventRec>,
    /// Only valid clicks; `user` is the `user_id` as read.
    pub clicks: Vec<ClickRec>,
    scratch: Scratch,
}

impl ParseOut {
    /// Slot `free-early`: empties the partitions and the click list and hands their pages back
    /// to the kernel on the calling thread. For the worker to call once the dedup stage has
    /// read them.
    pub fn release(&mut self) {
        self.parts.release();
        let bytes = self.clicks.capacity() * size_of::<ClickRec>();
        self.clicks.clear();
        // SAFETY: the list is empty, so nothing reads its buffer before writing it again.
        unsafe { types::mem::release(self.clicks.as_ptr().cast(), bytes) };
    }
}

/// A file mapped, or a padded copy of a file's last lines.
enum Buf {
    Map(memmap2::Mmap),
    Own(Vec<u8>),
}

/// The lines of a buffer that begin in bytes `start..end`: a cut by bytes, so planning reads
/// (and faults in) none of the file. `Ingest::seg` finds the lines when the segment is parsed,
/// by the worker that then reads those pages anyway; planned line by line, the cuts cost one
/// page fault each on the main thread, 5 of the 14 ms the inputs stage took at 20M events on
/// the x86 box. Byte `start - 1` is readable, byte `limit - 1` is a `\n` no line of the segment
/// passes, and `PAD` bytes after `limit` are readable.
struct Seg {
    buf: usize,
    start: usize,
    end: usize,
    limit: usize,
}

type ParseFn = fn(&Ingest, &Ctx, usize, &mut ParseOut);
type LinksFn = fn(&Ingest) -> Vec<[Id; 2]>;

/// Every input file mapped and cut into segments: what `Ingest` holds that needs no `Ctx`.
struct Files {
    bufs: Vec<Buf>,
    events: Vec<Seg>,
    clicks: Vec<Seg>,
    links: Vec<Seg>,
}

impl Files {
    fn map(data: &Path, chunk: usize) -> Result<Files, String> {
        let (mut bufs, mut segs) = (Vec::new(), [Vec::new(), Vec::new(), Vec::new()]);
        for (folder, segs) in ["events", "clicks", "links"].into_iter().zip(&mut segs) {
            for path in csv_files(&data.join(folder))? {
                plan(&path, chunk, &mut bufs, segs)?;
            }
        }
        let [events, clicks, links] = segs;
        Ok(Files { bufs, events, clicks, links })
    }
}

/// The mapped input files, the chunk tasks and whatever the parse needs prepared (date table).
pub struct Ingest {
    bufs: Vec<Buf>,
    /// Event and click chunks, largest first; `true` for a click chunk.
    tasks: Vec<(Seg, bool)>,
    links: Vec<Seg>,
    dates: Dates,
    /// Estimated lines, to size the buffers.
    event_rows: usize,
    click_rows: usize,
    link_rows: usize,
    /// The tokenizer's monomorphized loops, picked once.
    parse_fn: ParseFn,
    links_fn: LinksFn,
    /// The positional parse with the same tokenizer (`pos`).
    #[cfg(feature = "pipe-positional")]
    pos_fn: pos::PosFn,
    /// The campaigns by their bytes, for the fused click parse (`pos`).
    #[cfg(feature = "pipe-positional")]
    campaigns: pos::Campaigns,
    /// How the partitions are written: `types::parts::mode_from_env` (slot `scatter-wc`).
    scatter: Option<usize>,
    /// Slot `reader-populate`, picked by `SS_READER=populate`.
    #[cfg(all(feature = "reader-populate", target_os = "linux"))]
    populate: bool,
}

impl Ingest {
    /// Maps every `*.csv` in `events/`, `clicks/` and `links/` and plans the chunk tasks.
    /// `crate::open` does this beside `load_ctx`.
    pub fn open(data: &Path, ctx: &Ctx) -> Result<Ingest, String> {
        Ingest::with_files(Files::map(data, chunk_bytes())?, ctx)
    }

    /// The tokenizer `SS_TOK` picks, and the reader `SS_READER` picks.
    fn with_files(files: Files, ctx: &Ctx) -> Result<Ingest, String> {
        let options: &[(&str, ParseFn, LinksFn)] = &[
            #[cfg(feature = "tok-mask")]
            ("mask", parse_with::<tok::Mask>, links_with::<tok::Mask>),
            #[cfg(feature = "tok-scalar")]
            ("scalar", parse_with::<Scalar>, links_with::<Scalar>),
        ];
        let names: Vec<&str> = options.iter().map(|option| option.0).collect();
        let picked = env::pick("SS_TOK", &names);
        let (_, parse_fn, links_fn) = options[picked];
        #[allow(unused_mut)]
        let mut ingest = Ingest::with(files, ctx, parse_fn, links_fn)?;
        #[cfg(feature = "pipe-positional")]
        {
            // The positional parse's own tokenizer (`SS_POS_TOK`): `shape` by default, while the
            // classic pipeline keeps `SS_TOK`'s, so comparing the two pipelines also compares
            // two tokenizers.
            let _ = picked;
            let options: &[(&str, pos::PosFn)] = &[
                #[cfg(feature = "tok-shape")]
                ("shape", pos::parse_with::<tok::Shape>),
                #[cfg(feature = "tok-mask")]
                ("mask", pos::parse_with::<tok::Mask>),
                #[cfg(feature = "tok-scalar")]
                ("scalar", pos::parse_with::<Scalar>),
            ];
            let names: Vec<&str> = options.iter().map(|option| option.0).collect();
            ingest.pos_fn = options[env::pick("SS_POS_TOK", &names)].1;
        }
        #[cfg(all(feature = "reader-populate", target_os = "linux"))]
        {
            ingest.populate = env::pick("SS_READER", &["mmap", "populate"]) == 1;
        }
        Ok(ingest)
    }

    #[cfg(test)]
    fn open_with(data: &Path, ctx: &Ctx, chunk: usize, parse_fn: ParseFn, links_fn: LinksFn) -> Result<Ingest, String> {
        Ingest::with(Files::map(data, chunk)?, ctx, parse_fn, links_fn)
    }

    fn with(files: Files, ctx: &Ctx, parse_fn: ParseFn, links_fn: LinksFn) -> Result<Ingest, String> {
        if !(1..=20).contains(&ctx.part_bits) {
            return Err(format!("SS_PART_BITS={}: 1 to 20 supported", ctx.part_bits));
        }
        let Files { bufs, events, clicks, links } = files;
        let rows = |segs: &[Seg]| {
            let bytes: usize = segs.iter().map(|seg| seg.end - seg.start).sum();
            segs.iter().max_by_key(|seg| seg.end - seg.start).map_or(0, |seg| {
                let sample = &buf_bytes(&bufs[seg.buf])[seg.start..seg.end.min(seg.start + (256 << 10))];
                bytes * memchr::memchr_iter(b'\n', sample).count() / sample.len()
            })
        };
        let (event_rows, click_rows, link_rows) = (rows(&events), rows(&clicks), rows(&links));
        let mut tasks: Vec<(Seg, bool)> =
            events.into_iter().map(|seg| (seg, false)).chain(clicks.into_iter().map(|seg| (seg, true))).collect();
        tasks.sort_by_key(|(seg, _)| Reverse(seg.end - seg.start));
        Ok(Ingest {
            bufs,
            tasks,
            links,
            dates: Dates::new(ctx),
            event_rows,
            click_rows,
            link_rows,
            parse_fn,
            links_fn,
            #[cfg(feature = "pipe-positional")]
            pos_fn: pos::parse_with::<Scalar>,
            #[cfg(feature = "pipe-positional")]
            campaigns: pos::Campaigns::new(ctx),
            scatter: types::parts::mode_from_env(),
            #[cfg(all(feature = "reader-populate", target_os = "linux"))]
            populate: false,
        })
    }

    /// Number of chunk tasks (event and click chunks), ordered largest-first.
    pub fn tasks(&self) -> usize {
        self.tasks.len()
    }

    /// A worker's buffers, pre-sized for its share of the input.
    pub fn new_out(&self, ctx: &Ctx) -> ParseOut {
        let parts = 1usize << ctx.part_bits;
        // A quarter to spare: workers pull tasks unevenly, and rows hash unevenly to partitions.
        let room = |rows: usize, ways: usize| rows / ctx.workers / ways * 5 / 4 + 16;
        ParseOut {
            parts: Parts::new(parts, room(self.event_rows, parts), self.scatter),
            clicks: Vec::with_capacity(room(self.click_rows, 1)),
            scratch: Scratch::new(),
        }
    }

    /// Parses chunk `task` (`0..tasks()`) into `out`. Called concurrently, one `out` per worker.
    pub fn parse(&self, ctx: &Ctx, task: usize, out: &mut ParseOut) {
        (self.parse_fn)(self, ctx, task, out)
    }

    /// Every well-formed line of every links file with both sides non-empty, as ID pairs.
    /// Serial: it runs on the one worker that builds the union-find.
    pub fn links(&self) -> Vec<[Id; 2]> {
        (self.links_fn)(self)
    }

    /// The whole lines of a segment: (first byte, length). A line belongs to the segment its
    /// first byte is in, so it starts after the first `\n` at or after byte `start - 1`, and the
    /// segment ends after the first `\n` at or after byte `end - 1`. Empty when one line covers
    /// the whole cut.
    fn seg(&self, seg: &Seg) -> (*const u8, usize) {
        let bytes = &buf_bytes(&self.bufs[seg.buf])[..seg.limit];
        let after = |at: usize| memchr(b'\n', &bytes[at - 1..]).map_or(seg.limit, |found| at + found);
        let (start, end) = (after(seg.start), after(seg.end));
        (bytes.as_ptr().wrapping_add(start), end.saturating_sub(start))
    }

    /// Slot `free-early`: a parsed segment's pages of the mapped file go back to the kernel on
    /// the worker that parsed it. Left mapped, the input is unmapped at exit by one thread, a
    /// page-table entry per 4 KB.
    fn release(&self, seg: &Seg) {
        if types::mem::free_early() && let Buf::Map(map) = &self.bufs[seg.buf] {
            // SAFETY: a read-only file mapping. A neighbour still reading here (its last line,
            // which begins before `start`, or `PAD` bytes past its end) faults the page back
            // in from the page cache.
            unsafe { types::mem::release(map.as_ptr().wrapping_add(seg.start), seg.end - seg.start) };
        }
    }
}

fn buf_bytes(buf: &Buf) -> &[u8] {
    match buf {
        Buf::Map(map) => map,
        Buf::Own(own) => own,
    }
}

/// Every file directly in `dir` named `*.csv`, as a glob sees it (no dotfiles), in name order.
fn csv_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let error = |error: std::io::Error| format!("{}: {error}", dir.display());
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(error)? {
        let path = entry.map_err(error)?.path();
        let name = path.file_name().map_or(&[][..], |name| name.as_encoded_bytes());
        if name.ends_with(b".csv") && !name.starts_with(b".") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Cuts one file's bytes after the header into segments of `chunk` bytes (`Seg`). The last
/// lines, from at least `PAD` bytes before the end, go to a padded copy, so no read leaves the
/// mapping.
fn plan(path: &Path, chunk: usize, bufs: &mut Vec<Buf>, segs: &mut Vec<Seg>) -> Result<(), String> {
    let error = |error: std::io::Error| format!("{}: {error}", path.display());
    let file = File::open(path).map_err(error)?;
    if file.metadata().map_err(error)?.len() == 0 {
        return Ok(());
    }
    // SAFETY: the inputs are read-only and nothing writes them during the run.
    let map = unsafe { memmap2::Mmap::map(&file) }.map_err(error)?;
    let Some(header) = memchr(b'\n', &map) else { return Ok(()) };
    let (data, len) = (header + 1, map.len());
    let tail = if len - data <= PAD { data } else { memrchr(b'\n', &map[data..len - PAD]).map_or(data, |at| data + at + 1) };
    if tail < len {
        let own = padded(&map[tail..]);
        let end = own.len() - PAD;
        segs.push(Seg { buf: bufs.len(), start: 1, end, limit: end });
        bufs.push(Buf::Own(own));
    }
    if data < tail {
        // `map[data - 1]` and `map[tail - 1]` are `\n`.
        let mut start = data;
        while start < tail {
            let end = start.saturating_add(chunk).min(tail);
            segs.push(Seg { buf: bufs.len(), start, end, limit: tail });
            start = end;
        }
        bufs.push(Buf::Map(map));
    }
    Ok(())
}

fn parse_with<T: Tok>(ingest: &Ingest, ctx: &Ctx, task: usize, out: &mut ParseOut) {
    let (seg, click) = &ingest.tasks[task];
    let (p, len) = ingest.seg(seg);
    #[cfg(all(feature = "reader-populate", target_os = "linux"))]
    if ingest.populate && let Buf::Map(map) = &ingest.bufs[seg.buf] {
        // One call faults the chunk in, instead of a fault per 64 KB while parsing. A kernel
        // before 5.14 answers EINVAL and the pages fault in as before.
        let _ = map.advise_range(memmap2::Advice::PopulateRead, seg.start, len);
    }
    let dates = &ingest.dates;
    // SAFETY: `plan` made every task a segment.
    unsafe {
        if *click {
            let clicks = &mut out.clicks;
            T::lines::<4>(p, len, &mut out.scratch, |fields| click_row(ctx, dates, fields, clicks));
        } else if out.parts.combining() {
            events::<T, Combine>(p, len, ctx, dates, out);
        } else {
            events::<T, Plain>(p, len, ctx, dates, out);
        }
    }
    ingest.release(seg);
}

/// One event chunk, its partitions written the way `M` says (`out.parts.combining()`).
///
/// # Safety
/// `p, len` is a segment.
#[inline(always)]
unsafe fn events<T: Tok, M: Mode>(p: *const u8, len: usize, ctx: &Ctx, dates: &Dates, out: &mut ParseOut) {
    let (parts, shift) = (&mut out.parts, 64 - ctx.part_bits);
    unsafe { T::lines::<7>(p, len, &mut out.scratch, |fields| event_row::<M>(ctx, dates, shift, fields, parts)) };
    // Streamed lines become visible to the dedup stage's threads.
    parts.fence();
}

fn links_with<T: Tok>(ingest: &Ingest) -> Vec<[Id; 2]> {
    let mut pairs = Vec::with_capacity(ingest.link_rows + 16);
    let mut scratch = Scratch::new();
    for seg in &ingest.links {
        let (p, len) = ingest.seg(seg);
        // SAFETY: `plan` made it a segment, and a segment's fields have 16 bytes after them.
        unsafe {
            T::lines::<2>(p, len, &mut scratch, |&[a, b]| {
                let (a, b) = (pack_at(a.p, a.len), pack_at(b.p, b.len));
                if (a != 0) & (b != 0) {
                    pairs.push([a, b]);
                }
            })
        };
        ingest.release(seg);
    }
    pairs
}

/// Rules "Events" steps 1 to 4: only a missing `event_id` drops the line; every other check
/// only leaves `VALID` unset, since dedup comes first.
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn event_row<M: Mode>(ctx: &Ctx, dates: &Dates, shift: u32, fields: &[Field; 7], parts: &mut Parts<EventRec>) {
    let [id, user, ts, ingest, kind, product, price] = *fields;
    let id = unsafe { pack_at(id.p, id.len) };
    if id == 0 {
        return;
    }
    let user = unsafe { pack_at(user.p, user.len) };
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
    let valid = (user != 0)
        & ts_ok
        & (ts >= i64::from(ctx.event_lo))
        & (ts < i64::from(ctx.ts_end))
        & known
        & (product.len != 0)
        & price_ok
        & ingest_ok
        & (ingest_ms - ts <= DAY);
    let rec = EventRec { id, user, ts: ts as u32, ingest: ingest_rel, pay, flags: VALID * u32::from(valid) };
    // SAFETY: `shift` leaves `part_bits` bits, and there are `1 << part_bits` partitions.
    unsafe { parts.push_as::<M>((hash(id) >> shift) as usize, rec) };
}

/// An `ingest_ts` outside the date table (or not F1, which the rules leave undefined: such an
/// event never survives). Returns (ok, ms after origin, `EventRec::ingest`).
#[cold]
#[inline(never)]
fn ingest_slow(ctx: &Ctx, field: Field) -> (bool, i64, u32) {
    // SAFETY: a field's bytes are in its segment.
    match parse_f1(unsafe { std::slice::from_raw_parts(field.p, field.len) }) {
        Some(ms) => (true, ms - ctx.origin, ctx.ingest_rel(ms.div_euclid(SECOND))),
        None => (false, 0, u32::MAX),
    }
}

/// A purchase's `price_cents`: digits only, leading zeros skipped, 1 to 10,000,000.
///
/// # Safety
/// 8 bytes readable at `p`.
#[inline(always)]
unsafe fn price_cents(p: *const u8, len: usize) -> (bool, u32) {
    if len.wrapping_sub(1) >= 8 {
        return price_slow(p, len);
    }
    // Right-align the digits in the word and fill the front with '0'.
    let shift = 8 * (8 - len) as u32;
    let word = unsafe { rd(p) } << shift | (ZEROS & ((1u64 << shift) - 1));
    let cents = parse8(word);
    (is8(word) & (1..=u64::from(MAX_PRICE)).contains(&cents), cents as u32)
}

/// Empty, or more than 8 bytes: never generated valid except with leading zeros.
#[cold]
#[inline(never)]
fn price_slow(p: *const u8, len: usize) -> (bool, u32) {
    // SAFETY: a field's bytes are in its segment.
    let text = unsafe { std::slice::from_raw_parts(p, len) };
    let digits = &text[text.iter().position(|&b| b != b'0').unwrap_or(len)..];
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) || digits.len() > 8 {
        return (false, 0);
    }
    let cents = digits.iter().fold(0, |v, &d| v * 10 + u32::from(d - b'0'));
    ((1..=MAX_PRICE).contains(&cents), cents)
}

/// Rules "Clicks" steps 1 and 3: only a fully valid click is kept (decision "Click dedup").
///
/// # Safety
/// The fields are from a segment.
#[inline(always)]
unsafe fn click_row(ctx: &Ctx, dates: &Dates, fields: &[Field; 4], clicks: &mut Vec<ClickRec>) {
    let [click, user, campaign, ts] = *fields;
    let (click, user) = unsafe { (pack_at(click.p, click.len), pack_at(user.p, user.len)) };
    let (ok, ts) = unsafe { dates.ts(ts.p, ts.len) };
    if (click != 0) & (user != 0) & ok & (ts >= 0) & (ts < i64::from(ctx.ts_end))
        && let Some(campaign) = ctx.campaign_index.get(unsafe { pack_at(campaign.p, campaign.len) })
    {
        clicks.push(ClickRec { user, click, ts: ts as u32, campaign });
    }
}
