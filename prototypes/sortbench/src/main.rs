// Throwaway: sessionize sort. Global par_sort_unstable vs scatter by person, then sort_unstable or LSD radix per bucket.
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

const N: usize = 60_000_000;
const PEOPLE: u64 = 2_000_000;
const WINDOW_MS: u64 = 14 * 86_400_000;
const BITS: u32 = 10;

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; *s
}

fn keys() -> Vec<u64> {
    (0..N as u64).into_par_iter().map(|i| {
        let mut s = i.wrapping_mul(0x9E3779B97F4A7C15) | 1;
        rng(&mut s);
        let u = (rng(&mut s) >> 11) as f64 / (1u64 << 53) as f64;
        let person = if u < 0.075 { (u / 0.015) as u64 } else {
            let v = (rng(&mut s) >> 11) as f64 / (1u64 << 53) as f64;
            5 + (v.powi(4) * (PEOPLE - 5) as f64) as u64
        };
        person << 32 | rng(&mut s) % WINDOW_MS
    }).collect()
}

// The generator's time shape instead of uniform timestamps: a session sits in its own two-hour
// slot and lasts under 30 minutes, 3 to 8 events on average, and a heavy person has one in every slot.
fn session_keys() -> Vec<u64> {
    let scale = 0.925 * N as f64 / ((PEOPLE - 5) as f64).powf(0.25);
    (0..N as u64).into_par_iter().map(|i| {
        let mut s = i.wrapping_mul(0x9E3779B97F4A7C15) | 1;
        rng(&mut s);
        let u = (rng(&mut s) >> 11) as f64 / (1u64 << 53) as f64;
        let (person, events) = if u < 0.075 { ((u / 0.015) as u64, 0.015 * N as f64) } else {
            let v = (rng(&mut s) >> 11) as f64 / (1u64 << 53) as f64;
            let x = (v.powi(4) * (PEOPLE - 5) as f64) as u64;
            // The expected rows of person x under the v^4 draw.
            (5 + x, scale * (((x + 1) as f64).powf(0.25) - (x as f64).powf(0.25)))
        };
        let mut p = (person + 1).wrapping_mul(0xD6E8FEB86659FD93) | 1;
        rng(&mut p);
        let sessions = ((events / (3 + rng(&mut p) % 6) as f64) as u64).clamp(1, 168);
        let session = rng(&mut s) % sessions;
        // 5 is coprime to 168, so a person's sessions get distinct slots.
        let slot = (rng(&mut p) % 168 + session * 5) % 168;
        let mut q = (person * 168 + session + 1).wrapping_mul(0x9E3779B97F4A7C15) | 1;
        rng(&mut q);
        let start = slot * 7_200_000 + rng(&mut q) % 1_500 * 1_000;
        let seconds = ((events / sessions as f64 - 1.0).max(0.0) * 153.0).min(1_799.0) as u64;
        person << 32 | (start + rng(&mut s) % (seconds + 1) * 1_000)
    }).collect()
}

const GAP: u64 = 1_800_000;

// Sessions in a sorted bucket: a new person, or a gap over 30 minutes, starts one.
fn sessions_sorted(v: &[u64], shift: u32) -> u64 {
    let mut prev = None;
    v.iter().filter(|&&k| {
        let new = prev.is_none_or(|p| k >> shift != p >> shift || k - p > GAP);
        prev = Some(k);
        new
    }).count() as u64
}

#[derive(Clone, Copy)]
struct Bin { key: u64, min: u32, max: u32, rows: u32 }
const EMPTY: Bin = Bin { key: u64::MAX, min: u32::MAX, max: 0, rows: 0 };

thread_local! { static BINS: std::cell::RefCell<Vec<Bin>> = const { std::cell::RefCell::new(Vec::new()) }; }

// The same count with no sort of the rows, which may come in several pieces in any order.
// (person, 30-minute bin) -> min ts, max ts, rows: rows in one bin are under 30 minutes apart, so
// they share a session, and a session breaks between two occupied bins of a person exactly when
// min(next) - max(previous) is over the gap. Only the bins are sorted.
// None when the rows are too spread out for this to pay; the caller sorts instead.
fn sessions_binned(pieces: &[&[u64]], shift: u32) -> Option<u64> {
    let len: usize = pieces.iter().map(|p| p.len()).sum();
    let cap = (len / 2).next_power_of_two().max(1024);
    let down = 64 - cap.trailing_zeros();
    BINS.with_borrow_mut(|table| {
        table.clear();
        table.resize(cap, EMPTY);
        let mut used = 0;
        for &k in pieces.iter().flat_map(|p| p.iter()) {
            let ts = (k & ((1u64 << shift) - 1)) as u32;
            // shortcut: person and bin share a u64, fine while a person is under 53 bits; a packed 63-bit ID needs a wider key.
            let key = k >> shift << 11 | (ts / GAP as u32) as u64;
            let mut at = (key.wrapping_mul(0x9E3779B97F4A7C15) >> down) as usize;
            loop {
                let e = &mut table[at];
                // One branch for "mine" and "empty", so a bin's first row costs no misprediction.
                if (e.key == key) | (e.key == u64::MAX) {
                    used += (e.key == u64::MAX) as usize;
                    *e = Bin { key, min: e.min.min(ts), max: e.max.max(ts), rows: e.rows + 1 };
                    break;
                }
                at = (at + 1) & (cap - 1);
            }
            if used > cap / 4 * 3 { return None; }
        }
        let mut bins: Vec<Bin> = table.iter().filter(|e| e.key != u64::MAX).copied().collect();
        bins.sort_unstable_by_key(|b| b.key);
        assert_eq!(bins.iter().map(|b| b.rows as usize).sum::<usize>(), len);
        let mut prev: Option<Bin> = None;
        Some(bins.iter().filter(|&&b| {
            let new = prev.is_none_or(|p| b.key >> 11 != p.key >> 11 || (b.min - p.max) as u64 > GAP);
            prev = Some(b);
            new
        }).count() as u64)
    })
}

fn bucket(key: u64) -> usize {
    ((key >> 32).wrapping_mul(0x9E3779B97F4A7C15) >> (64 - BITS)) as usize
}

// Rewrites keys to (bucket-local person index << 31 | ts) and returns the person count.
fn intern(v: &mut [u64]) -> u32 {
    let cap = (v.len() / 4).max(4096).next_power_of_two();
    let mut table = vec![(u32::MAX, 0u32); cap];
    let mut count = 0u32;
    for k in v.iter_mut() {
        let person = (*k >> 32) as u32;
        let mut at = (person as u64).wrapping_mul(0xD6E8FEB86659FD93) as usize >> 20 & (cap - 1);
        let local = loop {
            if table[at].0 == person { break table[at].1; }
            if table[at].0 == u32::MAX { table[at] = (person, count); count += 1; break count - 1; }
            at = (at + 1) & (cap - 1);
        };
        assert!((count as usize) < cap / 2, "benchmark table too small");
        *k = (local as u64) << 31 | (*k & 0x7FFF_FFFF);
    }
    count
}

fn radix(v: &mut Vec<u64>, key_bits: u32, digit: u32) {
    let passes = key_bits.div_ceil(digit) as usize;
    let size = 1usize << digit;
    let mut hist = vec![0u32; passes * size];
    for &k in v.iter() {
        for p in 0..passes { hist[p * size + ((k >> (p as u32 * digit)) as usize & (size - 1))] += 1; }
    }
    let mut tmp = vec![0u64; v.len()];
    for p in 0..passes {
        let h = &mut hist[p * size..(p + 1) * size];
        let mut sum = 0u32;
        for c in h.iter_mut() { let n = *c; *c = sum; sum += n; }
        for &k in v.iter() {
            let d = (k >> (p as u32 * digit)) as usize & (size - 1);
            tmp[h[d] as usize] = k;
            h[d] += 1;
        }
        std::mem::swap(v, &mut tmp);
    }
}

fn scatter(v: &[u64]) -> Vec<Vec<Vec<u64>>> {
    let p = 1usize << BITS;
    v.par_chunks(v.len().div_ceil(rayon::current_num_threads())).map(|chunk| {
        let mut b: Vec<Vec<u64>> = (0..p).map(|_| Vec::with_capacity(chunk.len() / p * 5 / 4)).collect();
        for &k in chunk { b[bucket(k)].push(k); }
        b
    }).collect()
}

// Sessionize straight from the scattered pieces: no gather, no intern, no sort of the rows.
// Returns the time after the scatter, the sessions, and the buckets that fell back to the sort.
fn run_binned(v: &[u64]) -> (f64, u64, usize) {
    let local = scatter(v);
    let t = Instant::now();
    let (sessions, fell_back) = (0..1usize << BITS).into_par_iter().map(|i| {
        let pieces: Vec<&[u64]> = local.iter().map(|t| &t[i][..]).collect();
        match sessions_binned(&pieces, 32) {
            Some(n) => (n, 0),
            None => { let mut all = pieces.concat(); all.sort_unstable(); (sessions_sorted(&all, 32), 1) }
        }
    }).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    (t.elapsed().as_secs_f64(), sessions, fell_back)
}

fn run(v: &[u64], check: bool, leaf: impl Fn(&mut Vec<u64>, u32) + Sync) -> (f64, f64) {
    let p = 1usize << BITS;
    let t = Instant::now();
    let local = scatter(v);
    let scatter = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let out: Vec<Vec<u64>> = (0..p).into_par_iter().map(|i| {
        let mut out = Vec::with_capacity(local.iter().map(|t| t[i].len()).sum());
        for t in &local { out.extend_from_slice(&t[i]); }
        let people = intern(&mut out);
        leaf(&mut out, people);
        out
    }).collect();
    let leaves = t.elapsed().as_secs_f64();
    if check { assert!(out.iter().all(|x| x.is_sorted())); }
    (scatter, leaves)
}

fn main() {
    // The gap rule at its edge: exactly 30 minutes joins, 1 ms more splits, also across a bin edge.
    for (edge, want) in [(vec![0, GAP], 1), (vec![0, GAP + 1], 2), (vec![GAP - 1, GAP, 2 * GAP + 1], 2), (vec![5, 5, 1 << 31 | 5], 2)] {
        assert_eq!((sessions_sorted(&edge, 31), sessions_binned(&[&edge], 31)), (want, Some(want)));
    }
    // SESSIONS=1 gives timestamps the generator's session shape; the default is uniform over the window.
    let shaped = std::env::var_os("SESSIONS").is_some();
    let base = if shaped { session_keys() } else { keys() };
    println!("threads {} timestamps {}", rayon::current_num_threads(), if shaped { "session-shaped" } else { "uniform" });
    let count = AtomicU64::new(0);
    for round in 0..3 {
        let mut all = base.clone();
        let t = Instant::now();
        all.par_sort_unstable();
        let global = t.elapsed().as_secs_f64();
        drop(all);
        let (scatter, none) = run(&base, false, |_, _| {});
        let (_, cmp) = run(&base, true, |v, _| v.sort_unstable());
        let bits = |people: u32| 31 + (32 - people.leading_zeros());
        let mut line = format!("round {round}: global par_sort_unstable {global:.3}s | scatter {scatter:.3}s | gather+intern {none:.3}s | +sort_unstable {:.3}s", cmp - none);
        for digit in [8u32, 11, 12] {
            let (_, r) = run(&base, true, |v, people| {
                // Whale buckets are far out of cache; keep the comparison sort there.
                if v.len() > 1 << 20 { v.sort_unstable() } else { radix(v, bits(people), digit) }
            });
            line += &format!(" | +radix {digit}-bit {:.3}s", r - none);
        }
        // Sessionize end to end after the scatter: gather, intern, sort and scan, against binning the pieces.
        let (_, sorted) = run(&base, false, |v, _| { v.sort_unstable(); count.fetch_add(sessions_sorted(v, 31), Relaxed); });
        let (binned, sessions, fell_back) = run_binned(&base);
        assert_eq!(sessions, count.swap(0, Relaxed));
        line += &format!(" || after the scatter, {sessions} sessions: gather+intern+sort_unstable+scan {sorted:.3}s | bins from the pieces {binned:.3}s ({fell_back} buckets fell back to the sort)");
        println!("{line}");
    }
}
