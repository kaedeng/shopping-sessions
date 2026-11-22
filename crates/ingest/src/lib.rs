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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use memchr::{memchr, memrchr};
use types::hash::hash;
use types::id::{pack, pack_at};
use types::table::Products;
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

/// Reads `manifest.json`, `products.csv` and `campaigns.csv` under `data` into a `Ctx`, on the
/// calling thread. The binary reads them beside the inputs' opening (`open`).
pub fn load_ctx(data: &Path) -> Result<Ctx, String> {
    let products = Lookup::read(data, "products.csv")?;
    let (categories, products) = products_of(&[&products.products(0, 1)])?;
    let window = window(data)?;
    Ctx::with_products(window.0, window.1, categories, products, &Lookup::read(data, "campaigns.csv")?.rows())
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

/// `window_start` and `window_end` of `manifest.json`, in epoch milliseconds.
fn window(data: &Path) -> Result<(i64, i64), String> {
    let path = data.join("manifest.json");
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| format!("manifest.json: {error}"))?;
    let instant = |key: &str| {
        manifest[key]
            .as_str()
            .and_then(|text| parse_f1(text.as_bytes()))
            .ok_or_else(|| format!("manifest.json: {key} is not a YYYY-MM-DDTHH:MM:SSZ string"))
    };
    Ok((instant("window_start")?, instant("window_end")?))
}

/// A lookup file read whole, once: its lines after the header are `bytes[from..to]`, the last
/// ending in `\n`, with the header's `\n` before them and `PAD` zeros after: a segment.
struct Lookup {
    bytes: Vec<u8>,
    from: usize,
    to: usize,
}

/// One piece of `products.csv`: its rows with a non-empty ID, in file order, each with its
/// category's index into `names` (the piece's categories in order of first appearance).
#[derive(Default)]
struct ProductRows {
    /// IDs that are a plain number (`Products::number_at`).
    numbers: Vec<(u32, u8)>,
    others: Vec<(Id, u8)>,
    names: Vec<Vec<u8>>,
}

impl Lookup {
    fn read(data: &Path, name: &str) -> Result<Lookup, String> {
        use std::io::Read;
        let path = data.join(name);
        let error = |error: std::io::Error| format!("{}: {error}", path.display());
        let mut file = File::open(&path).map_err(error)?;
        let len = file.metadata().map_err(error)?.len() as usize;
        // Room for the final `\n` a file may lack and for the zeros, so nothing is copied.
        let mut bytes = Vec::with_capacity(len + PAD + 2);
        file.read_to_end(&mut bytes).map_err(error)?;
        let from = memchr(b'\n', &bytes).map_or(bytes.len(), |header| header + 1);
        if from == bytes.len() {
            // No line after the header (or no header's end at all): a segment of no lines
            // still wants a readable byte before it.
            bytes.push(b'\n');
            let from = bytes.len();
            bytes.resize(from + PAD, 0);
            return Ok(Lookup { bytes, from, to: from });
        }
        if bytes.last() != Some(&b'\n') {
            bytes.push(b'\n');
        }
        let to = bytes.len();
        bytes.resize(to + PAD, 0);
        Ok(Lookup { bytes, from, to })
    }

    /// The lines that begin in piece `piece` of `pieces` equal cuts of the lines' bytes.
    fn piece(&self, piece: usize, pieces: usize) -> (usize, usize) {
        let len = self.to - self.from;
        // A cut falls after the first `\n` at or after the byte before it, as `Ingest::seg`'s.
        let after = |at: usize| if at == 0 { 0 } else { memchr(b'\n', &self.bytes[self.from + at - 1..self.to]).map_or(len, |found| at + found) };
        (self.from + after(len * piece / pieces), self.from + after(len * (piece + 1) / pieces))
    }

    /// The `(key, value)` rows with a non-empty key among the lines `from..to`, in order.
    fn pairs<'a>(&'a self, from: usize, to: usize, mut row: impl FnMut(Field, &'a [u8])) {
        let value_of = |field: Field| -> &'a [u8] {
            let at = field.p as usize - self.bytes.as_ptr() as usize;
            &self.bytes[at..at + field.len]
        };
        // SAFETY: whole lines of a segment (`Lookup`).
        unsafe {
            tok::Quick::lines::<2>(self.bytes.as_ptr().add(from), to - from, &mut Scratch::new(), |&[key, value]| {
                if key.len > 0 {
                    row(key, value_of(value));
                }
            })
        };
    }

    /// Every `(key, value)` row with a non-empty key.
    fn rows(&self) -> Vec<(&[u8], &[u8])> {
        let mut rows = Vec::new();
        self.pairs(self.from, self.to, |key, value| {
            let at = key.p as usize - self.bytes.as_ptr() as usize;
            rows.push((&self.bytes[at..at + key.len], value));
        });
        rows
    }

    /// Piece `piece` of `pieces` of `products.csv`.
    fn products(&self, piece: usize, pieces: usize) -> ProductRows {
        let (from, to) = self.piece(piece, pieces);
        let mut rows = ProductRows::default();
        // A line is 14 bytes or so.
        rows.numbers.reserve((to - from) / 12);
        // The name of the row before, most often not this row's: a scan of the few names.
        let mut last = usize::MAX;
        self.pairs(from, to, |key, category| {
            if rows.names.get(last).is_none_or(|name| name != category) {
                last = rows.names.iter().position(|name| name == category).unwrap_or_else(|| {
                    rows.names.push(category.to_vec());
                    rows.names.len() - 1
                });
            }
            // More names than a `u8` holds are too many anyway (`Ctx::categories`); the
            // count is checked when the pieces are put together.
            let category = last.min(255) as u8;
            // SAFETY: a field of a segment has 16 readable bytes.
            match unsafe { Products::number_at(key.p, key.len) } {
                Some(number) => rows.numbers.push((number, category)),
                None => rows.others.push((unsafe { pack_at(key.p, key.len) }, category)),
            }
        });
        rows
    }
}

/// The categories in byte order and the products' table (`Ctx::with_products`) from the
/// pieces of `products.csv` in file order.
fn products_of(pieces: &[&ProductRows]) -> Result<(Vec<Vec<u8>>, Products), String> {
    // The categories in order of first appearance, as one pass over the rows would list them.
    let mut names: Vec<&[u8]> = vec![b"unknown"];
    let mut global: Vec<Vec<usize>> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        if piece.names.len() > 255 {
            return Err("more than 255 categories, at most 255 supported".into());
        }
        global.push(
            piece
                .names
                .iter()
                .map(|name| {
                    names.iter().position(|known| known == name).unwrap_or_else(|| {
                        names.push(name);
                        names.len() - 1
                    })
                })
                .collect(),
        );
    }
    let (categories, rank) = Ctx::categories(&names)?;
    let numbers = pieces.iter().flat_map(|piece| &piece.numbers).map(|&(number, _)| number as usize + 1).max().unwrap_or(0);
    let mut products = Products::new(numbers, pieces.iter().map(|piece| piece.others.len()).sum());
    // In file order: the first row of an ID is the one kept.
    for (piece, global) in pieces.iter().zip(&global) {
        for &(number, category) in &piece.numbers {
            let category = rank[global[category as usize]];
            if !products.insert_number(number, category) {
                products.insert(pack(format!("p{number}").as_bytes()), category);
            }
        }
        for &(id, category) in &piece.others {
            products.insert(id, rank[global[category as usize]]);
        }
    }
    Ok((categories, products))
}

/// What `open` leaves: the lookups, the inputs, and when the positional pipeline is to run,
/// its plan and every worker's lists.
pub struct Opened {
    pub ctx: Ctx,
    pub ingest: Ingest,
    #[cfg(feature = "pipe-positional")]
    pub pos: Option<(pos::PosPlan, Vec<pos::PosOut>)>,
}

/// Pieces `products.csv` is read in.
const PRODUCT_PIECES: usize = 8;

/// A result a setup task leaves for the others. Every task sets its cells whatever happens,
/// so a thread that waits for one always gets it.
struct Cell<T>(OnceLock<T>);

impl<T> Cell<T> {
    fn new() -> Cell<T> {
        Cell(OnceLock::new())
    }

    fn set(&self, value: T) {
        let _ = self.0.set(value);
    }

    /// Waits, watching: these are set within a millisecond.
    fn wait(&self) -> &T {
        loop {
            if let Some(value) = self.0.get() {
                return value;
            }
            std::hint::spin_loop();
        }
    }

    fn take(self) -> T {
        self.0.into_inner().expect("its task ran")
    }
}

/// The whole inputs stage as one round of `workers` workers. Nothing in it waits for another
/// part longer than it must:
///
/// - one thread lists and opens the input files (`Files::open`), estimates the rows, and
///   finds the keys of the positional pipeline from a sample (`pos`);
/// - the others read the manifest and `campaigns.csv`, and `products.csv` in
///   `PRODUCT_PIECES` pieces;
/// - each worker builds its own lists for the positional parse as soon as the row estimate
///   says how large (`pos::out`).
///
/// One after another on the calling thread, with only the mapping of the files on a second
/// one, these were 6 ms of a 21 ms run at 1M events: 3.3 ms for the lookup files, then the
/// workers' threads, the keys and the lists, each with every other thread idle.
///
/// `positional`: whether to prepare the positional pipeline, and if so whether to run it
/// whatever the sample says (`force`: missing keys are zeros). `keep_open`: whether the input
/// files may stay open for the run (`Input`).
pub fn open(data: &Path, workers: usize, positional: Option<bool>, keep_open: bool) -> Result<Opened, String> {
    #[cfg(not(feature = "pipe-positional"))]
    let _ = positional;
    let chunk = chunk_bytes();
    let folders: [Cell<Result<Folder, String>>; 3] = [Cell::new(), Cell::new(), Cell::new()];
    let products: Cell<Result<Lookup, String>> = Cell::new();
    let pieces: Vec<Cell<Option<ProductRows>>> = (0..PRODUCT_PIECES).map(|_| Cell::new()).collect();
    // The `Ctx` without its products, and what the parse wants prepared from it; then the
    // products, which take longest to read.
    let base: Cell<Result<(Ctx, Prepared), String>> = Cell::new();
    let table: Cell<Result<(Vec<Vec<u8>>, Products), String>> = Cell::new();
    #[cfg(feature = "pipe-positional")]
    // The shape with the events' estimated lines, which it is from.
    let (shape, keys): (Cell<Option<(pos::PosShape, usize)>>, Cell<Option<(u64, u64)>>) = (Cell::new(), Cell::new());
    #[cfg(feature = "pipe-positional")]
    let scatter = types::parts::mode_from_env();
    #[cfg(feature = "pipe-positional")]
    let mut outs: Vec<Option<pos::PosOut>> = (0..workers).map(|_| None).collect();
    #[cfg(not(feature = "pipe-positional"))]
    let mut outs: Vec<()> = vec![(); workers];
    let next = AtomicUsize::new(0);
    types::par::each(&mut outs, |_worker, _out| {
        loop {
            match next.fetch_add(1, Relaxed) {
                // The folders by size, each a task; the events' first lines are also the
                // positional pipeline's sample, which wants more of them.
                0 => {
                    #[cfg(feature = "pipe-positional")]
                    let sample = if positional.is_some() { pos::SAMPLE_BYTES } else { ROWS_SAMPLE };
                    #[cfg(not(feature = "pipe-positional"))]
                    let sample = ROWS_SAMPLE;
                    let events = Folder::open(data, 0, chunk, keep_open, sample);
                    types::tl::mark("events open");
                    #[cfg(feature = "pipe-positional")]
                    match (&events, positional) {
                        (Ok(events), Some(force)) => {
                            shape.set(Some((pos::PosShape::of(events.rows), events.rows)));
                            keys.set(pos::keys(&pos::sample(&events.lines), events.rows, force));
                            types::tl::mark("keys");
                        }
                        _ => {
                            shape.set(None);
                            keys.set(None);
                        }
                    }
                    folders[0].set(events);
                }
                1 => {
                    products.set(Lookup::read(data, "products.csv"));
                    types::tl::mark("products read");
                }
                2 => {
                    folders[1].set(Folder::open(data, 1, chunk, keep_open, ROWS_SAMPLE));
                    types::tl::mark("clicks open");
                }
                3 => {
                    base.set(window(data).and_then(|window| {
                        let campaigns = Lookup::read(data, "campaigns.csv")?;
                        let ctx = Ctx::with_products(window.0, window.1, vec![b"unknown".to_vec()], Products::new(0, 0), &campaigns.rows())?;
                        let prepared = Prepared::new(&ctx);
                        Ok((ctx, prepared))
                    }));
                    types::tl::mark("campaigns");
                }
                4 => {
                    folders[2].set(Folder::open(data, 2, chunk, keep_open, ROWS_SAMPLE));
                    types::tl::mark("links open");
                }
                task if task < 5 + PRODUCT_PIECES => {
                    pieces[task - 5].set(products.wait().as_ref().ok().map(|lookup| lookup.products(task - 5, PRODUCT_PIECES)));
                    types::tl::mark("products piece");
                }
                task if task == 5 + PRODUCT_PIECES => {
                    // The products' pieces together, once all are read.
                    let rows: Option<Vec<&ProductRows>> = pieces.iter().map(|piece| piece.wait().as_ref()).collect();
                    table.set(match (products.wait(), rows) {
                        (Ok(_), Some(rows)) => products_of(&rows),
                        (Err(error), _) => Err(error.clone()),
                        (Ok(_), None) => unreachable!("products.csv was read, so its pieces are"),
                    });
                    types::tl::mark("products");
                }
                _ => break,
            }
        }
        // The worker's lists, as soon as the estimates say how large; then, while the round
        // still waits for the keys or the lookups, their pages (`PosOut::touch`).
        #[cfg(feature = "pipe-positional")]
        if let Some((shape, event_rows)) = shape.wait() {
            let (events, pays) = pos::event_lists(workers, shape, scatter);
            let click_rows = folders[1].wait().as_ref().map_or(0, |folder| folder.rows);
            let clicks = pos::click_lists(workers, shape, *event_rows, click_rows, scatter);
            let out = _out.insert(pos::PosOut::new(events, pays, clicks, chunk));
            types::tl::mark("lists");
            let click_lists = pos::lists_in_use(shape, *event_rows) * 5 / 4 + 1;
            out.touch(click_lists, &|| keys.0.get().is_some() && base.0.get().is_some() && table.0.get().is_some() && folders[2].0.get().is_some());
            types::tl::mark("touched");
        }
    });
    types::tl::mark("setup round");
    let [events, clicks, links] = folders.map(Cell::take);
    let files = Files::join([events?, clicks?, links?]);
    let (mut ctx, prepared) = base.take()?;
    let (categories, products) = table.take()?;
    ctx.set_products(categories, products)?;
    let ingest = Ingest::with_files(files, &ctx, prepared)?;
    #[cfg(feature = "pipe-positional")]
    let pos = match (shape.take(), keys.take()) {
        (Some((shape, _)), Some(keys)) => Some((ingest.pos_plan(shape, keys), outs.into_iter().map(|out| out.expect("every worker built its lists")).collect())),
        _ => None,
    };
    types::tl::mark("ingest");
    Ok(Opened {
        ctx,
        ingest,
        #[cfg(feature = "pipe-positional")]
        pos,
    })
}

/// What the parse wants prepared from the `Ctx`: made beside the inputs' opening.
struct Prepared {
    dates: Dates,
    #[cfg(feature = "pipe-positional")]
    campaigns: pos::Campaigns,
}

impl Prepared {
    fn new(ctx: &Ctx) -> Prepared {
        Prepared {
            dates: Dates::new(ctx),
            #[cfg(feature = "pipe-positional")]
            campaigns: pos::Campaigns::new(ctx),
        }
    }
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

/// An input file, or a padded copy of a file's last lines.
enum Buf {
    File(Input),
    Own(Vec<u8>),
}

/// An input file. It is read with `pread` (`Ingest::read`, `Input::read_at`) and mapped only
/// if something asks for its bytes as a slice (`buf_bytes`: the classic parse, a line too
/// long for a read): mapping every file up front was a `mmap` and two page faults a file
/// before the parse, on the address-space lock the workers' first page faults need.
struct Input {
    path: PathBuf,
    /// Kept open for the run when the files are few (`KEEP_OPEN`): a read is then one system
    /// call, and no worker opens a file while others parse.
    file: Option<File>,
    map: OnceLock<memmap2::Mmap>,
}

impl Input {
    /// The file mapped, on first use.
    fn map(&self) -> &memmap2::Mmap {
        self.map.get_or_init(|| {
            let mapped = File::open(&self.path).and_then(|file| {
                // SAFETY: the inputs are read-only and nothing writes them during the run.
                unsafe { memmap2::Mmap::map(&file) }
            });
            mapped.unwrap_or_else(|error| panic!("{}: {error}", self.path.display()))
        })
    }

    /// Bytes `at..at + buf.len()` of the file, which holds them.
    fn read_at(&self, buf: &mut [u8], at: usize) -> std::io::Result<()> {
        match &self.file {
            Some(file) => read_at(file, buf, at),
            None => read_at(&File::open(&self.path)?, buf, at),
        }
    }
}

/// Bytes `at..at + buf.len()` of `file`, which holds them.
fn read_at(file: &File, buf: &mut [u8], at: usize) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, at as u64)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            match std::os::windows::fs::FileExt::seek_read(file, &mut buf[done..], (at + done) as u64)? {
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                read => done += read,
            }
        }
        Ok(())
    }
}

/// Bytes `Ingest::read` takes past a segment's cut for its last line; a longer line sends the
/// segment to the mapping.
#[cfg(feature = "pipe-positional")]
const READ_SLACK: usize = 4096;

/// Bytes `Ingest::read` leaves readable before the chunk it reads, at least: a kernel may
/// read a few bytes before a line (`pos`).
#[cfg(feature = "pipe-positional")]
pub(crate) const READ_LEAD: usize = 64;

/// Room a read buffer needs beyond the bytes read and `PAD`: the lead, and up to a page more
/// to put the bytes at the file's own offset within a page (`Ingest::read`).
#[cfg(feature = "pipe-positional")]
pub(crate) const READ_ROOM: usize = READ_LEAD + 4096;

/// The lines of a buffer that begin in bytes `start..end`: a cut by bytes, so planning reads
/// none of the file but its ends. `Ingest::seg` finds the lines when the segment is parsed, by
/// the worker that then reads those bytes anyway. Byte `start - 1` is readable, byte
/// `limit - 1` is a `\n` no line of the segment passes, and `PAD` bytes after `limit` are
/// readable.
struct Seg {
    buf: usize,
    start: usize,
    end: usize,
    limit: usize,
}

type ParseFn = fn(&Ingest, &Ctx, usize, &mut ParseOut);
type LinksFn = fn(&Ingest) -> Vec<[Id; 2]>;

/// Every input file opened and cut into segments: what `Ingest` holds that needs no `Ctx`.
struct Files {
    bufs: Vec<Buf>,
    events: Vec<Seg>,
    clicks: Vec<Seg>,
    links: Vec<Seg>,
    /// Estimated lines of the events, the clicks and the links, to size the buffers.
    rows: [usize; 3],
}

/// One of the three input folders opened and cut into segments.
struct Folder {
    bufs: Vec<Buf>,
    /// Their `buf` counts within this folder's `bufs`.
    segs: Vec<Seg>,
    /// Estimated lines.
    rows: usize,
    /// The first lines of the largest segment (`head`), which the estimate is from.
    lines: Vec<u8>,
}

/// Bytes of a folder's largest segment read to estimate its lines.
const ROWS_SAMPLE: usize = 64 << 10;

/// Input files of each folder kept open at most (`Input::file`): together under the usual
/// limit of 1,024 descriptors.
const KEEP_OPEN: [usize; 3] = [512, 192, 64];
const FOLDERS: [&str; 3] = ["events", "clicks", "links"];

impl Folder {
    /// Folder `which` of `FOLDERS` under `data`. `sample`: the bytes to estimate its lines from.
    fn open(data: &Path, which: usize, chunk: usize, keep_open: bool, sample: usize) -> Result<Folder, String> {
        let (mut bufs, mut segs) = (Vec::new(), Vec::new());
        for path in csv_files(&data.join(FOLDERS[which]))? {
            let keep = keep_open && bufs.len() < KEEP_OPEN[which];
            plan(path, chunk, keep, &mut bufs, &mut segs)?;
        }
        let lines = head(&bufs, &segs, sample);
        let bytes: usize = segs.iter().map(|seg| seg.end - seg.start).sum();
        let sampled = &lines[1..lines.len() - PAD];
        let rows = if sampled.is_empty() { 0 } else { bytes * memchr::memchr_iter(b'\n', sampled).count() / sampled.len() };
        Ok(Folder { bufs, segs, rows, lines })
    }
}

/// The first whole lines in at most `most` bytes of the largest of `segs`, padded like
/// `padded`: a `\n`, the lines, `PAD` zeros. No lines for no segments or a failed read.
fn head(bufs: &[Buf], segs: &[Seg], most: usize) -> Vec<u8> {
    let Some(seg) = segs.iter().max_by_key(|seg| seg.end - seg.start) else { return padded(&[]) };
    // From the byte before the segment, as `Ingest::seg` looks for the first line.
    let (from, to) = (seg.start - 1, seg.limit.min(seg.start + most));
    let mut bytes = vec![0u8; to - from];
    match &bufs[seg.buf] {
        Buf::Own(own) => bytes.copy_from_slice(&own[from..to]),
        Buf::File(input) => {
            if input.read_at(&mut bytes, from).is_err() {
                return padded(&[]);
            }
        }
    }
    match (memchr(b'\n', &bytes), memrchr(b'\n', &bytes)) {
        (Some(first), Some(last)) if first < last => padded(&bytes[first + 1..=last]),
        _ => padded(&[]),
    }
}

impl Files {
    /// The three folders, one after another.
    fn open(data: &Path, chunk: usize, keep_open: bool) -> Result<Files, String> {
        let folder = |which| Folder::open(data, which, chunk, keep_open, ROWS_SAMPLE);
        Ok(Files::join([folder(0)?, folder(1)?, folder(2)?]))
    }

    /// The folders of `FOLDERS`, in that order, as one.
    fn join(folders: [Folder; 3]) -> Files {
        let rows = [folders[0].rows, folders[1].rows, folders[2].rows];
        let (mut bufs, mut kinds) = (Vec::new(), Vec::new());
        for folder in folders {
            let first = bufs.len();
            bufs.extend(folder.bufs);
            kinds.push(folder.segs.into_iter().map(|seg| Seg { buf: first + seg.buf, ..seg }).collect::<Vec<Seg>>());
        }
        let links = kinds.pop().expect("three folders");
        let clicks = kinds.pop().expect("three folders");
        let events = kinds.pop().expect("three folders");
        Files { bufs, events, clicks, links, rows }
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
    /// The positional parse reads its chunks with `pread` (`read`); `SS_POS_READER`.
    #[cfg(feature = "pipe-positional")]
    pread: bool,
    /// Where `read` puts a chunk within a page, past the file's own offset; `SS_READ_ALIGN`.
    #[cfg(feature = "pipe-positional")]
    read_align: usize,
    /// How the partitions are written: `types::parts::mode_from_env` (slot `scatter-wc`).
    scatter: Option<usize>,
    /// Slot `reader-populate`, picked by `SS_READER=populate`.
    #[cfg(all(feature = "reader-populate", target_os = "linux"))]
    populate: bool,
}

impl Ingest {
    /// Opens every `*.csv` in `events/`, `clicks/` and `links/` and plans the chunk tasks.
    /// `crate::open` does this beside the lookups.
    pub fn open(data: &Path, ctx: &Ctx) -> Result<Ingest, String> {
        Ingest::with_files(Files::open(data, chunk_bytes(), false)?, ctx, Prepared::new(ctx))
    }

    /// The tokenizer `SS_TOK` picks, and the reader `SS_READER` picks.
    fn with_files(files: Files, ctx: &Ctx, prepared: Prepared) -> Result<Ingest, String> {
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
        let mut ingest = Ingest::with(files, ctx, prepared, parse_fn, links_fn)?;
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
        Ingest::with(Files::open(data, chunk, false)?, ctx, Prepared::new(ctx), parse_fn, links_fn)
    }

    fn with(files: Files, ctx: &Ctx, prepared: Prepared, parse_fn: ParseFn, links_fn: LinksFn) -> Result<Ingest, String> {
        if !(1..=20).contains(&ctx.part_bits) {
            return Err(format!("SS_PART_BITS={}: 1 to 20 supported", ctx.part_bits));
        }
        let Files { bufs, events, clicks, links, rows: [event_rows, click_rows, link_rows], .. } = files;
        let mut tasks: Vec<(Seg, bool)> =
            events.into_iter().map(|seg| (seg, false)).chain(clicks.into_iter().map(|seg| (seg, true))).collect();
        tasks.sort_by_key(|(seg, _)| Reverse(seg.end - seg.start));
        Ok(Ingest {
            bufs,
            tasks,
            links,
            dates: prepared.dates,
            event_rows,
            click_rows,
            link_rows,
            parse_fn,
            links_fn,
            #[cfg(feature = "pipe-positional")]
            pos_fn: pos::parse_with::<Scalar>,
            #[cfg(feature = "pipe-positional")]
            campaigns: prepared.campaigns,
            #[cfg(feature = "pipe-positional")]
            pread: env::pick("SS_POS_READER", &["pread", "mmap"]) == 0,
            #[cfg(feature = "pipe-positional")]
            read_align: env::number("SS_READ_ALIGN", 0),
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

    /// The whole lines of a segment as `seg` gives them, but read with one `pread` into `buf`,
    /// a worker's own: the mapping's pages are then never touched. Reading through the
    /// mapping costs a fault per 64 KB, and handing a chunk's pages back (`release`) takes the
    /// kernel a TLB flush on every other worker's CPU: with 8 workers at 20M, the parse took
    /// 179 ms with the release and 159 without, and without it the process then spent 60 ms
    /// unmapping at exit (x86 box). A copy has neither cost, and leaves the bytes in cache.
    /// `None` for a segment that is not a mapped file's, a read that fails, or a last line
    /// that passes `READ_SLACK`: the caller takes the mapping (`seg`, then `release`).
    #[cfg(feature = "pipe-positional")]
    fn read(&self, seg: &Seg, buf: &mut Vec<u8>, open: &mut Option<(usize, File)>) -> Option<(*const u8, usize)> {
        let Buf::File(input) = &self.bufs[seg.buf] else { return None };
        if !self.pread {
            return None;
        }
        let file = match &input.file {
            Some(file) => file,
            None => {
                // The files are too many to keep open (`KEEP_OPEN`), or the process's
                // descriptor table had no room made for them: growing it while the workers
                // run waits out an RCU grace period each time (40 ms each for a few hundred
                // files at 20M on the x86 box). A worker then keeps one file open, the one it
                // read last: chunks come file by file, so most reads find theirs.
                if open.as_ref().is_none_or(|&(buf, _)| buf != seg.buf) {
                    *open = Some((seg.buf, File::open(&input.path).ok()?));
                }
                &open.as_ref()?.1
            }
        };
        // From the byte before the segment's first to a little past its cut.
        let (from, to) = (seg.start - 1, (seg.end + READ_SLACK).min(seg.limit));
        let bytes = to - from;
        // The bytes past the read are an earlier read's or zeros: readable, like a
        // mapping's bytes past a segment.
        if buf.len() < READ_ROOM + bytes + PAD {
            buf.resize(READ_ROOM + bytes + PAD, 0);
        }
        // The bytes go where their offset within a page is the file's (plus `SS_READ_ALIGN`,
        // for measurements): the kernel copies page by page, and how fast depends on how the
        // two line up. With the chunk 64 bytes into a page-aligned buffer the copy took twice
        // as long as with it 0 or 1 byte in (x86 box, 20M: 870 against 450 ms of system time).
        let align = self.read_align.wrapping_add(from).wrapping_sub(buf.as_ptr() as usize) % 4096;
        let lead = if align < READ_LEAD { align + 4096 } else { align };
        let buf = &mut buf[lead..];
        read_at(file, &mut buf[..bytes], from).ok()?;
        // As `seg`: after the first `\n` at or after byte `start - 1`, to after the first
        // at or after byte `end - 1`.
        let start = memchr(b'\n', &buf[..bytes])? + 1;
        let cut = seg.end - seg.start;
        let end = cut + memchr(b'\n', &buf[cut..bytes])? + 1;
        Some((buf.as_ptr().wrapping_add(start), end.saturating_sub(start)))
    }

    /// Slot `free-early`: a parsed segment's pages of the mapped file go back to the kernel on
    /// the worker that parsed it. Left mapped, the input is unmapped at exit by one thread, a
    /// page-table entry per 4 KB.
    fn release(&self, seg: &Seg) {
        if types::mem::free_early() && let Buf::File(input) = &self.bufs[seg.buf] && let Some(map) = input.map.get() {
            // SAFETY: a read-only file mapping. A neighbour still reading here (its last line,
            // which begins before `start`, or `PAD` bytes past its end) faults the page back
            // in from the page cache.
            unsafe { types::mem::release(map.as_ptr().wrapping_add(seg.start), seg.end - seg.start) };
        }
    }
}

fn buf_bytes(buf: &Buf) -> &[u8] {
    match buf {
        Buf::File(input) => input.map(),
        Buf::Own(own) => own,
    }
}

/// Every file directly in `dir` named `*.csv`, as a glob sees it (no dotfiles), in name order.
fn csv_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let error = |error: std::io::Error| format!("{}: {error}", dir.display());
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(error)? {
        let entry = entry.map_err(error)?;
        let path = entry.path();
        let name = path.file_name().map_or(&[][..], |name| name.as_encoded_bytes());
        // The entry's own type where the folder gives it (no system call); a link is followed.
        if name.ends_with(b".csv") && !name.starts_with(b".") && (entry.file_type().is_ok_and(|kind| kind.is_file()) || path.is_file()) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Bytes read from each end of a file to find its header and its last lines.
const ENDS: usize = 4096;

/// Cuts one file's bytes after the header into segments of `chunk` bytes (`Seg`). The last
/// lines, from at least `PAD` bytes before the end, go to a padded copy, so no read of a
/// segment leaves the file. Only the file's two ends are read: the header's `\n` and the last
/// lines. `keep`: whether the file stays open for the run.
fn plan(path: PathBuf, chunk: usize, keep: bool, bufs: &mut Vec<Buf>, segs: &mut Vec<Seg>) -> Result<(), String> {
    let error = |error: std::io::Error| format!("{}: {error}", path.display());
    let file = File::open(&path).map_err(error)?;
    let len = file.metadata().map_err(error)?.len() as usize;
    if len == 0 {
        return Ok(());
    }
    // The header ends at the first `\n`: in the first read, or the file is read on for it.
    let mut window = vec![0u8; len.min(ENDS)];
    read_at(&file, &mut window, 0).map_err(error)?;
    let (mut header, mut scanned) = (memchr(b'\n', &window), window.len());
    while header.is_none() && scanned < len {
        let mut more = vec![0u8; (len - scanned).min(64 << 10)];
        read_at(&file, &mut more, scanned).map_err(error)?;
        header = memchr(b'\n', &more).map(|at| scanned + at);
        scanned += more.len();
    }
    let Some(header) = header else { return Ok(()) };
    let data = header + 1;
    // `window` holds the file's bytes `from..len`: the whole of a short file, else nothing yet.
    let mut from = if len <= ENDS { 0 } else { len };
    // The last lines begin after the last `\n` before the final `PAD` bytes: looked for in a
    // window of the file's end that grows until it holds one, or reaches the first line.
    let tail = if len - data <= PAD {
        data
    } else {
        let mut size = ENDS;
        loop {
            let start = len.saturating_sub(size).max(data);
            if start < from {
                window = vec![0u8; len - start];
                read_at(&file, &mut window, start).map_err(error)?;
                from = start;
            }
            let first = data.max(from);
            match memrchr(b'\n', &window[first - from..len - PAD - from]) {
                Some(at) => break first + at + 1,
                None if from <= data => break data,
                None => size *= 8,
            }
        }
    };
    if tail < len {
        if from > tail {
            window = vec![0u8; len - tail];
            read_at(&file, &mut window, tail).map_err(error)?;
            from = tail;
        }
        let own = padded(&window[tail - from..]);
        let end = own.len() - PAD;
        segs.push(Seg { buf: bufs.len(), start: 1, end, limit: end });
        bufs.push(Buf::Own(own));
    }
    if data < tail {
        // Bytes `data - 1` and `tail - 1` of the file are `\n`.
        let mut start = data;
        while start < tail {
            let end = start.saturating_add(chunk).min(tail);
            segs.push(Seg { buf: bufs.len(), start, end, limit: tail });
            start = end;
        }
        bufs.push(Buf::File(Input { path, file: keep.then_some(file), map: OnceLock::new() }));
    }
    Ok(())
}

fn parse_with<T: Tok>(ingest: &Ingest, ctx: &Ctx, task: usize, out: &mut ParseOut) {
    let (seg, click) = &ingest.tasks[task];
    let (p, len) = ingest.seg(seg);
    #[cfg(all(feature = "reader-populate", target_os = "linux"))]
    if ingest.populate && let Buf::File(input) = &ingest.bufs[seg.buf] {
        // One call faults the chunk in, instead of a fault per 64 KB while parsing. A kernel
        // before 5.14 answers EINVAL and the pages fault in as before.
        let _ = input.map().advise_range(memmap2::Advice::PopulateRead, seg.start, len);
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
