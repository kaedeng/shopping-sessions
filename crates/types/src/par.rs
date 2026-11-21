//! Slot `par-counter`: one thread per worker state, tasks handed out by a shared counter in
//! index order, so the caller orders them largest-first. Returning is the stage's barrier.
//!
//! The threads are kept for the whole run (`pool`, `SS_PAR=pool|spawn`): worker 0 is the
//! calling thread and the others wait between stages. Spawned per stage instead, each new
//! thread cost the caller an `mmap`, an `mprotect` and a `clone`, which take the address-space
//! lock for writing, while the workers already started were taking page faults on fresh
//! memory, which hold it for reading through the zeroing of a 2 MB page: the later workers of
//! a stage started 2 to 5 ms after the first (measured in the click stage at 20M with two
//! copies running), and a dozen stages a run each paid it.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

/// The workers kept between stages.
///
/// A round is published through atomics alone and the workers watch for it, spinning, for
/// `SS_SPIN_US` microseconds (300) before they sleep: stages follow each other within that,
/// so a round mostly starts on workers that are awake. (Behind a mutex and a condition
/// variable, every round woke seven sleepers that then queued for the mutex: 60 to 90 µs
/// from the first worker's start to the last's, a dozen times in a run of 20 ms at 1M events,
/// and a millisecond for the first round.)
mod pool {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::thread::Thread;
    use std::time::{Duration, Instant};

    /// A round's job, called with the worker's number.
    type Job<'a> = dyn Fn(usize) + Sync + 'a;

    /// Workers the pool holds at most; a round of more spawns its threads.
    const MOST: usize = 256;

    /// A worker thread's place: its handle, to wake it, and whether it sleeps.
    struct Seat {
        thread: OnceLock<Thread>,
        asleep: AtomicBool,
    }

    struct Pool {
        /// Counts the rounds, two a round: odd while the caller writes the round's `job`,
        /// `count` and `left`, even once they stand. A worker takes part once in each round
        /// that includes it.
        round: AtomicU64,
        /// The round's job as its two pointer words, lifetime erased: `round` does not return
        /// before `left` is 0.
        job: [AtomicUsize; 2],
        /// Workers `1..count` take part in the round.
        count: AtomicUsize,
        /// Of them, those that have not finished.
        left: AtomicUsize,
        panicked: AtomicBool,
        /// One round at a time. A caller that finds the pool taken (a job that itself asks
        /// for workers, or another thread's) gets `false` and spawns threads.
        taken: AtomicBool,
        /// Threads so far, started or about to be: workers `1..=threads`.
        threads: Mutex<usize>,
        /// Of them, those that run.
        up: AtomicUsize,
        seats: Vec<Seat>,
        /// The thread waiting for the round's last worker, when it sleeps.
        caller: Mutex<Option<Thread>>,
        caller_asleep: AtomicBool,
        /// How long a thread watches before it sleeps.
        spin: Duration,
    }

    fn pool() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        POOL.get_or_init(|| Pool {
            round: AtomicU64::new(0),
            job: [AtomicUsize::new(0), AtomicUsize::new(0)],
            count: AtomicUsize::new(0),
            left: AtomicUsize::new(0),
            panicked: AtomicBool::new(false),
            taken: AtomicBool::new(false),
            threads: Mutex::new(0),
            up: AtomicUsize::new(0),
            seats: (0..MOST).map(|_| Seat { thread: OnceLock::new(), asleep: AtomicBool::new(false) }).collect(),
            caller: Mutex::new(None),
            caller_asleep: AtomicBool::new(false),
            spin: Duration::from_micros(crate::env::number("SS_SPIN_US", 300) as u64),
        })
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        // Nothing panics while holding these.
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Calls `done` until it returns `Some`: watching for `pool.spin`, then asleep between
    /// looks. Before it sleeps it calls `settle` and sets `asleep`; whoever makes `done` come
    /// true must then look at that flag and wake the thread if it is set (both sides `SeqCst`,
    /// so one of the two sees the other).
    fn until<T>(pool: &Pool, asleep: &AtomicBool, settle: impl FnOnce(), done: impl Fn() -> Option<T>) -> T {
        let (mut looks, mut since) = (0u32, None::<Instant>);
        loop {
            if let Some(value) = done() {
                return value;
            }
            std::hint::spin_loop();
            looks += 1;
            // The clock every 256 looks: reading it is 20 ns.
            if looks & 255 == 0 && since.get_or_insert_with(Instant::now).elapsed() >= pool.spin {
                break;
            }
        }
        settle();
        asleep.store(true, SeqCst);
        let value = loop {
            if let Some(value) = done() {
                break value;
            }
            // Returns at once after an `unpark` since the last `park`, and now and then for
            // no reason: the loop looks again.
            std::thread::park();
        };
        asleep.store(false, SeqCst);
        value
    }

    /// Worker `index`'s thread. With `tree`, it first starts workers `2 * index` and
    /// `2 * index + 1` where those are below `tree`.
    fn work(pool: &'static Pool, index: usize, tree: usize) {
        let seat = &pool.seats[index];
        let _ = seat.thread.set(std::thread::current());
        for child in [2 * index, 2 * index + 1] {
            if child < tree {
                spawn(pool, child, tree);
            }
        }
        pool.up.fetch_add(1, SeqCst);
        crate::tl::mark("thread up");
        // The last round this worker looked at. A thread starts between rounds or during
        // one: a finished round did not include it (it would not have finished), and a
        // running one that does is its first.
        let mut seen = 0u64;
        loop {
            let round = until(pool, &seat.asleep, || (), || Some(pool.round.load(SeqCst)).filter(|&round| round != seen && round & 1 == 0));
            let (count, words) = (pool.count.load(SeqCst), [pool.job[0].load(SeqCst), pool.job[1].load(SeqCst)]);
            // Read while the next round was being written (so this one did not include this
            // worker): look again.
            if pool.round.load(SeqCst) != round {
                continue;
            }
            seen = round;
            if index >= count {
                continue;
            }
            // SAFETY: the words of this round's job, written before `round` took this value;
            // the caller keeps the job alive until `left` is 0, which waits for this worker.
            let job: &Job<'static> = unsafe { std::mem::transmute::<[usize; 2], &Job<'static>>(words) };
            if catch_unwind(AssertUnwindSafe(|| job(index))).is_err() {
                pool.panicked.store(true, SeqCst);
            }
            if pool.left.fetch_sub(1, SeqCst) == 1 && pool.caller_asleep.load(SeqCst) {
                if let Some(caller) = lock(&pool.caller).as_ref() {
                    caller.unpark();
                }
            }
        }
    }

    fn spawn(pool: &'static Pool, index: usize, tree: usize) {
        std::thread::Builder::new().name(format!("worker-{index}")).spawn(move || work(pool, index, tree)).expect("a worker thread");
    }

    /// Makes sure workers `1..workers` exist or are on their way. The first threads are
    /// started as a tree (the caller starts worker 1, a worker the two below it), so the
    /// caller is back after one `clone` and the rest start side by side: started one after
    /// another by the caller, seven threads took it 0.9 ms.
    fn grow(pool: &'static Pool, workers: usize) {
        let mut threads = lock(&pool.threads);
        if *threads == 0 && workers > 1 {
            *threads = workers - 1;
            spawn(pool, 1, workers);
        }
        while *threads + 1 < workers {
            *threads += 1;
            spawn(pool, *threads, 0);
        }
    }

    /// Starts the threads for `workers` workers now, so that no stage has to, and returns
    /// when they all run. Waiting is the point: a thread's start takes the address-space lock
    /// for writing (its stack is a `mmap` and an `mprotect`), and behind a page fault on fresh
    /// memory, which holds that lock while 2 MB are zeroed, it waits. With the first stage
    /// already running, each of seven threads took 0.35 ms to appear, one after another;
    /// with nothing else running, all of them are up in a fraction of that.
    pub fn start(workers: usize) {
        let pool = pool();
        let workers = workers.min(MOST);
        grow(pool, workers);
        // (Going on at once, with the first stage's tasks taken by the threads as they
        // appear, measured the same within the noise: 40 runs each in alternation at 1M.)
        while pool.up.load(SeqCst) + 1 < workers {
            std::hint::spin_loop();
        }
    }

    /// Runs `job(w)` for every `w` in `0..workers`, `job(0)` on the calling thread, and
    /// returns `true` once all have returned; or returns `false` at once, nothing run, when
    /// the pool is in use (or `workers` is more than it holds). Panics if a job panicked.
    pub fn round(workers: usize, job: &Job<'_>) -> bool {
        let pool = pool();
        if workers > MOST || pool.taken.swap(true, SeqCst) {
            return false;
        }
        grow(pool, workers);
        // SAFETY: a reference to a trait object is two words; they are read back as the same
        // type (`work`), and no worker is inside the job once `left` is 0 (`Wait`).
        let words = unsafe { std::mem::transmute::<&Job<'_>, [usize; 2]>(job) };
        pool.round.fetch_add(1, SeqCst);
        pool.job[0].store(words[0], SeqCst);
        pool.job[1].store(words[1], SeqCst);
        pool.count.store(workers, SeqCst);
        pool.left.store(workers - 1, SeqCst);
        pool.panicked.store(false, SeqCst);
        pool.round.fetch_add(1, SeqCst);
        for seat in &pool.seats[1..workers] {
            if seat.asleep.load(SeqCst) && let Some(thread) = seat.thread.get() {
                thread.unpark();
            }
        }
        /// Waits for the round's workers, also when the caller's own job unwinds, and gives
        /// the pool back.
        struct Wait(&'static Pool);
        impl Wait {
            /// Whether a worker's job panicked.
            fn finish(&self) -> bool {
                let pool = self.0;
                until(pool, &pool.caller_asleep, || *lock(&pool.caller) = Some(std::thread::current()), || (pool.left.load(SeqCst) == 0).then_some(()));
                let panicked = pool.panicked.load(SeqCst);
                pool.taken.store(false, SeqCst);
                panicked
            }
        }
        impl Drop for Wait {
            fn drop(&mut self) {
                self.finish();
            }
        }
        let wait = Wait(pool);
        job(0);
        let panicked = wait.finish();
        std::mem::forget(wait);
        assert!(!panicked, "worker panicked");
        true
    }
}

/// Whether the workers are kept between stages (`SS_PAR=pool`, the pick) or spawned for each
/// (`spawn`); read once.
fn pooled() -> bool {
    static PICK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PICK.get_or_init(|| crate::env::pick("SS_PAR", &["pool", "spawn"]) == 0)
}

/// Starts the threads of `workers` workers ahead of the first stage, when they are kept: a
/// thread started while others already work waits for their page faults (see the top). Call
/// it after the mappings the caller itself still has to make.
pub fn start(workers: usize) {
    if pooled() {
        pool::start(workers);
    }
}

/// A pointer to the worker states that the workers' threads may share: each takes its own.
struct States<S>(*mut S);
// SAFETY: worker `w` alone touches state `w`, and a state may move to its worker (`S: Send`).
unsafe impl<S: Send> Sync for States<S> {}

impl<S> States<S> {
    /// # Safety
    /// `worker` is below the states' count, no other reference to that state is alive, and
    /// the states outlive the returned one.
    #[allow(clippy::mut_from_ref)]
    unsafe fn of(&self, worker: usize) -> &mut S {
        unsafe { &mut *self.0.add(worker) }
    }
}

/// Runs `task(worker, state, index)` for every index in `0..tasks`, each worker on its own state.
pub fn run<S: Send>(states: &mut [S], tasks: usize, task: impl Fn(usize, &mut S, usize) + Sync) {
    let next = AtomicUsize::new(0);
    let stage = trace::next_stage();
    each(states, |worker, state| {
        crate::tl::mark("worker in");
        loop {
            let index = next.fetch_add(1, Relaxed);
            if index >= tasks {
                break;
            }
            let start = trace::now();
            task(worker, state, index);
            trace::span(stage, worker, index, start);
        }
        crate::tl::mark("worker out");
    });
}

/// Builds one state per worker, worker `w`'s by `make(w)`: when `parallel`, each on its own
/// thread (worker 0's on the calling thread), else all on the calling thread, one after another.
/// Building a state allocates its buffers, and glibc writes a header into every chunk, so the
/// builder takes the page faults: with 2 MB pages the kernel zeroes almost every reserved page
/// right there. Built in parallel, that zeroing is spread over the workers instead of stalling
/// them all behind one thread (slot `alloc-parallel` in `alternatives.md`).
/// Either way it is one trace stage, so `SS_TRACE` shows how long the build took.
pub fn init<S: Send>(workers: usize, parallel: bool, make: impl Fn(usize) -> S + Sync) -> Vec<S> {
    assert!(workers > 0, "at least one worker");
    let stage = trace::claim_stage();
    if !parallel {
        let start = trace::now();
        let states = (0..workers).map(&make).collect();
        trace::span(stage, 0, trace::WHOLE, start);
        return states;
    }
    let make = |worker| {
        let start = trace::now();
        crate::tl::mark("init in");
        let state = make(worker);
        crate::tl::mark("init out");
        trace::span(stage, worker, trace::WHOLE, start);
        state
    };
    if pooled() && workers > 1 {
        let mut slots: Vec<Option<S>> = (0..workers).map(|_| None).collect();
        let states = States(slots.as_mut_ptr());
        // SAFETY: a round gives each worker number below `workers` to one thread, and
        // returns after all of them have.
        if pool::round(workers, &|worker| *unsafe { states.of(worker) } = Some(make(worker))) {
            return slots.into_iter().map(|slot| slot.expect("every worker built its state")).collect();
        }
    }
    thread::scope(|scope| {
        let rest: Vec<_> = (1..workers)
            .map(|worker| {
                let make = &make;
                scope.spawn(move || make(worker))
            })
            .collect();
        let first = make(0);
        std::iter::once(first).chain(rest.into_iter().map(|handle| handle.join().expect("worker panicked"))).collect()
    })
}

/// Runs `job(worker, state)` once per worker, the first on the calling thread.
pub fn each<S: Send>(states: &mut [S], job: impl Fn(usize, &mut S) + Sync) {
    let (first, rest) = states.split_first_mut().expect("at least one worker");
    let stage = trace::claim_stage();
    let job = |worker, state: &mut S| {
        let start = trace::now();
        job(worker, state);
        trace::span(stage, worker, trace::WHOLE, start);
    };
    if pooled() && !rest.is_empty() {
        let workers = rest.len() + 1;
        // Worker 0's state is `first`; the others' are `rest`, by this pointer.
        let (states, first_at) = (States(rest.as_mut_ptr()), States(std::ptr::from_mut(first)));
        // SAFETY: a round gives each worker number below `workers` to one thread and returns
        // after all of them have, so each state is borrowed once, within this call.
        let one = |worker: usize| job(worker, unsafe { if worker == 0 { first_at.of(0) } else { states.of(worker - 1) } });
        if pool::round(workers, &one) {
            return;
        }
    }
    thread::scope(|scope| {
        for (worker, state) in rest.iter_mut().enumerate() {
            let job = &job;
            scope.spawn(move || job(worker + 1, state));
        }
        job(0, first);
    });
}

/// Feature `trace`: every `each` job and `run` task is a span, saved by `write` as Chrome trace
/// JSON for ui.perfetto.dev, one track per worker. Without it these are no-ops.
#[cfg(feature = "trace")]
pub mod trace {
    use std::fmt::Write;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::{LazyLock, Mutex};
    use std::time::Instant;

    /// The task of a span that covers a worker's whole job.
    pub const WHOLE: usize = usize::MAX;
    /// (stage, worker, task, start ns, end ns)
    type Span = (usize, usize, usize, u64, u64);
    static SPANS: Mutex<Vec<Span>> = Mutex::new(Vec::new());
    static STAGE: AtomicUsize = AtomicUsize::new(0);
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

    /// Stage numbers count `each` calls, so the stage of a `run` is the one its `each` takes next.
    pub(super) fn next_stage() -> usize {
        STAGE.load(Relaxed)
    }

    pub(super) fn claim_stage() -> usize {
        STAGE.fetch_add(1, Relaxed)
    }

    pub(super) fn now() -> u64 {
        EPOCH.elapsed().as_nanos() as u64
    }

    pub(super) fn span(stage: usize, worker: usize, task: usize, start: u64) {
        let end = now();
        SPANS.lock().unwrap().push((stage, worker, task, start, end));
    }

    /// Writes every span so far; `stages` names the `each` and `run` calls in order.
    pub fn write(path: &std::path::Path, stages: &[&str]) -> std::io::Result<()> {
        let mut json = String::from("{\"traceEvents\":[\n");
        for (i, &(stage, worker, task, start, end)) in SPANS.lock().unwrap().iter().enumerate() {
            let stage = stages.get(stage).copied().unwrap_or("stage");
            let (name, task) = if task == WHOLE { (stage.to_owned(), -1) } else { (format!("{stage} {task}"), task as i64) };
            let comma = if i == 0 { "" } else { ",\n" };
            let (ts, dur) = (start as f64 / 1e3, (end - start) as f64 / 1e3);
            write!(json, "{comma}{{\"name\":\"{name}\",\"cat\":\"{stage}\",\"ph\":\"X\",\"pid\":1,\"tid\":{worker},\"ts\":{ts:.3},\"dur\":{dur:.3},\"args\":{{\"task\":{task}}}}}").unwrap();
        }
        json.push_str("\n]}\n");
        std::fs::write(path, json)
    }
}

#[cfg(not(feature = "trace"))]
mod trace {
    pub const WHOLE: usize = 0;
    pub fn next_stage() -> usize {
        0
    }
    pub fn claim_stage() -> usize {
        0
    }
    pub fn now() -> u64 {
        0
    }
    pub fn span(_stage: usize, _worker: usize, _task: usize, _start: u64) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_task_runs_once() {
        let mut sums = vec![0usize; 4];
        super::run(&mut sums, 1000, |_, sum, index| *sum += index);
        assert_eq!(sums.iter().sum::<usize>(), 999 * 1000 / 2);
    }

    #[test]
    fn each_gives_every_worker_its_own_state() {
        // Many short rounds, of changing sizes: a worker must take part once in each round
        // that includes it and in no other.
        for round in 0..2000usize {
            let workers = 1 + round % 9;
            let mut states = vec![0usize; workers];
            super::each(&mut states, |worker, state| *state += worker + 1);
            assert!(states.iter().enumerate().all(|(worker, &state)| state == worker + 1), "round {round}: {states:?}");
        }
    }

    #[test]
    fn a_job_may_ask_for_workers() {
        // The pool is taken by the outer round, so the inner ones spawn their threads.
        let mut outer = vec![0usize; 3];
        super::each(&mut outer, |_, sum| {
            let mut inner = vec![0usize; 4];
            super::run(&mut inner, 100, |_, sum, index| *sum += index);
            *sum = inner.iter().sum();
        });
        assert_eq!(outer, vec![99 * 100 / 2; 3]);
    }

    #[test]
    fn a_workers_panic_reaches_the_caller() {
        for _ in 0..3 {
            let caught = std::panic::catch_unwind(|| {
                let mut states = vec![0usize; 4];
                super::each(&mut states, |worker, _| assert!(worker != 2, "worker 2 gives up"));
            });
            assert!(caught.is_err());
            // And the workers are there for the next round.
            let mut states = vec![0usize; 4];
            super::each(&mut states, |worker, state| *state = worker);
            assert_eq!(states, vec![0, 1, 2, 3]);
        }
    }

    #[test]
    fn init_builds_states_in_worker_order() {
        for parallel in [false, true] {
            for workers in [1, 2, 7] {
                let states = super::init(workers, parallel, |worker| vec![worker; 1 << 12]);
                assert_eq!(states.len(), workers);
                assert!(states.iter().enumerate().all(|(worker, state)| state.iter().all(|&w| w == worker)));
            }
        }
    }

    #[cfg(feature = "trace")]
    #[test]
    fn trace_writes_every_span() {
        let mut states = vec![(); 3];
        super::run(&mut states, 5, |_, _, _| {});
        let path = std::env::temp_dir().join(format!("par-trace-{}.json", std::process::id()));
        super::trace::write(&path, &["parse"]).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // Tests share the statics, so count only what this one adds: 5 tasks and 3 whole jobs.
        assert!((0..5).all(|task| json.contains(&format!("\"task\":{task}}}"))));
        assert!(json.matches("\"task\":-1}").count() >= 3);
        assert!(json.starts_with("{\"traceEvents\":[") && json.trim_end().ends_with("]}"));
    }
}
