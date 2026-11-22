//! A second try when the first one dies.
//!
//! The pipeline that runs (`opaque`) is made for speed and has met generated data only.
//! Should it fault, fail an assertion or return an error on an input it was not made for,
//! the process would end with nothing written, and a run with no output scores nothing. So
//! the process answers its own death: it starts itself over (`execve`) with
//! `SS_PIPE=classic`, the pipeline that takes every ID as a string and has its own parse,
//! dedup and bucket walk (the two share the timestamps, the allocator, the workers and the
//! writer). That one takes seconds where the first takes tenths, and it is right.
//!
//! - `arm`, the first thing `main` does, keeps what the handler needs, made while nothing
//!   is wrong: the program's path, its arguments, its environment with `SS_PIPE=classic` and
//!   `SS_SECOND=1`, and the CPUs the process may use. The handler makes system calls only.
//! - The signals: SIGSEGV and SIGBUS (a bad address, a stack past its end), SIGILL, SIGFPE,
//!   and SIGABRT, which a panic ends in (`panic = "abort"`) and a failed allocation too.
//! - An error `run` returns takes the same way (`again`).
//! - The second try is not armed: what kills it ends the process. There is no third.
//! - Once the last file is written (`done`), a death loses nothing: the handler ends the
//!   process as a success.
//!
//! `SS_FALLBACK=off` leaves the signals as they are. `SS_FAULT=KIND:STAGE[:thread]` kills the
//! run when stage `STAGE` ends, for tests (`fault`).
//!
//! Linux only: elsewhere a death is a death, as it was.

use std::path::Path;
use std::sync::OnceLock;

/// In the environment of the second try, and of nothing else.
const SECOND: &str = "SS_SECOND";

/// Whether this process is the second try.
pub fn second() -> bool {
    std::env::var_os(SECOND).is_some()
}

/// Empties the two output folders: the second try writes where the first may have left
/// files, some of them cut short, and not under the names the second gives its own.
pub fn clear(out: &Path) {
    for folder in ["user_daily", "campaign_daily"] {
        let Ok(entries) = std::fs::read_dir(out.join(folder)) else { continue };
        for entry in entries.flatten() {
            // A folder among them stays: nothing here made it.
            drop(std::fs::remove_file(entry.path()));
        }
    }
}

/// `SS_FAULT`: the kind of death, the stage it comes after, and whether on a thread of its
/// own (a thread started as the workers are, where the calling thread is the first).
fn asked() -> Option<&'static (String, String, bool)> {
    static ASKED: OnceLock<Option<(String, String, bool)>> = OnceLock::new();
    ASKED
        .get_or_init(|| {
            let asked = std::env::var("SS_FAULT").ok()?;
            let mut parts = asked.split(':');
            Some((parts.next()?.to_owned(), parts.next().unwrap_or("").to_owned(), parts.next() == Some("thread")))
        })
        .as_ref()
}

/// `SS_FAULT=error`: the error `run` returns before it has done anything.
pub fn failing() -> Result<(), String> {
    match asked() {
        Some((kind, ..)) if kind == "error" => Err("SS_FAULT=error".into()),
        _ => Ok(()),
    }
}

/// `SS_FAULT=KIND:STAGE[:thread]`: the run dies here if `stage` is `STAGE`. The kinds:
/// `segv` (a store to a bad address), `overflow` (a stack past its end), `abort` (a panic),
/// `ill`, `fpe`, `bus` (the signal, raised). The stages are the ones `SS_TIMING` prints, and
/// `files`: a worker that has written its `user_daily` file, the others in the middle of
/// theirs.
pub fn fault(stage: &str) {
    let Some((kind, at, thread)) = asked() else { return };
    if at != stage || kind == "error" {
        return;
    }
    if *thread {
        drop(std::thread::spawn(move || imp::die(kind)).join());
    } else {
        imp::die(kind);
    }
}

pub use imp::{again, arm, done};

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::{CString, c_char, c_int};
    use std::hint::black_box;
    use std::os::unix::ffi::OsStringExt;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::SeqCst};

    /// The C library's `struct sigaction` (x86-64 and AArch64 alike): the handler, the
    /// signals held back while it runs, the flags, and a field the library fills itself.
    #[repr(C)]
    struct Action {
        handler: usize,
        mask: [u64; 16],
        flags: c_int,
        restorer: usize,
    }

    unsafe extern "C" {
        fn sigaction(signal: c_int, new: *const Action, old: *mut Action) -> c_int;
        fn sigprocmask(how: c_int, new: *const [u64; 16], old: *mut [u64; 16]) -> c_int;
        fn sched_getaffinity(pid: i32, size: usize, mask: *mut u64) -> i32;
        fn sched_setaffinity(pid: i32, size: usize, mask: *const u64) -> i32;
        fn execve(path: *const c_char, args: *const *const c_char, env: *const *const c_char) -> c_int;
        fn write(descriptor: c_int, bytes: *const u8, len: usize) -> isize;
        fn pthread_self() -> usize;
        fn pause() -> c_int;
        fn raise(signal: c_int) -> c_int;
        fn _exit(code: c_int) -> !;
    }

    const SIGILL: c_int = 4;
    const SIGABRT: c_int = 6;
    const SIGBUS: c_int = 7;
    const SIGFPE: c_int = 8;
    const SIGSEGV: c_int = 11;
    /// The handler runs on the thread's other stack where it has one: the standard library
    /// gives one to the first thread and to every thread it starts, for its own report of a
    /// stack that overflowed, which this handler takes the place of.
    const SA_ONSTACK: c_int = 0x0800_0000;
    const SIG_SETMASK: c_int = 2;
    /// What the process ends with when it cannot start over.
    const LOST: c_int = 3;

    /// What starting over takes, all of it made by `arm`.
    struct Exec {
        /// The program: by `/proc`, then as it was called.
        paths: [*const c_char; 2],
        args: *const *const c_char,
        env: *const *const c_char,
        /// The CPUs the process had when it began, if they could be read: a worker pinned
        /// to one CPU (`SS_PIN`) would hand that one CPU on to the whole second try.
        cpus: Option<[u64; 16]>,
    }

    static EXEC: AtomicPtr<Exec> = AtomicPtr::new(null_mut());
    static DONE: AtomicBool = AtomicBool::new(false);
    /// The thread that is starting the process over, as `pthread_self` names it; 0 for none.
    static DYING: AtomicUsize = AtomicUsize::new(0);

    /// Keeps what the second try starts from and takes the five signals. Nothing for a
    /// process that is the second try, or with `SS_FALLBACK=off`.
    pub fn arm() {
        if super::second() || types::env::pick("SS_FALLBACK", &["on", "off"]) == 1 {
            return;
        }
        // Texts and lists that live to the end of the process, as C wants them.
        let text = |bytes: Vec<u8>| CString::new(bytes).ok().map(|text| text.into_raw().cast_const());
        let list = |mut texts: Vec<*const c_char>| {
            texts.push(null());
            Box::leak(texts.into_boxed_slice()).as_ptr()
        };
        let args: Vec<*const c_char> = std::env::args_os().filter_map(|arg| text(arg.into_vec())).collect();
        let Some(&called) = args.first() else { return };
        // The environment as it is, but for the pipeline, and for a death asked for.
        let mut env: Vec<*const c_char> = std::env::vars_os()
            .filter(|(name, _)| name != "SS_PIPE" && name != "SS_FAULT")
            .filter_map(|(name, value)| {
                let mut pair = name.into_vec();
                pair.push(b'=');
                pair.extend(value.into_vec());
                text(pair)
            })
            .collect();
        env.push(c"SS_PIPE=classic".as_ptr());
        env.push(c"SS_SECOND=1".as_ptr());
        let mut cpus = [0u64; 16];
        // SAFETY: the mask is as long as the call is told.
        let read = unsafe { sched_getaffinity(0, size_of_val(&cpus), cpus.as_mut_ptr()) } == 0;
        let exec = Exec { paths: [c"/proc/self/exe".as_ptr(), called], args: list(args), env: list(env), cpus: read.then_some(cpus) };
        EXEC.store(Box::leak(Box::new(exec)), SeqCst);
        let action = Action { handler: died as extern "C" fn(c_int) as usize, mask: [0; 16], flags: SA_ONSTACK, restorer: 0 };
        for signal in [SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT] {
            // SAFETY: `action` is the C library's structure, and `died` a handler that
            // makes system calls only.
            unsafe { sigaction(signal, &action, null_mut()) };
        }
    }

    /// The last file is written: from here a death ends the process as a success.
    pub fn done() {
        DONE.store(true, SeqCst);
    }

    /// The program again, as the second try. Returns if no `execve` did it.
    ///
    /// # Safety
    /// `exec` is what `arm` made.
    unsafe fn start_over(exec: &Exec) {
        // SAFETY: system calls on memory `arm` leaked; the lists end in a null.
        unsafe {
            if let Some(cpus) = &exec.cpus {
                sched_setaffinity(0, size_of_val(cpus), cpus.as_ptr());
            }
            // The new program gets this thread's held-back signals, the one being handled
            // among them.
            sigprocmask(SIG_SETMASK, &[0; 16], null_mut());
            for path in exec.paths {
                execve(path, exec.args, exec.env);
            }
        }
    }

    /// The handler of the five signals.
    extern "C" fn died(signal: c_int) {
        // SAFETY: system calls and atomics, nothing that allocates or locks; `EXEC` is set
        // before the handler is.
        unsafe {
            if DONE.load(SeqCst) {
                _exit(0);
            }
            let me = pthread_self();
            if let Err(first) = DYING.compare_exchange(0, me, SeqCst, SeqCst) {
                if first == me {
                    // This thread died again on its way to the second try.
                    _exit(LOST);
                }
                // Another thread is starting the process over, which ends this one.
                loop {
                    pause();
                }
            }
            let mut said = *b"shopping-sessions: signal 00, starting over with SS_PIPE=classic\n";
            said[26] = b'0' + (signal / 10 % 10) as u8;
            said[27] = b'0' + (signal % 10) as u8;
            write(2, said.as_ptr(), said.len());
            let exec = EXEC.load(SeqCst);
            if !exec.is_null() {
                start_over(&*exec);
            }
            _exit(LOST);
        }
    }

    /// `run` returned an error: the second try, if this is the first and its files are not
    /// all written. Returns where there is none to start.
    pub fn again() {
        let exec = EXEC.load(SeqCst);
        if exec.is_null() || DONE.load(SeqCst) {
            return;
        }
        // SAFETY: a thread's own name.
        if DYING.compare_exchange(0, unsafe { pthread_self() }, SeqCst, SeqCst).is_err() {
            // A worker's death got there first.
            loop {
                // SAFETY: waits for a signal, or for the process's end.
                unsafe { pause() };
            }
        }
        eprintln!("shopping-sessions: starting over with SS_PIPE=classic");
        // SAFETY: `arm` made `exec` and never frees it.
        unsafe { start_over(&*exec) };
    }

    /// Calls itself until the stack ends.
    #[inline(never)]
    fn deep(depth: u64) -> u64 {
        let held = black_box([depth; 64]);
        if black_box(depth) == u64::MAX { 0 } else { deep(depth + 1).wrapping_add(held[7]) }
    }

    /// `super::fault`'s death of kind `kind`.
    pub(super) fn die(kind: &str) {
        match kind {
            // SAFETY: none: the store is to an address nothing is mapped at, which is the
            // point.
            "segv" => unsafe { black_box(8 as *mut u64).write_volatile(1) },
            "overflow" => drop(black_box(deep(0))),
            "abort" => panic!("SS_FAULT=abort"),
            // SAFETY: a signal to the calling thread.
            "ill" => drop(unsafe { raise(SIGILL) }),
            // SAFETY: as above.
            "fpe" => drop(unsafe { raise(SIGFPE) }),
            // SAFETY: as above.
            "bus" => drop(unsafe { raise(SIGBUS) }),
            _ => panic!("SS_FAULT={kind}: segv, overflow, abort, ill, fpe, bus or error"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn arm() {}

    pub fn done() {}

    pub fn again() {}

    pub(super) fn die(kind: &str) {
        panic!("SS_FAULT={kind}");
    }
}
