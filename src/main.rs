//! Shopping Sessions. Three data-parallel stages with two barriers (decision "Parallelism"):
//! parse (with the union-find beside it), dedup and person scatter, then sessions and output.
//!
//! Reads  $DATA_DIR   (default /data)
//! Writes $OUTPUT_DIR (default /output): user_daily/ and campaign_daily/ as Parquet
//! `SS_TIMING=1` prints each stage's wall time to stderr; implementation.md lists every knob.
//! Built with `--features trace`, `SS_TRACE=FILE` saves every task as a span (`types::par::trace`).

#[cfg(feature = "pipe-positional")]
mod opaque;
#[cfg(feature = "pipe-positional")]
mod positional;
mod probe;
#[cfg(feature = "alloc-region")]
mod region;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use ingest::ParseOut;
use sessions::Agg;
use types::par;
use types::parts::Parts;
use types::record::{ClickRec, PersonEvent};

// Slot `alloc-mimalloc`: a build-time swap, since a process has one global allocator.
#[cfg(feature = "alloc-mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Slot `alloc-region`, the pick: one reserved stretch of address space, no system call after
// the first allocation (`region`). Without either feature the allocator is the system's.
#[cfg(all(feature = "alloc-region", not(feature = "alloc-mimalloc")))]
#[global_allocator]
static ALLOCATOR: region::Region = region::Region;

/// Clicks per person-lookup task.
const CLICK_TASK: usize = 1 << 16;

/// Slot `alloc-parallel`: how each stage's per-worker buffers are built (`types::par::init`).
/// `SS_ALLOC=serial` builds them all on the calling thread, as before the slot existed.
const ALLOC: &[&str] = if cfg!(feature = "alloc-parallel") { &["parallel", "serial"] } else { &["serial"] };

/// One worker's person buckets, filled after dedup: one list per bucket, carved out of one
/// allocation per worker and record type (`types::parts`).
struct Scattered {
    events: Parts<PersonEvent>,
    clicks: Parts<ClickRec>,
    scratch: dedup::Scratch,
    /// The opaque pipeline's dedup buffers (`opaque::stages`).
    #[cfg(feature = "pipe-positional")]
    opaque: opaque::Scratch,
    /// The person buckets as ranges of the persons' numbers, when the opaque stages write
    /// them that way (`opaque::Staged::dense`).
    #[cfg(feature = "pipe-positional")]
    dense: Option<opaque::DenseBuckets>,
}

fn dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(|| default.into(), PathBuf::from)
}

/// Keeps glibc's own `malloc` to its first arena. This program allocates through `region`,
/// but the C library still calls `malloc` for itself, once in every thread's start
/// (`pthread_getattr_np`, for the stack's guard page): and for a thread's first `malloc` glibc
/// makes the thread an arena, 128 MB mapped, both ends unmapped, the rest unprotected, and the
/// first page of it, 2 MB with huge pages, zeroed. That was 0.3 ms a thread, the threads one
/// after another behind the address-space lock.
fn one_arena() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_ARENA_MAX: i32 = -8;
        // SAFETY: a setting of the C library's allocator, made before any thread is started.
        unsafe { mallopt(M_ARENA_MAX, 1) };
    }
}

/// Makes room in the process's descriptor table for the input files, and says whether there
/// is room. The table starts with 64 places and grows when a descriptor past its end is
/// used; in a process with threads each growth waits for every CPU to pass a quiet point (an
/// RCU grace period: tens of milliseconds when a few hundred files were opened while the
/// workers ran). One descriptor at 1,000 while the process is a single thread grows the
/// table once, at no such cost.
fn descriptors() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn dup2(from: i32, to: i32) -> i32;
            fn close(descriptor: i32) -> i32;
        }
        // SAFETY: descriptor 2 is duplicated onto one nothing uses this early, which is
        // closed at once; a failure (a limit below 1,001 descriptors) changes nothing.
        unsafe {
            if dup2(2, 1000) == 1000 {
                close(1000);
                return true;
            }
        }
    }
    false
}

/// Slot `free-early`, the last of it (`SS_EXIT=leave|release`, the pick first): with
/// `release`, every large block the run still holds goes back to the kernel now, a piece a
/// task on all the workers, and the process ends with next to nothing mapped. Call it
/// last: what the blocks held reads as zeros afterwards.
///
/// Left to the exit, one thread unmaps it all after the output is written and before the
/// process is gone, which whoever times the run waits for. On the x86 box (WSL2) with 4 KB
/// pages that was 51 ms at 20M events, and handing the blocks back first made the process
/// 8% faster. On the scorer the same build was 19 ms slower with it than without (0.6270
/// against 0.6076 s, one binary): unmapping at exit is cheap there, or eight threads
/// unmapping at once is dear. So the pick is to leave it to the exit.
fn hand_back(workers: usize) {
    #[cfg(feature = "alloc-region")]
    if types::mem::free_early() && types::env::pick("SS_EXIT", &["leave", "release"]) == 1 {
        if let Some((from, to)) = region::large_used() {
            let started = Instant::now();
            // A thread's chunk is 64 MB, mostly one worker's; smaller pieces so that the
            // workers come out even.
            const PIECE: usize = 8 << 20;
            let mut states = vec![(); workers.max(1)];
            // `SS_EXIT_WORKERS=N`: that many of them (all when unset). Eight threads handing
            // pages back at once share the kernel's locks on the page lists and the
            // container's counters, and each one's TLB flushes interrupt the other seven.
            par::limit(types::env::number("SS_EXIT_WORKERS", usize::MAX).max(1));
            par::run(&mut states, (to - from).div_ceil(PIECE), |_, _, piece| {
                let start = from + piece * PIECE;
                // SAFETY: whole pages of the reservation; nothing reads a large block after
                // this (the caller exits, and the workers' own state is not in one).
                unsafe { types::mem::release(start as *const u8, PIECE.min(to - start)) };
            });
            // After `total`: the process's wall time has it, the stages' sum does not.
            let took = started.elapsed();
            if std::env::var_os("SS_TIMING").is_some() {
                eprintln!("handback {:>8.1} ms", took.as_secs_f64() * 1e3);
            }
            if probe::stretched().as_deref() == Some("handback") {
                probe::wait(took);
            }
        }
    }
    #[cfg(not(feature = "alloc-region"))]
    let _ = workers;
}

/// `SS_REAP=on|off` (off by default): a thread of its own that hands dead stretches back to
/// the kernel (`MADV_DONTNEED`, 4 MB at a time) while the workers go on: the parse's lists
/// once the dedup stage is done with them (with `SS_RECYCLE=off`; else once the sessions
/// stage is, since their chunks hold the buckets' lists), the buckets' lists once the
/// sessions stage is.
///
/// The exit unmaps what a run still holds on one thread, after the output, and that thread's
/// time is in the run's (51 ms at 20M events on the x86 box with 4 KB pages). Handed back by
/// all eight workers first (`hand_back`) it was 8% faster there and 19 ms slower on the
/// scorer, where eight threads freeing pages at once share the kernel's locks and flush each
/// other's TLBs. One thread beside the output stage pays the exit's price per page, not
/// eight times it, and pays it before the exit. The exit stops it wherever it is: what it
/// handed back by then is what the exit does not unmap.
struct Reaper {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Bytes handed back so far.
    done: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    started: Instant,
    handle: std::thread::JoinHandle<()>,
    /// Where more stretches go (`give`).
    more: std::sync::mpsc::Sender<Vec<(usize, usize)>>,
    bytes: usize,
}

impl Reaper {
    const PIECE: usize = 4 << 20;

    /// A thread that waits for stretches (`give`) and hands each back as it comes.
    fn start() -> Reaper {
        use std::sync::atomic::Ordering::Relaxed;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (more, coming) = std::sync::mpsc::channel::<Vec<(usize, usize)>>();
        let (stopping, counting) = (stop.clone(), done.clone());
        let handle = std::thread::Builder::new()
            .name("reaper".into())
            .stack_size(64 << 10)
            .spawn(move || {
                for stretches in coming {
                    for (at, bytes) in stretches {
                        let mut from = at;
                        while from < at + bytes {
                            if stopping.load(Relaxed) {
                                return;
                            }
                            let piece = Self::PIECE.min(at + bytes - from);
                            // SAFETY: dead memory of the run (`give`'s contract).
                            unsafe { types::mem::release(from as *const u8, piece) };
                            counting.fetch_add(piece, Relaxed);
                            from += piece;
                        }
                    }
                }
            })
            .expect("the reaper's thread");
        Reaper { stop, done, started: Instant::now(), handle, more, bytes: 0 }
    }

    /// More stretches to hand back, after those given before.
    ///
    /// # Safety
    /// Nothing reads them again, and the allocator never gives them out again.
    unsafe fn give(&mut self, stretches: Vec<(usize, usize)>) {
        self.bytes += stretches.iter().map(|&(_, bytes)| bytes).sum::<usize>();
        drop(self.more.send(stretches));
    }

    /// Before the exit: under `SS_TIMING`, how far it got; with `join`, it is stopped and
    /// waited for (so that `hand_back` does not run beside it).
    fn finish(self, timing: bool, join: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if join {
            self.stop.store(true, Relaxed);
            drop(self.more);
            drop(self.handle.join());
        }
        if timing {
            eprintln!("reaper: {} of {} MB handed back by {:.1} ms into the run", self.done.load(Relaxed) >> 20, self.bytes >> 20, self.started.elapsed().as_secs_f64() * 1e3);
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("shopping-sessions: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let started = Instant::now();
    let timing = std::env::var_os("SS_TIMING").is_some();
    types::tl::start(std::env::var_os("SS_TIMING").is_some_and(|value| value == "2"));
    types::tl::mark("main");
    let mut last = started;
    // `SS_W_PARSE`, `SS_W_DEDUP`, `SS_W_SESSIONS`: the workers that take a stage's tasks, all
    // of them when unset (`par::limit`). Each is set when the stage before it ends.
    let stage_workers = [("inputs", "SS_W_PARSE"), ("parse", "SS_W_DEDUP"), ("dedup", "SS_W_SESSIONS"), ("sessions", "")].map(|(after, var)| (after, if var.is_empty() { usize::MAX } else { types::env::number(var, usize::MAX) }));
    // `SS_PROBE=stage:NAME`: the run waits ten times that stage's time when it ends
    // (`probe::stretched`).
    let stretched = probe::stretched();
    let mut lap = |stage: &'static str| {
        if let Some(&(_, taking)) = stage_workers.iter().find(|(after, _)| *after == stage) {
            par::limit(taking);
        }
        types::tl::mark(stage);
        probe::lapped();
        if timing || stretched.is_some() {
            let now = Instant::now();
            if timing {
                eprintln!("{stage:<9}{:>8.1} ms", (now - last).as_secs_f64() * 1e3);
            }
            if stretched.as_deref() == Some(stage) {
                probe::wait(now - last);
            }
            // After the wait: the next stage's time has none of it.
            last = Instant::now();
        }
    };

    let (data, out) = (dir("DATA_DIR", "/data"), dir("OUTPUT_DIR", "/output"));
    // `SS_PROBE`: a report on the machine after the run (`probe`).
    let probe = probe::Probe::start();
    one_arena();
    // Room in the descriptor table for every input file, made while the process is one
    // thread: the files then stay open for the run (`ingest::open`).
    let keep_open = descriptors();
    // The workers' threads (`types::par`) first, while the process has touched next to
    // nothing: a thread's start needs the address-space lock for writing, and waits behind
    // every page fault on fresh memory. The inputs stage is their first round.
    let workers = types::env::number("NCPU", 8).max(1);
    par::start(workers);
    // Slot `pipe-positional`: when the rows carry the numbering the generator had before its
    // IDs became a keyed bijection, the positional pipeline runs instead of the three stages
    // below. Today's IDs carry none, so the pick is `classic`, which never tries it; `auto`
    // tries it, and `positional` runs it whatever the sample says (exact either way; for
    // tests).
    //
    // The pick is `opaque` (`opaque::stages`): the same three stages with the parse and the
    // dedup made for IDs that are opaque strings of two widths.
    #[cfg(feature = "pipe-positional")]
    let (positional, opaque) = {
        const PIPES: &[&str] = &["opaque", "classic", "auto", "positional"];
        let pipe = PIPES[types::env::pick("SS_PIPE", PIPES)];
        ((pipe == "auto" || pipe == "positional").then_some(pipe == "positional"), pipe == "opaque")
    };
    #[cfg(not(feature = "pipe-positional"))]
    let (positional, opaque) = (None, false);
    let opened = ingest::open(&data, workers, positional, keep_open)?;
    let (ctx, ingest) = (opened.ctx, opened.ingest);
    let parallel_alloc = ALLOC[types::env::pick("SS_ALLOC", ALLOC)] == "parallel";
    // Slot `scatter-wc`: how the person buckets are written (the parse partitions: `Ingest`).
    let scatter = types::parts::bucket_mode_from_env();
    lap("inputs");

    #[cfg(feature = "pipe-positional")]
    {
        if let Some((plan, parsed)) = opened.pos {
            positional::run(&ctx, &ingest, &plan, parsed, &out, parallel_alloc, &mut lap)?;
            if timing {
                eprintln!("total    {:>8.1} ms", started.elapsed().as_secs_f64() * 1e3);
            }
            types::tl::report();
            if cfg!(feature = "fast-exit") {
                hand_back(workers);
                std::process::exit(0);
            }
            return Ok(());
        }
    }

    // Stage 1. Task 0 is the union-find, the only serial job; its worker then joins the parse.
    let buckets = 1usize << ctx.bucket_bits;
    let free_early = types::mem::free_early();
    let spilled = |count: usize, of: usize| format!("{count} of {of}");
    // The stages up to the sessions, once; twice when the sessions stage says that the
    // long `click_id`s, kept by their first 9 bytes, left a tie between two campaigns'
    // clicks open (`ingest::opaque::click_id_at`): then with those IDs whole.
    #[cfg(feature = "pipe-positional")]
    let mut whole_clicks = types::env::pick("SS_CLICK_IDS", &["first", "whole"]) == 1;
    // `SS_REAP=on`: one more thread hands dead lists back while the workers go on (`Reaper`):
    // the parse's lists as soon as nothing reads them (after the dedup stage where their
    // chunks were not recycled into the buckets' lists, else after the sessions stage), the
    // buckets' lists after the sessions stage.
    #[allow(unused_mut)]
    let mut reaper: Option<Reaper> = (free_early && opaque && types::env::pick("SS_REAP", &["off", "on"]) == 1).then(Reaper::start);
    #[allow(unused_mut)]
    let mut reapable: Vec<(usize, usize)> = Vec::new();
    let (mut scattered, mut aggs, names, spilled_partitions, spilled_events, spilled_clicks) = loop {
        // Stages 1 and 2: the person buckets, by the pipeline picked.
        #[cfg(feature = "pipe-positional")]
        let mut names = None;
        #[allow(unused_mut)]
        let mut dense_plan: Option<(u32, usize)> = None;
        #[cfg(feature = "pipe-positional")]
        let staged = opaque.then(|| {
            // A directory too small for the input's users (far more of them a line than
            // generated data has) is found out by the parse: once more with four times the room.
            let mut grow = 0;
            loop {
                if let Some(staged) = opaque::stages(&ctx, &ingest, grow, whole_clicks, parallel_alloc, scatter, &mut lap) {
                    break staged;
                }
                if timing {
                    eprintln!("opaque: the users' directory was too small, starting over");
                }
                grow += 2;
            }
        });
        #[cfg(not(feature = "pipe-positional"))]
        let staged: Option<(Vec<Scattered>, ())> = None;
        let (scattered, spilled_partitions) = if let Some(staged) = staged {
            #[cfg(feature = "pipe-positional")]
            {
                dense_plan = staged.dense;
                names = Some(staged.names);
                match &mut reaper {
                    // SAFETY: the parse's lists, forgotten and dead (`Staged::parsed`).
                    Some(reaper) if staged.parsed_dead => unsafe { reaper.give(staged.parsed) },
                    _ => reapable.extend(staged.parsed),
                }
                (staged.scattered, spilled(0, 0))
            }
            #[cfg(not(feature = "pipe-positional"))]
            (staged.0, spilled(0, 0))
        } else {
            let mut parsed: Vec<ParseOut> = par::init(ctx.workers, parallel_alloc, |_| ingest.new_out(&ctx));
            let link_table = OnceLock::new();
            par::run(&mut parsed, ingest.tasks() + 1, |_, out, task| match task {
                0 => drop(link_table.set(links::build(&ingest.links()))),
                _ => ingest.parse(&ctx, task - 1, out),
            });
            let link_table = link_table.get().expect("task 0 ran");
            lap("parse");

            // Stage 2. Each dedup partition, then its survivors' persons; clicks only need the person.
            let parts = 1usize << ctx.part_bits;
            let click_tasks: Vec<(usize, usize)> = parsed
                .iter()
                .enumerate()
                .flat_map(|(worker, out)| (0..out.clicks.len()).step_by(CLICK_TASK).map(move |start| (worker, start)))
                .collect();
            let events: usize = parsed.iter().map(|out| out.parts.len()).sum();
            let clicks: usize = parsed.iter().map(|out| out.clicks.len()).sum();
            let room = |rows: usize| rows / ctx.workers / buckets * 9 / 8 + 64;
            let mut scattered: Vec<Scattered> = par::init(ctx.workers, parallel_alloc, |_| Scattered {
                events: Parts::new(buckets, room(events), scatter),
                clicks: Parts::new(buckets, room(clicks), scatter),
                scratch: Default::default(),
                #[cfg(feature = "pipe-positional")]
                opaque: Default::default(),
                #[cfg(feature = "pipe-positional")]
                dense: None,
            });
            par::run(&mut scattered, parts + click_tasks.len(), |_, mine, task| {
                if task < parts {
                    let pieces = parsed.iter().flat_map(|out| out.parts.pieces(task));
                    let kept = dedup::partition(pieces, ctx.part_bits, &mut mine.scratch);
                    links::scatter_events(link_table, kept, ctx.bucket_bits, &mut mine.events);
                } else {
                    let (worker, start) = click_tasks[task - parts];
                    let clicks = &parsed[worker].clicks;
                    let end = clicks.len().min(start + CLICK_TASK);
                    links::scatter_clicks(link_table, &clicks[start..end], ctx.bucket_bits, &mut mine.clicks);
                }
            });
            lap("dedup");
            // Lists that outgrew their carved room and spilled into a `Vec` (`types::parts`), counted
            // before `free-early` empties them.
            let spilled_partitions = spilled(parsed.iter().map(|out| out.parts.spilled()).sum(), parts * ctx.workers);
            // Slot `free-early`: the dedup partitions and the parsed clicks are dead; each worker hands
            // its own back to the kernel, so their pages are not left for the one thread that exits.
            if free_early {
                par::each(&mut parsed, |_, out| out.release());
            }
            (scattered, spilled_partitions)
        };
        let spilled_events = spilled(scattered.iter().map(|mine| mine.events.spilled()).sum(), buckets * ctx.workers);
        let spilled_clicks = spilled(scattered.iter().map(|mine| mine.clicks.spilled()).sum(), buckets * ctx.workers);

        // Stage 3. Person buckets, largest first so a whale's bucket is not the last to start.
        let mut aggs: Vec<Agg> = par::init(ctx.workers, parallel_alloc, |_| Agg::new(&ctx));
        #[cfg(feature = "pipe-positional")]
        if let Some((shift, lists)) = dense_plan {
            // Buckets that are ranges of the persons' numbers: no hash, no lookup.
            fn dense(mine: &Scattered) -> &opaque::DenseBuckets {
                mine.dense.as_ref().expect("the opaque stages wrote ranges")
            }
            let mut order: Vec<usize> = (0..lists).collect();
            order.sort_by_cached_key(|&bucket| std::cmp::Reverse(scattered.iter().map(|mine| dense(mine).keys.len_of(bucket)).sum::<usize>()));
            par::run(&mut aggs, lists, |_, agg, task| {
                let bucket = order[task];
                sessions::bucket_dense(
                    &ctx,
                    ((bucket as u32) << shift) + 1,
                    1usize << shift,
                    &|each| scattered.iter().for_each(|mine| dense(mine).keys.for_each(bucket, |piece| each(piece))),
                    &|each| scattered.iter().for_each(|mine| dense(mine).pays.for_each(bucket, |piece| each(piece))),
                    &|each| scattered.iter().for_each(|mine| dense(mine).clicks.for_each(bucket, |piece| each(piece))),
                    agg,
                );
            });
        }
        if dense_plan.is_none() {
            let mut order: Vec<usize> = (0..buckets).collect();
            order.sort_by_cached_key(|&bucket| {
                std::cmp::Reverse(scattered.iter().map(|mine| mine.events.len_of(bucket)).sum::<usize>())
            });
            par::run(&mut aggs, buckets, |_, agg, task| {
                let bucket = order[task];
                let events = scattered.iter().flat_map(|mine| mine.events.pieces(bucket));
                let clicks = scattered.iter().flat_map(|mine| mine.clicks.pieces(bucket));
                sessions::bucket(&ctx, events, clicks, agg);
            });
        }
        lap("sessions");
        #[cfg(feature = "pipe-positional")]
        if opaque && !whole_clicks && sessions::ambiguous_tie() {
            if timing {
                eprintln!("opaque: a tie between two long click_ids of one beginning, starting over with them whole");
            }
            whole_clicks = true;
            // The exit path skips destructors (`fast-exit`); so does this one.
            std::mem::forget((scattered, aggs, names));
            continue;
        }
        #[cfg(not(feature = "pipe-positional"))]
        let names: Option<()> = None;
        break (scattered, aggs, names, spilled_partitions, spilled_events, spilled_clicks);
    };

    // `SS_REAP=on`: the buckets' lists are dead now, and the parse's that were held back.
    #[cfg(feature = "pipe-positional")]
    if let Some(reaper) = &mut reaper {
        for mine in &scattered {
            if let Some(dense) = &mine.dense {
                dense.used(|at, bytes| reapable.push((at, bytes)));
            }
        }
        // SAFETY: read by the sessions stage, which is done; `scattered` is never dropped
        // before the exit, and the parse's lists are forgotten.
        unsafe { reaper.give(std::mem::take(&mut reapable)) };
    }

    let (user_folder, campaign_folder) = (out.join("user_daily"), out.join("campaign_daily"));
    for folder in [&user_folder, &campaign_folder] {
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    }
    let failure = OnceLock::new();
    let mut outputs: Vec<(&mut Agg, &mut Scattered)> = aggs.iter_mut().zip(&mut scattered).collect();
    par::each(&mut outputs, |worker, (agg, mine)| {
        let rows = std::mem::take(&mut agg.user_daily);
        // The opaque pipeline's persons are places in its directory: the writer asks for
        // their names.
        #[cfg(feature = "pipe-positional")]
        let written = match &names {
            Some(names) => output::user_daily_named(&user_folder, worker, &ctx, rows, &|keys, out| names.texts(keys, out)),
            None => output::user_daily(&user_folder, worker, &ctx, rows),
        };
        #[cfg(not(feature = "pipe-positional"))]
        let written = output::user_daily(&user_folder, worker, &ctx, rows);
        if let Err(error) = written {
            drop(failure.set(error));
        }
        // Slot `free-early`: the person buckets are dead since the sessions stage.
        if free_early {
            mine.events.release();
            mine.clicks.release();
        }
    });
    drop(outputs);
    types::tl::mark("user_daily written");
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    output::campaign_daily(&campaign_folder, 0, &ctx, sessions::campaign_daily(&ctx, &aggs))?;
    lap("output");
    if timing {
        eprintln!("total    {:>8.1} ms", started.elapsed().as_secs_f64() * 1e3);
        eprintln!("spilled: {spilled_partitions} partitions, {spilled_events} event buckets, {spilled_clicks} click buckets");
    }
    types::tl::report();
    if let Some(probe) = &probe {
        probe.finish(&data);
    }
    #[cfg(feature = "trace")]
    if let Some(path) = std::env::var_os("SS_TRACE") {
        // One name per `par` scope, in order: each `par::init` and `par::each` is a stage of its own.
        let mut stages = vec!["parse init", "parse", "dedup init", "dedup", "sessions init", "sessions", "output"];
        if free_early {
            stages.insert(4, "free parse");
        }
        par::trace::write(path.as_ref(), &stages).map_err(|error| format!("SS_TRACE: {error}"))?;
    }
    // Slot `fast-exit`: the files are written, so skip the destructors and let the kernel
    // reclaim; what is large goes back first, on all the workers (`hand_back`).
    if cfg!(feature = "fast-exit") {
        if let Some(reaper) = reaper {
            reaper.finish(timing, types::env::pick("SS_EXIT", &["leave", "release"]) == 1);
        }
        hand_back(workers);
        std::process::exit(0);
    }
    drop(reaper);
    Ok(())
}
