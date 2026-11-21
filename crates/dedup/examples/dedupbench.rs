//! `dedup::partition` at full size, sized like `prototypes/dedupbench`: 62M 32-byte records, every
//! 32nd another copy of a random row's event, scattered by `NCPU` workers (default 8) into 1,024 partitions, then
//! deduped on `NCPU` workers (default 8). Best of 5.
//!
//! `cargo run --release -p dedup --example dedupbench`

use std::time::Instant;

use types::hash::hash;
use types::par;
use types::record::{EventRec, VALID};

const N: usize = 62_000_000;
const PART_BITS: u32 = 10;

fn mix(x: u64) -> u64 {
    let x = (x ^ (x >> 32)).wrapping_mul(types::hash::C1);
    x ^ (x >> 29)
}

fn main() {
    let workers = types::env::number("NCPU", 8);
    let parts = 1usize << PART_BITS;
    let chunk = N.div_ceil(workers);
    let mut parsed: Vec<Vec<Vec<EventRec>>> = vec![Vec::new(); workers];
    par::each(&mut parsed, |worker, mine| {
        *mine = (0..parts).map(|_| Vec::with_capacity(chunk / parts * 9 / 8 + 64)).collect();
        for j in (worker * chunk) as u64..((worker + 1) * chunk).min(N) as u64 {
            let r = mix(j.wrapping_mul(types::hash::C2) ^ 0xABCD);
            let copy_of = if j % 32 == 31 { (r % N as u64) - u64::from(r % N as u64 % 32 == 31) } else { j };
            let ingest = (r >> 40) as u32 & 0xF_FFFF;
            let rec = EventRec { id: mix(copy_of + 1), user: r, ts: r as u32 >> 2, ingest, pay: j as u32, flags: VALID };
            mine[(hash(rec.id) >> (64 - PART_BITS)) as usize].push(rec);
        }
    });
    let survivors = N - (N + 1) / 32;

    let mut best = f64::MAX;
    for round in 0..5 {
        let mut states: Vec<(dedup::Scratch, usize, u64)> = (0..workers).map(|_| Default::default()).collect();
        let started = Instant::now();
        par::run(&mut states, parts, |_, (scratch, kept, sum), part| {
            let pieces = parsed.iter().map(|mine| &mine[part][..]);
            let survivors = dedup::partition(pieces, PART_BITS, scratch);
            *kept += survivors.len();
            *sum += survivors.iter().map(|r| u64::from(r.ingest)).sum::<u64>();
        });
        let secs = started.elapsed().as_secs_f64();
        best = best.min(secs);
        let kept: usize = states.iter().map(|s| s.1).sum();
        let sum: u64 = states.iter().map(|s| s.2).sum();
        assert_eq!(kept, survivors);
        println!("round {round}: dedup {secs:.3} s, {kept} survivors, ingest sum {sum}");
    }
    println!("best of 5: {best:.3} s ({N} records, {parts} partitions, {workers} workers)");
}
