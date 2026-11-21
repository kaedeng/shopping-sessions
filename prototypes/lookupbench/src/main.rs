// Throwaway: what does the user -> person lookup cost per row, and how wide can scattered records be?
use rayon::prelude::*;
use std::time::Instant;

const ROWS: usize = 62_000_000;
const PEOPLE: u64 = 2_000_000;
const C1: u64 = 0x9E37_79B9_7F4A_7C15;
const C2: u64 = 0xD6E8_FEB8_6659_FD93;
const TABLE_BITS: u32 = 23;

fn mix(x: u64) -> u64 { let x = (x ^ x >> 32).wrapping_mul(C1); x ^ x >> 29 }

// Generator's alias mix: 45% one ID, 30% two, 13% three, the rest four to eight.
fn aliases(p: u64) -> u64 {
    match mix(p ^ 0x1234) % 100 { 0..45 => 1, 45..75 => 2, 75..88 => 3, _ => 4 + mix(p ^ 0x77) % 5 }
}
fn uid(p: u64, a: u64) -> u64 { mix(p * 8 + a + 1) }

#[derive(Clone, Copy)]
struct Row { user: u64, ts: u32, pay: u32 }

fn rows() -> Vec<Row> {
    (0..ROWS as u64).into_par_iter().map(|i| {
        let r = mix(i.wrapping_mul(C2) ^ 0xABCD);
        let u = (r >> 11) as f64 / (1u64 << 53) as f64;
        let person = if u < 0.075 { (u / 0.015) as u64 } else {
            let v = (mix(r) >> 11) as f64 / (1u64 << 53) as f64;
            5 + (v.powi(4) * (PEOPLE - 5) as f64) as u64
        };
        let n = aliases(person);
        let alias = if mix(r ^ 1) % 2 == 0 { 0 } else { mix(r ^ 2) % n };
        Row { user: uid(person, alias), ts: r as u32 >> 2, pay: i as u32 }
    }).collect()
}

fn slot(key: u64) -> usize { (key.wrapping_mul(C2) >> (64 - TABLE_BITS)) as usize }

fn link_table() -> (Vec<(u64, u64)>, usize) {
    let mut table = vec![(0u64, 0u64); 1 << TABLE_BITS];
    let mut linked = 0;
    for p in 0..PEOPLE {
        let n = aliases(p);
        if n < 2 { continue; }
        let root = (0..n).map(|a| uid(p, a)).min().unwrap();
        for a in 0..n {
            let mut at = slot(uid(p, a));
            while table[at].0 != 0 { at = (at + 1) & ((1 << TABLE_BITS) - 1); }
            table[at] = (uid(p, a), root);
            linked += 1;
        }
    }
    (table, linked)
}

#[inline(always)]
fn prefetch<T>(p: &T) {
    #[cfg(target_arch = "x86_64")]
    unsafe { core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p as *const T as *const i8) }
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p as *const T, options(nostack, preserves_flags, readonly)) }
}

// mode 0: no lookup. 1: lookup, plain loop. 2: lookup with a prefetch one block ahead.
fn resolve_and_scatter(rows: &[Row], table: &[(u64, u64)], mode: u8, block: usize) -> (f64, u64) {
    const P_BITS: u32 = 10;
    let mask = table.len() - 1;
    let t = Instant::now();
    let out: Vec<(Vec<Vec<Row>>, u64)> = rows.par_chunks(rows.len().div_ceil(rayon::current_num_threads())).map(|chunk| {
        let mut b: Vec<Vec<Row>> = (0..1 << P_BITS).map(|_| Vec::with_capacity((chunk.len() >> P_BITS) * 9 / 8 + 64)).collect();
        let mut sum = 0u64;
        for batch in chunk.chunks(block) {
            if mode == 2 { for r in batch { prefetch(&table[slot(r.user)]); } }
            for r in batch {
                let mut person = r.user;
                if mode != 0 {
                    let mut at = slot(r.user);
                    loop {
                        let (k, root) = table[at];
                        if k == r.user { person = root; break; }
                        if k == 0 { break; }
                        at = (at + 1) & mask;
                    }
                }
                sum = sum.wrapping_add(person);
                b[(person.wrapping_mul(C1) >> (64 - P_BITS)) as usize].push(Row { user: person, ..*r });
            }
        }
        (b, sum)
    }).collect();
    let secs = t.elapsed().as_secs_f64();
    (secs, out.iter().fold(0, |a, x| a.wrapping_add(x.1)))
}

fn scatter_width<const W: usize>(n: usize) -> f64 {
    let src: Vec<[u64; W]> = (0..n as u64).into_par_iter().map(|i| [mix(i + 1); W]).collect();
    let t = Instant::now();
    let out: Vec<Vec<Vec<[u64; W]>>> = src.par_chunks(n.div_ceil(rayon::current_num_threads())).map(|chunk| {
        let mut b: Vec<Vec<[u64; W]>> = (0..1024).map(|_| Vec::with_capacity(chunk.len() / 1024 * 9 / 8 + 64)).collect();
        for r in chunk { b[(r[0].wrapping_mul(C2) >> 54) as usize].push(*r); }
        b
    }).collect();
    let secs = t.elapsed().as_secs_f64();
    drop(out);
    secs
}

fn main() {
    let rows = rows();
    let (table, linked) = link_table();
    let hits = rows.par_iter().filter(|r| { let mut at = slot(r.user); loop { let k = table[at].0; if k == r.user { break true } if k == 0 { break false } at = (at + 1) & (table.len() - 1); } }).count();
    println!("threads {} rows {} linked ids {} table {} MB, rows that hit {:.0}%", rayon::current_num_threads(), rows.len(), linked, table.len() * 16 >> 20, 100.0 * hits as f64 / rows.len() as f64);
    for round in 0..3 {
        let (base, _) = resolve_and_scatter(&rows, &table, 0, 16);
        let (plain, s1) = resolve_and_scatter(&rows, &table, 1, 16);
        let (pre16, s2) = resolve_and_scatter(&rows, &table, 2, 16);
        let (pre64, s3) = resolve_and_scatter(&rows, &table, 2, 64);
        assert!(s1 == s2 && s2 == s3);
        println!("round {round}: scatter only {base:.3}s | +lookup {:.3}s | +lookup, prefetch 16 ahead {:.3}s | +lookup, prefetch 64 ahead {:.3}s || scatter 16B {:.3}s 24B {:.3}s 32B {:.3}s 48B {:.3}s",
            plain - base, pre16 - base, pre64 - base, scatter_width::<2>(ROWS), scatter_width::<3>(ROWS), scatter_width::<4>(ROWS), scatter_width::<6>(ROWS));
    }
}
