// Throwaway: event dedup two ways.
//   shared:      one presized lock-free table, insert with a prefetch 16 rows ahead, losers flagged dead in place.
//   partitioned: scatter 32-byte records by id hash, then dedup each partition with a small local table.
use rayon::prelude::*;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::time::Instant;

#[derive(Clone, Copy)]
#[repr(C)]
struct Rec { id: u64, user: u64, ingest: u32, ts: u32, pay: u64 }

const N: usize = 62_000_000;
const C1: u64 = 0x9E37_79B9_7F4A_7C15;
const C2: u64 = 0xD6E8_FEB8_6659_FD93;

// A bijection, so distinct rows get distinct ids.
fn mix(x: u64) -> u64 { let x = (x ^ x >> 32).wrapping_mul(C1); x ^ x >> 29 }

fn records() -> Vec<Rec> {
    (0..N as u64).into_par_iter().map(|j| {
        let r = mix(j.wrapping_mul(C2) ^ 0xABCD);
        // Every 32nd row is another copy of a random row's event, with its own ingest time.
        let src = if j % 32 == 31 { let t = r % N as u64; if t % 32 == 31 { t - 1 } else { t } } else { j };
        Rec { id: mix(src + 1), user: r, ingest: (r >> 40) as u32 & 0xF_FFFF, ts: r as u32 >> 2, pay: j }
    }).collect()
}

#[inline(always)]
fn prefetch<T>(p: &T) {
    #[cfg(target_arch = "x86_64")]
    unsafe { core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p as *const T as *const i8) }
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p as *const T, options(nostack, preserves_flags, readonly)) }
}

fn zeroed<A>(len: usize) -> Vec<A> {
    // calloc-backed, so untouched pages cost nothing until first use. A is an atomic integer.
    let mut raw = ManuallyDrop::new(vec![0u8; len * size_of::<A>()]);
    assert!(raw.as_ptr() as usize % align_of::<A>() == 0);
    unsafe { Vec::from_raw_parts(raw.as_mut_ptr() as *mut A, len, len) }
}

fn shared(recs: &[Rec], bits: u32, pretouch: bool, ahead: bool) -> (f64, usize, u64) {
    let t = Instant::now();
    let table: Vec<AtomicU64> = zeroed(2usize << bits);
    if pretouch { table.par_chunks(1 << 18).for_each(|c| c.iter().for_each(|x| x.store(0, Relaxed))); }
    let dead: Vec<AtomicU8> = zeroed(recs.len());
    let mask = (1usize << bits) - 1;
    let chunk = recs.len().div_ceil(rayon::current_num_threads());
    recs.par_chunks(chunk).enumerate().for_each(|(c, part)| {
        const B: usize = 16;
        for (b, block) in part.chunks(B).enumerate() {
            let mut h = [0usize; B];
            for (k, r) in block.iter().enumerate() {
                h[k] = (r.id.wrapping_mul(C2) >> (64 - bits)) as usize;
                if ahead { prefetch(&table[2 * h[k]]); }
            }
            for (k, r) in block.iter().enumerate() {
                let row = (c * chunk + b * B + k) as u64;
                let mut at = h[k];
                loop {
                    let key = &table[2 * at];
                    let cur = key.load(Relaxed);
                    if cur == r.id { break; }
                    if cur == 0 {
                        match key.compare_exchange(0, r.id, Relaxed, Relaxed) {
                            Ok(_) => break,
                            Err(now) if now == r.id => break,
                            Err(_) => {}
                        }
                    }
                    at = (at + 1) & mask;
                }
                // Inverted, so zeroed memory means empty and fetch_max keeps the smallest (ingest, row).
                let mine = !((r.ingest as u64) << 32 | row);
                let prev = table[2 * at + 1].fetch_max(mine, Relaxed);
                if prev != 0 { dead[!prev.min(mine) as u32 as usize].store(1, Relaxed); }
            }
        }
    });
    let secs = t.elapsed().as_secs_f64();
    let (n, sum) = recs.par_iter().zip(&dead).filter(|(_, d)| d.load(Relaxed) == 0)
        .map(|(r, _)| (1usize, r.ingest as u64)).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    (secs, n, sum)
}

fn partitioned(recs: &[Rec], bits: u32) -> (f64, f64, usize, u64) {
    let p = 1usize << bits;
    let t = Instant::now();
    let local: Vec<Vec<Vec<Rec>>> = recs.par_chunks(recs.len().div_ceil(rayon::current_num_threads())).map(|chunk| {
        let mut b: Vec<Vec<Rec>> = (0..p).map(|_| Vec::with_capacity(chunk.len() / p * 9 / 8 + 64)).collect();
        for r in chunk { b[(r.id.wrapping_mul(C2) >> (64 - bits)) as usize].push(*r); }
        b
    }).collect();
    let scatter = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let (n, sum) = (0..p).into_par_iter().map_init(|| (Vec::<u32>::new(), Vec::<Rec>::new()), |(table, keep), i| {
        let rows: usize = local.iter().map(|t| t[i].len()).sum();
        let cap = (2 * rows).next_power_of_two().max(1024);
        let shift = 64 - bits - cap.trailing_zeros();
        table.clear();
        table.resize(cap, 0);
        keep.clear();
        for piece in &local {
            for r in &piece[i] {
                // The bits just below the ones that chose the partition.
                let mut at = (r.id.wrapping_mul(C2) >> shift) as usize & (cap - 1);
                loop {
                    let slot = table[at];
                    if slot == 0 { keep.push(*r); table[at] = keep.len() as u32; break; }
                    let kept = &mut keep[slot as usize - 1];
                    if kept.id == r.id { if r.ingest < kept.ingest { *kept = *r; } break; }
                    at = (at + 1) & (cap - 1);
                }
            }
        }
        (keep.len(), keep.iter().map(|r| r.ingest as u64).sum::<u64>())
    }).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    (scatter, t.elapsed().as_secs_f64(), n, sum)
}

fn main() {
    let recs = records();
    println!("threads {} rows {}", rayon::current_num_threads(), recs.len());
    for round in 0..3 {
        let mut line = format!("round {round}:");
        let mut check = None;
        for (name, pretouch, ahead) in [("shared lazy no-prefetch", false, false), ("shared lazy prefetch", false, true), ("shared pretouched prefetch", true, true)] {
            let (secs, n, sum) = shared(&recs, 27, pretouch, ahead);
            assert_eq!(*check.get_or_insert((n, sum)), (n, sum));
            line += &format!(" {name} {secs:.3}s |");
        }
        for bits in [9, 11, 12] {
            let (scatter, dedup, n, sum) = partitioned(&recs, bits);
            assert_eq!(check, Some((n, sum)));
            line += &format!(" partitioned 2^{bits} {:.3}s (scatter {scatter:.3} + dedup {dedup:.3}) |", scatter + dedup);
        }
        println!("{line} survivors {}", check.unwrap().0);
    }
}
