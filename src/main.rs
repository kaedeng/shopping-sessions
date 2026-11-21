//! Shopping Sessions. Three data-parallel stages with two barriers (decision "Parallelism"):
//! parse (with the union-find beside it), dedup and person scatter, then sessions and output.
//!
//! Reads  $DATA_DIR   (default /data)
//! Writes $OUTPUT_DIR (default /output): user_daily/ and campaign_daily/ as Parquet
//! `SS_TIMING=1` prints each stage's wall time to stderr; implementation.md lists every knob.
//! Built with `--features trace`, `SS_TRACE=FILE` saves every task as a span (`types::par::trace`).

#[cfg(feature = "pipe-positional")]
mod positional;
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
    let mut lap = |stage: &'static str| {
        types::tl::mark(stage);
        if timing {
            let now = Instant::now();
            eprintln!("{stage:<9}{:>8.1} ms", (now - last).as_secs_f64() * 1e3);
            last = now;
        }
    };

    let (data, out) = (dir("DATA_DIR", "/data"), dir("OUTPUT_DIR", "/output"));
    one_arena();
    // Room in the descriptor table for every input file, made while the process is one
    // thread: the files then stay open for the run (`ingest::open`).
    let keep_open = descriptors();
    // The workers' threads (`types::par`) first, while the process has touched next to
    // nothing: a thread's start needs the address-space lock for writing, and waits behind
    // every page fault on fresh memory. The inputs stage is their first round.
    let workers = types::env::number("NCPU", 8).max(1);
    par::start(workers);
    // Slot `pipe-positional`: when the rows carry the generator's numbering, the positional
    // pipeline runs instead of the three stages below. `SS_PIPE=classic` never tries it, and
    // `positional` runs it whatever the sample says (exact either way; for tests).
    #[cfg(feature = "pipe-positional")]
    let positional = {
        const PIPES: &[&str] = &["auto", "classic", "positional"];
        let pipe = PIPES[types::env::pick("SS_PIPE", PIPES)];
        (pipe != "classic").then_some(pipe == "positional")
    };
    #[cfg(not(feature = "pipe-positional"))]
    let positional = None;
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
                std::process::exit(0);
            }
            return Ok(());
        }
    }

    // Stage 1. Task 0 is the union-find, the only serial job; its worker then joins the parse.
    let mut parsed: Vec<ParseOut> = par::init(ctx.workers, parallel_alloc, |_| ingest.new_out(&ctx));
    let link_table = OnceLock::new();
    par::run(&mut parsed, ingest.tasks() + 1, |_, out, task| match task {
        0 => drop(link_table.set(links::build(&ingest.links()))),
        _ => ingest.parse(&ctx, task - 1, out),
    });
    let link_table = link_table.get().expect("task 0 ran");
    lap("parse");

    // Stage 2. Each dedup partition, then its survivors' persons; clicks only need the person.
    let (parts, buckets) = (1usize << ctx.part_bits, 1usize << ctx.bucket_bits);
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
    let spilled = |count: usize, of: usize| format!("{count} of {of}");
    let spilled_partitions = spilled(parsed.iter().map(|out| out.parts.spilled()).sum(), parts * ctx.workers);
    let spilled_events = spilled(scattered.iter().map(|mine| mine.events.spilled()).sum(), buckets * ctx.workers);
    let spilled_clicks = spilled(scattered.iter().map(|mine| mine.clicks.spilled()).sum(), buckets * ctx.workers);
    // Slot `free-early`: the dedup partitions and the parsed clicks are dead; each worker hands
    // its own back to the kernel, so their pages are not left for the one thread that exits.
    let free_early = types::mem::free_early();
    if free_early {
        par::each(&mut parsed, |_, out| out.release());
    }

    // Stage 3. Person buckets, largest first so a whale's bucket is not the last to start.
    let mut order: Vec<usize> = (0..buckets).collect();
    order.sort_by_cached_key(|&bucket| {
        std::cmp::Reverse(scattered.iter().map(|mine| mine.events.len_of(bucket)).sum::<usize>())
    });
    let mut aggs: Vec<Agg> = par::init(ctx.workers, parallel_alloc, |_| Agg::new(&ctx));
    par::run(&mut aggs, buckets, |_, agg, task| {
        let bucket = order[task];
        let events = scattered.iter().flat_map(|mine| mine.events.pieces(bucket));
        let clicks = scattered.iter().flat_map(|mine| mine.clicks.pieces(bucket));
        sessions::bucket(&ctx, events, clicks, agg);
    });
    lap("sessions");

    let (user_folder, campaign_folder) = (out.join("user_daily"), out.join("campaign_daily"));
    for folder in [&user_folder, &campaign_folder] {
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    }
    let failure = OnceLock::new();
    let mut outputs: Vec<(&mut Agg, &mut Scattered)> = aggs.iter_mut().zip(&mut scattered).collect();
    par::each(&mut outputs, |worker, (agg, mine)| {
        let rows = std::mem::take(&mut agg.user_daily);
        if let Err(error) = output::user_daily(&user_folder, worker, &ctx, rows) {
            drop(failure.set(error));
        }
        // Slot `free-early`: the person buckets are dead since the sessions stage.
        if free_early {
            mine.events.release();
            mine.clicks.release();
        }
    });
    drop(outputs);
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    output::campaign_daily(&campaign_folder, 0, &ctx, sessions::campaign_daily(&ctx, &aggs))?;
    lap("output");
    if timing {
        eprintln!("total    {:>8.1} ms", started.elapsed().as_secs_f64() * 1e3);
        eprintln!("spilled: {spilled_partitions} partitions, {spilled_events} event buckets, {spilled_clicks} click buckets");
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
    // Slot `fast-exit`: the files are written, so skip the destructors and let the kernel reclaim.
    if cfg!(feature = "fast-exit") {
        std::process::exit(0);
    }
    Ok(())
}
