//! Two benchmarks, best of 5 each:
//! 1. `links::build` on one thread over the real pairs in `full/links/*.csv` (or the folder given).
//! 2. `links::scatter_events` at full size, sized like `prototypes/lookupbench`: 62M rows against
//!    3.4M linked IDs, 1,024 partitions of rows on `NCPU` workers (default 8) into 1,024 person buckets each, timed
//!    against the same scatter without the lookup.
//!
//! `cargo run --release -p links --example linksbench [links folder]`

use std::collections::HashSet;
use std::time::Instant;

use types::hash::{C1, C2, person_hash};
use types::id::{Id, pack};
use types::par;
use types::record::{EventRec, PersonEvent, VALID};

const ROWS: usize = 62_000_000;
const PEOPLE: u64 = 2_000_000;
const PART_BITS: u32 = 10;
const BUCKET_BITS: u32 = 10;

fn main() {
    let workers = types::env::number("NCPU", 8);
    let dir = std::env::args().nth(1).unwrap_or_else(|| "full/links".into());
    union_find(&dir);
    lookup(workers);
}

/// The throwaway splitter: the first line of a file is the header; a line needs exactly one
/// comma and two non-empty sides, each stripped of surrounding quotes.
fn read_pairs(dir: &str) -> Vec<[Id; 2]> {
    let mut pairs = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("links folder").map(|f| f.unwrap().path()).collect();
    files.retain(|path| path.extension().is_some_and(|ext| ext == "csv"));
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        for line in bytes.split(|&b| b == b'\n').skip(1) {
            let mut fields = line.split(|&b| b == b',');
            let (Some(a), Some(b), None) = (fields.next(), fields.next(), fields.next()) else { continue };
            let strip = |f: &[u8]| -> Vec<u8> {
                if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' { f[1..f.len() - 1].to_vec() } else { f.to_vec() }
            };
            let (a, b) = (strip(a), strip(b));
            if !a.is_empty() && !b.is_empty() {
                pairs.push([pack(&a), pack(&b)]);
            }
        }
    }
    pairs
}

fn union_find(dir: &str) {
    let pairs = read_pairs(dir);
    let distinct = pairs.iter().flatten().collect::<HashSet<_>>().len();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let table = links::build(&pairs);
        best = best.min(started.elapsed().as_secs_f64());
        std::hint::black_box(&table);
    }
    println!("build: {best:.3} s, one thread, {} pairs, {distinct} distinct IDs", pairs.len());
}

fn mix(x: u64) -> u64 {
    let x = (x ^ (x >> 32)).wrapping_mul(C1);
    x ^ (x >> 29)
}

/// The prototype's alias mix: 45% one ID, 30% two, 13% three, the rest four to eight.
fn aliases(person: u64) -> u64 {
    match mix(person ^ 0x1234) % 100 {
        0..45 => 1,
        45..75 => 2,
        75..88 => 3,
        _ => 4 + mix(person ^ 0x77) % 5,
    }
}

/// A generated-looking ID: `u` and base36 of a 40-bit value.
fn uid(person: u64, alias: u64) -> Id {
    let mut value = mix(person * 8 + alias + 1) & ((1 << 40) - 1);
    let mut digits = vec![b'u'];
    loop {
        digits.insert(1, b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break pack(&digits);
        }
    }
}

fn lookup(workers: usize) {
    // Each alias linked to a random earlier one, as the generator does, in a shuffled order.
    let mut pairs: Vec<[Id; 2]> = Vec::new();
    for person in 0..PEOPLE {
        for alias in 1..aliases(person) {
            pairs.push([uid(person, alias), uid(person, mix(person ^ alias) % alias)]);
        }
    }
    for at in (1..pairs.len()).rev() {
        pairs.swap(at, (mix(at as u64 ^ 0x5EED) % (at as u64 + 1)) as usize);
    }
    let linked = pairs.iter().flatten().collect::<HashSet<_>>().len();
    let table = links::build(&pairs);

    // The prototype's rows: 5 whales with 1.5% each, the rest Zipf-like; half on the first alias.
    let mut rows: Vec<(Vec<EventRec>, usize)> = vec![Default::default(); workers];
    let chunk = ROWS.div_ceil(workers);
    par::each(&mut rows, |worker, (mine, hits)| {
        *mine = ((worker * chunk) as u64..((worker + 1) * chunk).min(ROWS) as u64)
            .map(|i| {
                let r = mix(i.wrapping_mul(C2) ^ 0xABCD);
                let u = (r >> 11) as f64 / (1u64 << 53) as f64;
                let person = if u < 0.075 {
                    (u / 0.015) as u64
                } else {
                    5 + (((mix(r) >> 11) as f64 / (1u64 << 53) as f64).powi(4) * (PEOPLE - 5) as f64) as u64
                };
                let alias = if mix(r ^ 1).is_multiple_of(2) { 0 } else { mix(r ^ 2) % aliases(person) };
                *hits += usize::from(aliases(person) > 1);
                EventRec { id: i + 1, user: uid(person, alias), ts: r as u32 >> 2, ingest: 0, pay: i as u32, flags: VALID }
            })
            .collect();
    });
    let hits: usize = rows.iter().map(|w| w.1).sum();
    let rows: Vec<EventRec> = rows.into_iter().flat_map(|w| w.0).collect();
    println!(
        "lookup: {} rows, {linked} linked IDs ({} pairs), {:.0}% of rows hit the table",
        rows.len(),
        pairs.len(),
        100.0 * hits as f64 / rows.len() as f64
    );

    let parts = 1usize << PART_BITS;
    let per_part = rows.len().div_ceil(parts);
    let room = rows.len() / workers / (1 << BUCKET_BITS) * 9 / 8 + 64;
    let run = |with_lookup: bool| {
        let mut out: Vec<Vec<Vec<PersonEvent>>> =
            (0..workers).map(|_| (0..1 << BUCKET_BITS).map(|_| Vec::with_capacity(room)).collect()).collect();
        let started = Instant::now();
        par::run(&mut out, parts, |_, buckets, part| {
            let kept = &rows[(part * per_part).min(rows.len())..((part + 1) * per_part).min(rows.len())];
            if with_lookup {
                links::scatter_events(&table, kept, BUCKET_BITS, buckets);
            } else {
                bare_scatter(kept, buckets);
            }
        });
        let secs = started.elapsed().as_secs_f64();
        let check: u64 = out.iter().flatten().flatten().fold(0, |sum, e| sum.wrapping_add(e.person));
        (secs, check)
    };
    let (mut bare, mut looked) = (f64::MAX, f64::MAX);
    for round in 0..5 {
        let (b, _) = run(false);
        let (l, check) = run(true);
        println!("round {round}: bare scatter {b:.3} s, with lookup {l:.3} s, lookup adds {:.3} s (check {check:x})", l - b);
        bare = bare.min(b);
        looked = looked.min(l);
    }
    println!("best of 5: bare scatter {bare:.3} s, with lookup {looked:.3} s, lookup adds {:.3} s", looked - bare);
}

/// The scatter `scatter_events` does, with the user as its own person.
fn bare_scatter(kept: &[EventRec], out: &mut [Vec<PersonEvent>]) {
    for r in kept {
        if r.flags & VALID != 0 {
            let person = r.user;
            out[(person_hash(person) >> (64 - BUCKET_BITS)) as usize].push(PersonEvent { person, ts: r.ts, pay: r.pay });
        }
    }
}
