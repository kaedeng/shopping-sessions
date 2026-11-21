//! Slot `par-counter`: one thread per worker state, tasks handed out by a shared counter in
//! index order, so the caller orders them largest-first. Returning is the stage's barrier.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Runs `task(worker, state, index)` for every index in `0..tasks`, each worker on its own state.
pub fn run<S: Send>(states: &mut [S], tasks: usize, task: impl Fn(usize, &mut S, usize) + Sync) {
    let next = AtomicUsize::new(0);
    each(states, |worker, state| {
        loop {
            let index = next.fetch_add(1, Relaxed);
            if index >= tasks {
                break;
            }
            task(worker, state, index);
        }
    });
}

/// Runs `job(worker, state)` once per worker, the first on the calling thread.
pub fn each<S: Send>(states: &mut [S], job: impl Fn(usize, &mut S) + Sync) {
    let (first, rest) = states.split_first_mut().expect("at least one worker");
    thread::scope(|scope| {
        for (worker, state) in rest.iter_mut().enumerate() {
            let job = &job;
            scope.spawn(move || job(worker + 1, state));
        }
        job(0, first);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_task_runs_once() {
        let mut sums = vec![0usize; 4];
        super::run(&mut sums, 1000, |_, sum, index| *sum += index);
        assert_eq!(sums.iter().sum::<usize>(), 999 * 1000 / 2);
    }
}
