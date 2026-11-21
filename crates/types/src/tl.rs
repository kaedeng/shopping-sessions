//! `SS_TIMING=2`: a timeline of named marks, from any thread, printed at the end of the run.
//!
//! `SS_TIMING=1` gives a stage's wall time; this says where inside a stage the time went and
//! which thread spent it, which is what decides a run of a few tens of milliseconds: a serial
//! step, a worker that starts late, a wait. Off, a mark is one load and a branch.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();
static MARKS: Mutex<Vec<(&'static str, u64, u64)>> = Mutex::new(Vec::new());

/// Sets the timeline's zero and whether marks are kept. Call once, first thing.
pub fn start(on: bool) {
    START.get_or_init(Instant::now);
    ON.store(on, Relaxed);
}

/// Whether marks are kept.
#[inline(always)]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// Notes that the calling thread reached `name` now.
#[inline(always)]
pub fn mark(name: &'static str) {
    if on() {
        keep(name);
    }
}

#[cold]
#[inline(never)]
fn keep(name: &'static str) {
    let ns = START.get().map_or(0, |start| start.elapsed().as_nanos() as u64);
    MARKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((name, ns, thread()));
}

/// A small number per thread, in order of first mark.
fn thread() -> u64 {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static MINE: u64 = NEXT.fetch_add(1, Relaxed);
    }
    MINE.with(|mine| *mine)
}

/// Prints every mark in time order: milliseconds since `start`, the thread, the name.
pub fn report() {
    if !on() {
        return;
    }
    let mut marks = MARKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    marks.sort_by_key(|&(_, ns, _)| ns);
    for (name, ns, thread) in marks {
        eprintln!("tl {:>9.3} ms  t{thread:<2} {name}", ns as f64 / 1e6);
    }
}
