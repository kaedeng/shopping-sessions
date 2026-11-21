//! Slot `par-counter`: one thread per worker state, tasks handed out by a shared counter in
//! index order, so the caller orders them largest-first. Returning is the stage's barrier.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

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

/// Runs `job(worker, state)` once per worker, the first on the calling thread.
pub fn each<S: Send>(states: &mut [S], job: impl Fn(usize, &mut S) + Sync) {
    let (first, rest) = states.split_first_mut().expect("at least one worker");
    let stage = trace::claim_stage();
    let job = |worker, state: &mut S| {
        let start = trace::now();
        job(worker, state);
        trace::span(stage, worker, trace::WHOLE, start);
    };
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
