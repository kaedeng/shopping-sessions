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
mod pool {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
    use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

    /// A round's job, called with the worker's number.
    type Job<'a> = dyn Fn(usize) + Sync + 'a;

    struct State {
        /// Counts the rounds; a worker takes part once in each round that includes it.
        round: u64,
        /// The round's job, its lifetime erased: `round` does not return before `left` is 0.
        job: Option<&'static Job<'static>>,
        /// Workers `1..count` take part in the round.
        count: usize,
        /// Of them, those that have not finished.
        left: usize,
        panicked: bool,
        /// Threads so far: workers `1..=threads`.
        threads: usize,
    }

    struct Pool {
        state: Mutex<State>,
        /// The workers wait here for a round, the caller there for its last worker.
        start: Condvar,
        done: Condvar,
        /// `State::round` again, to read without the lock: a worker watches it for a moment
        /// before it sleeps, since stages mostly follow each other within microseconds.
        round: AtomicU64,
        /// One round at a time. A caller that finds the pool taken (a job that itself asks
        /// for workers, or another thread's) gets `false` and spawns threads.
        taken: AtomicBool,
    }

    /// Looks at the round counter this many times before sleeping: some tens of microseconds.
    const SPINS: usize = 2000;

    fn pool() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        POOL.get_or_init(|| Pool {
            state: Mutex::new(State { round: 0, job: None, count: 0, left: 0, panicked: false, threads: 0 }),
            start: Condvar::new(),
            done: Condvar::new(),
            round: AtomicU64::new(0),
            taken: AtomicBool::new(false),
        })
    }

    fn lock(pool: &Pool) -> MutexGuard<'_, State> {
        // Jobs run outside the lock and their panics are caught, so it is never poisoned by one.
        pool.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Worker `index`'s thread; `seen` is the round at its start, which it is not part of.
    fn work(pool: &'static Pool, index: usize, mut seen: u64) {
        loop {
            let mut spins = 0;
            while pool.round.load(SeqCst) == seen && spins < SPINS {
                std::hint::spin_loop();
                spins += 1;
            }
            let mut state = lock(pool);
            while state.round == seen {
                state = pool.start.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            seen = state.round;
            if index >= state.count {
                continue;
            }
            let job = state.job.expect("a round has a job");
            drop(state);
            let ok = catch_unwind(AssertUnwindSafe(|| job(index))).is_ok();
            let mut state = lock(pool);
            state.panicked |= !ok;
            state.left -= 1;
            if state.left == 0 {
                pool.done.notify_all();
            }
        }
    }

    /// Makes sure workers `1..workers` exist. Called with the pool taken, or before any round.
    fn grow(pool: &'static Pool, state: &mut State, workers: usize) {
        while state.threads + 1 < workers {
            state.threads += 1;
            let (index, seen) = (state.threads, state.round);
            std::thread::Builder::new().name(format!("worker-{index}")).spawn(move || work(pool, index, seen)).expect("a worker thread");
        }
    }

    /// Starts the threads for `workers` workers now, so that no stage has to.
    pub fn start(workers: usize) {
        let pool = pool();
        grow(pool, &mut lock(pool), workers);
    }

    /// Runs `job(w)` for every `w` in `0..workers`, `job(0)` on the calling thread, and
    /// returns `true` once all have returned; or returns `false` at once, nothing run, when
    /// the pool is in use. Panics if a job panicked.
    pub fn round(workers: usize, job: &Job<'_>) -> bool {
        let pool = pool();
        if pool.taken.swap(true, SeqCst) {
            return false;
        }
        // SAFETY: only the lifetime changes. The reference is dropped from the state before
        // this function returns or unwinds (`Wait`), and by then no worker is inside the job.
        let erased: &'static Job<'static> = unsafe { std::mem::transmute::<&Job<'_>, &'static Job<'static>>(job) };
        {
            let mut state = lock(pool);
            grow(pool, &mut state, workers);
            (state.job, state.count, state.left, state.panicked) = (Some(erased), workers, workers - 1, false);
            state.round += 1;
            pool.round.store(state.round, SeqCst);
        }
        pool.start.notify_all();
        /// Waits for the round's workers, also when the caller's own job unwinds.
        struct Wait(&'static Pool);
        impl Drop for Wait {
            fn drop(&mut self) {
                let mut state = lock(self.0);
                while state.left > 0 {
                    state = self.0.done.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                state.job = None;
                drop(state);
                self.0.taken.store(false, SeqCst);
            }
        }
        let wait = Wait(pool);
        job(0);
        drop(wait);
        // `panicked` is this round's: the next round resets it, and only after taking the pool.
        assert!(!lock(pool).panicked, "worker panicked");
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
        loop {
            let index = next.fetch_add(1, Relaxed);
            if index >= tasks {
                break;
            }
            let start = trace::now();
            task(worker, state, index);
            trace::span(stage, worker, index, start);
        }
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
        let state = make(worker);
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
