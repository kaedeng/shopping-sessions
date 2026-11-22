//! `SS_PROBE=1`: what the machine under the run is, written to stderr once the output is
//! written. For one submission that is read, not scored: the CPU and which of the allowed
//! CPUs share a core, the huge-page settings and what the run's memory ended up on, the
//! mitigations, the cgroup's limits and throttling, where the input came from (page cache or
//! disk), how long the threads waited for a CPU, and the clock the cores ran at.
//!
//! Everything is read from `/proc` and `/sys`; what the container does not show is left out.

use std::fmt::Write as _;
use std::time::Instant;

/// What is read before the run, to set beside the same after it.
pub struct Probe {
    vmstat: String,
    stat: String,
    /// The CPUs the process may use, read before the workers are pinned to one each.
    allowed: Vec<usize>,
    started: Instant,
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// `path`'s text under its name, a line each.
fn show(out: &mut String, path: &str) {
    if let Some(text) = read(path) {
        let text = text.trim_end();
        if text.contains('\n') {
            let _ = writeln!(out, "probe: {path}:");
            for line in text.lines() {
                let _ = writeln!(out, "probe:   {line}");
            }
        } else {
            let _ = writeln!(out, "probe: {path}: {text}");
        }
    }
}

/// The lines of `text` that begin with one of `keys`.
fn lines<'t>(text: &'t str, keys: &[&str]) -> Vec<&'t str> {
    text.lines().filter(|line| keys.iter().any(|key| line.starts_with(key))).collect()
}

fn grep(out: &mut String, path: &str, keys: &[&str]) {
    if let Some(text) = read(path) {
        for line in lines(&text, keys) {
            let _ = writeln!(out, "probe: {path}: {line}");
        }
    }
}

const VMSTAT: &[&str] = &["thp_", "compact_", "pgfault", "pgmajfault", "pgsteal", "pgscan", "allocstall", "numa_"];

#[cfg(target_arch = "x86_64")]
fn ticks() -> u64 {
    // SAFETY: every x86-64 CPU has the counter.
    unsafe { std::arch::x86_64::_rdtsc() }
}

#[cfg(not(target_arch = "x86_64"))]
fn ticks() -> u64 {
    0
}

/// The clock of the calling thread's core, in GHz, and its time-stamp counter's: a chain of
/// multiply-and-rotate, 4 cycles a step on every Intel and AMD core of the last decade.
fn clock() -> (f64, f64) {
    const STEPS: u64 = 4_000_000;
    let mut x = std::hint::black_box(0x9E37_79B9_7F4A_7C15u64);
    let (start, from) = (Instant::now(), ticks());
    for _ in 0..STEPS {
        x = x.wrapping_mul(0x2545_F491_4F6C_DD1D).rotate_left(5);
    }
    std::hint::black_box(x);
    let (ns, counted) = (start.elapsed().as_nanos() as f64, ticks().wrapping_sub(from) as f64);
    ((4 * STEPS) as f64 / ns, counted / ns)
}

/// The word in brackets of a setting like `always [madvise] never`.
fn chosen(path: &str) -> Option<String> {
    let text = read(path)?;
    let from = text.find('[')? + 1;
    Some(text[from..from + text[from..].find(']')?].to_owned())
}

/// `SS_PROBE=sleep`: the machine as a number, for a scorer that shows a run's time and
/// nothing else. The run then sleeps 100 ms times this number before it ends, so
/// `(time - the usual time) / 0.1 s`, rounded, is the number; four digits, read from the
/// lowest:
///
/// - `% 2`, an advised huge-page fault may compact memory itself (`defrag` is `always`,
///   `madvise` or `defer+madvise`): 1 yes, 0 no or unreadable;
/// - `/ 2 % 4`, transparent huge pages: 0 unreadable, 1 `always`, 2 `madvise`, 3 `never`;
/// - `/ 8 % 4`, the allowed CPUs: 0 not 8 of them or unreadable, 1 eight threads of 4 cores,
///   2 eight cores without a second thread, 3 eight cores whose second threads are not ours;
/// - `/ 32`, the CPU: 0 another Intel or unreadable, 1 Skylake-SP, 2 Cascade Lake or Cooper
///   Lake, 3 Ice Lake-SP, 4 Sapphire or Emerald Rapids, 5 AMD.
///
/// At most 191: 19.1 s.
fn code(allowed: &[usize]) -> u64 {
    let cpuinfo = read("/proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| cpuinfo.lines().find(|line| line.starts_with(key)).and_then(|line| line.split(':').nth(1)).map(|value| value.trim().to_owned()).unwrap_or_default();
    let number = |key: &str| field(key).parse::<u32>().unwrap_or(0);
    let cpu = match (field("vendor_id").as_str(), number("cpu family"), number("model\t"), number("stepping")) {
        ("AuthenticAMD", ..) => 5,
        ("GenuineIntel", 6, 85, ..=4) => 1,
        ("GenuineIntel", 6, 85, _) => 2,
        ("GenuineIntel", 6, 106 | 108, _) => 3,
        ("GenuineIntel", 6, 143 | 207, _) => 4,
        _ => 0,
    };
    let lists: Vec<Vec<usize>> = allowed.iter().map(|&cpu| types::par::pin::siblings(cpu)).collect();
    let mut cores: Vec<usize> = lists.iter().filter_map(|list| list.iter().copied().min()).collect();
    cores.sort_unstable();
    cores.dedup();
    let readable = std::path::Path::new("/sys/devices/system/cpu/cpu0/topology/thread_siblings_list").exists();
    let threads = match (allowed.len(), cores.len(), readable) {
        (8, 4, true) => 1,
        (8, 8, true) if lists.iter().all(|list| list.len() == 1) => 2,
        (8, 8, true) => 3,
        _ => 0,
    };
    let huge = match chosen("/sys/kernel/mm/transparent_hugepage/enabled").as_deref() {
        Some("always") => 1,
        Some("madvise") => 2,
        Some("never") => 3,
        _ => 0,
    };
    let compacts = u64::from(matches!(chosen("/sys/kernel/mm/transparent_hugepage/defrag").as_deref(), Some("always" | "madvise" | "defer+madvise")));
    ((cpu * 4 + threads) * 4 + huge) * 2 + compacts
}

/// Whether this process can have 2 MB pages where the machine's setting is `never` (what the
/// first probe read on the scorer): by `MADV_COLLAPSE` (Linux 6.1, and it does not ask the
/// setting) on a range with one page written, and by a `MAP_HUGETLB` mapping (which needs
/// pages the machine's owner set aside).
#[cfg(target_os = "linux")]
fn huge_pages() -> (bool, bool) {
    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    }
    const HUGE: usize = 2 << 20;
    const READ_WRITE: i32 = 1 | 2;
    const PRIVATE_ANONYMOUS: i32 = 0x02 | 0x20;
    const MAP_HUGETLB: i32 = 0x40000;
    const MADV_COLLAPSE: i32 = 25;
    let huge_kb = || read("/proc/self/smaps_rollup").unwrap_or_default().lines().find_map(|line| line.strip_prefix("AnonHugePages:")).and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).unwrap_or(0);
    // SAFETY: fresh mappings of this function's own, unmapped before it returns; the one
    // write is inside the first.
    unsafe {
        let mut collapsed = false;
        let before = huge_kb();
        let block = mmap(std::ptr::null_mut(), 2 * HUGE, READ_WRITE, PRIVATE_ANONYMOUS, -1, 0);
        if block as isize != -1 && !block.is_null() {
            let aligned = ((block as usize + HUGE - 1) & !(HUGE - 1)) as *mut u8;
            aligned.write_volatile(1);
            // Said to have worked, and seen in the process's own count of huge pages.
            collapsed = madvise(aligned, HUGE, MADV_COLLAPSE) == 0 && huge_kb() >= before + 2048;
            munmap(block, 2 * HUGE);
        }
        let reserved = mmap(std::ptr::null_mut(), HUGE, READ_WRITE, PRIVATE_ANONYMOUS | MAP_HUGETLB, -1, 0);
        let set_aside = reserved as isize != -1 && !reserved.is_null();
        if set_aside {
            munmap(reserved, HUGE);
        }
        (collapsed, set_aside)
    }
}

#[cfg(not(target_os = "linux"))]
fn huge_pages() -> (bool, bool) {
    (false, false)
}

/// `SS_PROBE=sleep2`: four more facts, in steps of 200 ms. A step is far more than a switch
/// moves a run (tens of milliseconds), so the number can ride on a submission that tests
/// something else and both are read from the one time. Read from the lowest:
///
/// - `% 2`, a `MAP_HUGETLB` mapping is granted (`huge_pages`): 1 yes;
/// - `/ 2 % 2`, `MADV_COLLAPSE` makes a 2 MB page (`huge_pages`): 1 yes;
/// - `/ 4 % 2`, speculative store bypass for this process (`Speculation_Store_Bypass` in
///   `/proc/self/status`): 1 mitigated, so a load waits for the address of every store
///   before it (`x86.md`); 0 allowed, not affected or unreadable;
/// - `/ 8`, the kernel: 0 unreadable, 1 before 5.16 (a seccomp filter turns the store bypass
///   off), 2 from 5.16 to 6.0, 3 from 6.1 to 6.3 (`MADV_COLLAPSE` exists), 4 from 6.4 on
///   (page faults no longer queue behind the address-space lock).
///
/// At most 39: 7.8 s.
fn code2() -> u64 {
    let release = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = release.split(|letter: char| !letter.is_ascii_digit()).map(|part| part.parse::<u32>().ok());
    let kernel = match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) if (major, minor) < (5, 16) => 1,
        (Some(major), Some(minor)) if (major, minor) < (6, 1) => 2,
        (Some(major), Some(minor)) if (major, minor) < (6, 4) => 3,
        (Some(_), Some(_)) => 4,
        _ => 0,
    };
    let mitigated = read("/proc/self/status").unwrap_or_default().lines().any(|line| line.starts_with("Speculation_Store_Bypass:") && line.contains("mitigated"));
    let (collapsed, set_aside) = huge_pages();
    ((kernel * 2 + u64::from(mitigated)) * 2 + u64::from(collapsed)) * 2 + u64::from(set_aside)
}

/// `SS_PROBE=stage:NAME`: the stage whose time the run reports by waiting, `NAME` one of
/// `inputs`, `parse`, `dedup`, `sessions`, `output` (the laps of `SS_TIMING`) or `handback`
/// (the round before the exit). When that stage ends the run sleeps `SS_PROBE_TIMES` (10)
/// times what the stage took (`wait`), so on a scorer that shows nothing but a run's time,
/// the time less the same build's time without the probe, divided by 10, is the stage's
/// time there. A stage a submission; what the stages do not add up to is the process's
/// start and its exit.
pub fn stretched() -> Option<String> {
    std::env::var("SS_PROBE").ok()?.strip_prefix("stage:").map(str::to_owned)
}

/// Laps so far (`lapped`): 1 while the parse runs, 2 the dedup, 3 the sessions stage.
static LAPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Per stage, by `LAPS`: the core's clock over its time-stamp counter's as `f64` bits, read
/// in the middle of the stage (`read_clock`); 0 for none.
static CLOCKS: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// A stage has ended; `main` calls it at every lap.
pub fn lapped() {
    LAPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The clock of the calling thread's core over its time-stamp counter's: the fastest of
/// 2,000 bursts of about a microsecond, 2.4 ms in all.
///
/// A burst is 16 register adds a round, each needing the one before: one a cycle whatever
/// the clock, on any of four ports. The core's other thread is at work on the stage and
/// takes ports now and then, more in a stage of scalar code than in one of vector code,
/// which is the very comparison: a burst as long as 20 us read 5 to 15% low in the dedup
/// stage on a machine whose clock does not change. A burst this short is often left alone
/// from end to end, and the fastest one is the clock: the three stages then read the same
/// there. (The counter of an AMD core steps by 42, so a burst cannot be much shorter.)
#[cfg(target_arch = "x86_64")]
fn core_clock() -> f64 {
    const ROUNDS: u64 = 256;
    let mut best = 0f64;
    for _ in 0..2000 {
        let from = ticks();
        // SAFETY: registers only.
        unsafe {
            std::arch::asm!(
                "2:",
                "add {x}, {y}", "add {y}, {x}", "add {x}, {y}", "add {y}, {x}",
                "add {x}, {y}", "add {y}, {x}", "add {x}, {y}", "add {y}, {x}",
                "add {x}, {y}", "add {y}, {x}", "add {x}, {y}", "add {y}, {x}",
                "add {x}, {y}", "add {y}, {x}", "add {x}, {y}", "add {y}, {x}",
                "dec {n}",
                "jnz 2b",
                x = inout(reg) 1u64 => _,
                y = inout(reg) 1u64 => _,
                n = inout(reg) ROUNDS => _,
                options(nomem, nostack),
            );
        }
        let counted = ticks().wrapping_sub(from);
        if counted != 0 {
            best = best.max((16 * ROUNDS) as f64 / counted as f64);
        }
    }
    best
}

#[cfg(not(target_arch = "x86_64"))]
fn core_clock() -> f64 {
    0.0
}

/// `types::par::midway`'s hook: the clock of worker 0's core, in the middle of a stage.
fn read_clock() {
    use std::sync::atomic::Ordering::Relaxed;
    CLOCKS[LAPS.load(Relaxed).min(7)].store(core_clock().to_bits(), Relaxed);
}

/// `SS_PROBE=clock:parse` or `clock:sessions`: how far that stage's cores run below the
/// dedup stage's, as a wait. A Cascade Lake lowers a core's clock while it runs 512-bit
/// instructions (by about a tenth for the light ones, more for multiplies) and for most of
/// a millisecond after; the parse and the sessions stage are full of them, the dedup stage
/// has none, and all eight workers are busy in each, so the turbo bin is the same. The run
/// sleeps 50 ms times `10 + percent` at its end, `percent` from -9 to 60; no sleep at all
/// means a stage was not read. So the score less the same build's, over 0.05 s, less 10,
/// is the percent.
fn clock_code(stage: &str) -> u64 {
    use std::sync::atomic::Ordering::Relaxed;
    let read = |laps: usize| f64::from_bits(CLOCKS[laps].load(Relaxed));
    let (of, dedup) = (read(if stage == "sessions" { 3 } else { 1 }), read(2));
    eprintln!("probe: clock over counter: parse {:.4} dedup {:.4} sessions {:.4}", read(1), read(2), read(3));
    if of <= 0.0 || dedup <= 0.0 {
        return 0;
    }
    (10.0 + (100.0 * (1.0 - of / dedup)).round()).clamp(1.0, 70.0) as u64
}

/// The wait of `stretched` for a stage that took `took`.
pub fn wait(took: std::time::Duration) {
    std::thread::sleep(took * types::env::number("SS_PROBE_TIMES", 10) as u32);
}

impl Probe {
    /// `Some` when `SS_PROBE` is set. Call before the workers are started.
    pub fn start() -> Option<Probe> {
        let probe = std::env::var("SS_PROBE").ok()?;
        if probe.starts_with("clock:") {
            types::par::midway(read_clock);
        }
        Some(Probe { vmstat: read("/proc/vmstat").unwrap_or_default(), stat: read("/proc/stat").unwrap_or_default(), allowed: types::par::pin::allowed(), started: Instant::now() })
    }

    /// The report. `data` is the input folder. With `SS_PROBE=sleep`, the machine's number
    /// (`code`) as a wait instead.
    pub fn finish(&self, data: &std::path::Path) {
        // A stage's wait is all that probe does: no report, which would add its own time.
        if stretched().is_some() {
            return;
        }
        if let Some(stage) = std::env::var("SS_PROBE").ok().as_deref().and_then(|value| value.strip_prefix("clock:")) {
            let code = clock_code(stage);
            eprintln!("probe: clock code {code}");
            std::thread::sleep(std::time::Duration::from_millis(50 * code));
            return;
        }
        if std::env::var_os("SS_PROBE").is_some_and(|value| value == "sleep") {
            let code = code(&self.allowed);
            eprintln!("probe: code {code}");
            std::thread::sleep(std::time::Duration::from_millis(100 * code));
            return;
        }
        if std::env::var_os("SS_PROBE").is_some_and(|value| value == "sleep2") {
            let code = code2();
            eprintln!("probe: code2 {code}");
            std::thread::sleep(std::time::Duration::from_millis(200 * code));
            return;
        }
        let mut out = String::new();
        let o = &mut out;
        let _ = writeln!(o, "probe: run {:.1} ms", self.started.elapsed().as_secs_f64() * 1e3);
        // The machine.
        show(o, "/proc/sys/kernel/osrelease");
        show(o, "/proc/version");
        if let Some(cpuinfo) = read("/proc/cpuinfo") {
            let _ = writeln!(o, "probe: cpuinfo: {} processors", cpuinfo.lines().filter(|line| line.starts_with("processor")).count());
            for key in ["vendor_id", "cpu family", "model\t", "model name", "stepping", "microcode", "cache size", "siblings", "cpu cores", "flags", "bugs", "address sizes"] {
                if let Some(line) = cpuinfo.lines().find(|line| line.starts_with(key)) {
                    let _ = writeln!(o, "probe: cpuinfo: {line}");
                }
            }
            let mhz: Vec<&str> = cpuinfo.lines().filter(|line| line.starts_with("cpu MHz")).filter_map(|line| line.split(':').nth(1)).map(str::trim).collect();
            let _ = writeln!(o, "probe: cpuinfo: cpu MHz {}", mhz.join(" "));
        }
        // The CPUs this process may use, and which of them share a core and a cache.
        let allowed = &self.allowed;
        let _ = writeln!(o, "probe: allowed cpus {allowed:?}; workers pinned in the order {:?}", types::par::pin::order());
        for &cpu in allowed {
            let at = |file: &str| read(&format!("/sys/devices/system/cpu/cpu{cpu}/{file}")).map_or_else(|| "?".into(), |text| text.trim().to_owned());
            let _ = writeln!(
                o,
                "probe: cpu{cpu}: siblings {} core {} package {} l2 shared with {} l3 shared with {} governor {} cur_khz {} max_khz {}",
                at("topology/thread_siblings_list"),
                at("topology/core_id"),
                at("topology/physical_package_id"),
                at("cache/index2/shared_cpu_list"),
                at("cache/index3/shared_cpu_list"),
                at("cpufreq/scaling_governor"),
                at("cpufreq/scaling_cur_freq"),
                at("cpufreq/cpuinfo_max_freq"),
            );
        }
        if let Some(&cpu) = allowed.first() {
            for index in 0..5 {
                let at = |file: &str| read(&format!("/sys/devices/system/cpu/cpu{cpu}/cache/index{index}/{file}")).map_or_else(|| "?".into(), |text| text.trim().to_owned());
                if at("size") != "?" {
                    let _ = writeln!(o, "probe: cpu{cpu} cache{index}: L{} {} {} ways {} line {}", at("level"), at("type"), at("size"), at("ways_of_associativity"), at("coherency_line_size"));
                }
            }
        }
        show(o, "/sys/devices/system/cpu/online");
        show(o, "/sys/devices/system/cpu/smt/active");
        show(o, "/sys/devices/system/node/online");
        show(o, "/sys/devices/system/cpu/intel_pstate/status");
        show(o, "/sys/devices/system/cpu/intel_pstate/no_turbo");
        show(o, "/sys/devices/system/cpu/cpuidle/current_driver");
        show(o, "/sys/hypervisor/type");
        show(o, "/sys/class/dmi/id/sys_vendor");
        show(o, "/sys/class/dmi/id/product_name");
        // Memory: huge pages, and what this process got.
        for file in ["enabled", "defrag", "shmem_enabled", "use_zero_page", "khugepaged/defrag", "khugepaged/max_ptes_none"] {
            show(o, &format!("/sys/kernel/mm/transparent_hugepage/{file}"));
        }
        grep(o, "/proc/meminfo", &["MemTotal", "MemFree", "MemAvailable", "Cached", "AnonHugePages", "HugePages_Total", "Hugepagesize", "SwapTotal", "Dirty"]);
        show(o, "/proc/self/smaps_rollup");
        show(o, "/proc/sys/vm/overcommit_memory");
        show(o, "/proc/sys/vm/max_map_count");
        show(o, "/proc/sys/kernel/numa_balancing");
        show(o, "/proc/sys/kernel/sched_autogroup_enabled");
        if let Some(after) = read("/proc/vmstat") {
            for line in lines(&after, VMSTAT) {
                let (name, now) = line.split_once(' ').unwrap_or((line, "0"));
                let before = self.vmstat.lines().find_map(|old| old.strip_prefix(name).and_then(|rest| rest.strip_prefix(' '))).unwrap_or("0");
                let (now, before) = (now.trim().parse::<i64>().unwrap_or(0), before.trim().parse::<i64>().unwrap_or(0));
                if now != before {
                    let _ = writeln!(o, "probe: vmstat during the run (machine-wide): {name} +{}", now - before);
                }
            }
        }
        // The process.
        grep(
            o,
            "/proc/self/status",
            &["Vm", "Rss", "HugetlbPages", "Threads", "Seccomp", "Speculation", "Cpus_allowed_list", "Mems_allowed_list", "voluntary_ctxt_switches", "nonvoluntary_ctxt_switches", "NoNewPrivs", "CapEff", "THP_enabled"],
        );
        show(o, "/proc/self/io");
        show(o, "/proc/self/schedstat");
        show(o, "/proc/self/limits");
        if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
            for task in tasks.flatten() {
                let folder = task.path();
                let part = |file: &str| std::fs::read_to_string(folder.join(file)).map_or_else(|_| "?".into(), |text| text.trim().to_owned());
                let stat = part("stat");
                // After the name's closing parenthesis: field 3 on; 14 and 15 are the CPU
                // ticks in user and kernel, 39 the CPU the thread last ran on.
                let fields: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
                let field = |number: usize| fields.get(number - 3).copied().unwrap_or("?");
                let _ = writeln!(o, "probe: thread {} {}: schedstat (run ns, waited ns, slices) {}; user ticks {} kernel ticks {} minor faults {} last cpu {}", task.file_name().to_string_lossy(), part("comm"), part("schedstat"), field(14), field(15), field(10), field(39));
            }
        }
        // Mitigations.
        if let Ok(files) = std::fs::read_dir("/sys/devices/system/cpu/vulnerabilities") {
            let mut names: Vec<_> = files.flatten().map(|file| file.path()).collect();
            names.sort();
            for name in names {
                show(o, &name.to_string_lossy());
            }
        }
        // The cgroup: limits, throttling, pressure.
        show(o, "/proc/self/cgroup");
        for file in [
            "cpu.max", "cpu.stat", "cpu.weight", "cpuset.cpus", "cpuset.cpus.effective", "cpuset.mems.effective", "memory.max", "memory.high", "memory.swap.max", "memory.peak", "memory.current", "memory.events", "pids.max", "cpu.pressure", "memory.pressure", "io.pressure", "io.max", "io.stat",
            "cpu/cpu.cfs_quota_us", "cpu/cpu.cfs_period_us", "cpu/cpu.stat", "cpu/cpu.shares", "cpuset/cpuset.cpus", "cpuset/cpuset.effective_cpus", "memory/memory.limit_in_bytes", "memory/memory.max_usage_in_bytes", "memory/memory.failcnt",
        ] {
            show(o, &format!("/sys/fs/cgroup/{file}"));
        }
        grep(o, "/sys/fs/cgroup/memory.stat", &["anon ", "file ", "anon_thp", "file_mapped", "pgfault", "pgmajfault", "thp_", "workingset_refault", "pgscan", "pgsteal"]);
        for file in ["cpu", "memory", "io"] {
            show(o, &format!("/proc/pressure/{file}"));
        }
        show(o, "/proc/loadavg");
        show(o, "/proc/uptime");
        // CPU time machine-wide during the run, per CPU: user nice system idle iowait irq
        // softirq steal. Steal is another guest on the core; a busy CPU that is not ours is
        // a neighbour.
        if let Some(after) = read("/proc/stat") {
            for line in after.lines().filter(|line| line.starts_with("cpu")) {
                let mut words = line.split_whitespace();
                let name = words.next().unwrap_or("");
                let now: Vec<i64> = words.filter_map(|word| word.parse().ok()).collect();
                let before: Vec<i64> = self.stat.lines().find(|old| old.split_whitespace().next() == Some(name)).map(|old| old.split_whitespace().skip(1).filter_map(|word| word.parse().ok()).collect()).unwrap_or_default();
                let change: Vec<i64> = now.iter().zip(&before).map(|(now, before)| now - before).collect();
                let ours = name[3..].parse::<usize>().is_ok_and(|cpu| allowed.contains(&cpu));
                let busy: i64 = change.iter().take(3).sum::<i64>() + change.iter().skip(5).take(3).sum::<i64>();
                if name == "cpu" || ours || busy > 2 {
                    let _ = writeln!(o, "probe: stat during the run: {name}{} {change:?}", if ours { " (ours)" } else { "" });
                }
            }
        }
        // The folders.
        if let Some(mounts) = read("/proc/self/mountinfo") {
            for line in mounts.lines() {
                let place = line.split_whitespace().nth(4).unwrap_or("");
                if ["/", "/data", "/output", "/scratch", "/tmp"].contains(&place) {
                    let _ = writeln!(o, "probe: mount: {line}");
                }
            }
        }
        for folder in ["events", "clicks", "links"] {
            let (mut files, mut bytes, mut largest) = (0u64, 0u64, 0u64);
            if let Ok(entries) = std::fs::read_dir(data.join(folder)) {
                for size in entries.flatten().filter_map(|entry| entry.metadata().ok()).map(|meta| meta.len()) {
                    (files, bytes, largest) = (files + 1, bytes + size, largest.max(size));
                }
            }
            let _ = writeln!(o, "probe: {}/{folder}: {files} files, {bytes} bytes, the largest {largest}", data.display());
        }
        let mut env: Vec<String> = std::env::vars_os().map(|(name, value)| format!("{}={}", name.to_string_lossy(), value.to_string_lossy())).collect();
        env.sort();
        let _ = writeln!(o, "probe: env: {}", env.join(" "));
        // The clock, three times: the first reading may still be coming out of the run's
        // 512-bit work.
        for _ in 0..3 {
            let (core, counter) = clock();
            let _ = writeln!(o, "probe: clock {core:.3} GHz, time-stamp counter {counter:.3} GHz");
        }
        let _ = writeln!(o, "probe: done {:.1} ms", self.started.elapsed().as_secs_f64() * 1e3);
        eprint!("{out}");
    }
}
