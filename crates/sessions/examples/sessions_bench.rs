//! The sessions stage end to end on synthetic buckets. Events: `prototypes/sortbench` with
//! `SESSIONS=1` (60M rows, 2M people, 5 whales with 1.5% each, a session in its own two-hour
//! slot), 3% of them purchases. Clicks: 15M by the generator's rules (`generator/src/user.rs`,
//! `plan.rs`): 65% of people click, each click aimed at one of the 45% of their sessions that
//! are targets, 0 to 6 days before it on a log scale, some on the exact edges and 25% out of
//! reach; each whale gets 1%; 1% come in same-millisecond pairs; 4 campaigns take 30% of 2,000.
//! Scattered into 1,024 buckets as 8 pieces each, then the stage on 8 threads, largest bucket
//! first, best of 5; once with the clicks and once without.
//!
//! `CARGO_TARGET_DIR=target/agent-sessions cargo run --release -p sessions --example sessions_bench`

use std::time::Instant;

use types::hash::person_hash;
use types::record::{ClickRec, PAY_CATEGORY_SHIFT, PersonEvent};
use types::time::{DAY, SECOND, WEEK};
use types::{Ctx, par};

const N: usize = 60_000_000;
const PEOPLE: u64 = 2_000_000;
const WHALES: u64 = 5;
const CLICKS: usize = 15_000_000;
const CAMPAIGNS: u64 = 2_000;
const WORKERS: usize = 8;

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn unit(s: &mut u64) -> f64 {
    (rng(s) >> 11) as f64 / (1u64 << 53) as f64
}

fn seeded(value: u64) -> u64 {
    let mut s = value.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    rng(&mut s);
    s
}

fn scale() -> f64 {
    0.925 * N as f64 / ((PEOPLE - WHALES) as f64).powf(0.25)
}

/// The rows a person expects under sortbench's draw.
fn expected(person: u64) -> f64 {
    if person < WHALES {
        return 0.015 * N as f64;
    }
    let x = (person - WHALES) as f64;
    scale() * ((x + 1.0).powf(0.25) - x.powf(0.25))
}

/// sortbench's session shape: the person's session count and first slot.
fn shape(person: u64) -> (u64, u64) {
    let mut p = (person + 1).wrapping_mul(0xD6E8FEB86659FD93) | 1;
    rng(&mut p);
    let sessions = ((expected(person) / (3 + rng(&mut p) % 6) as f64) as u64).clamp(1, 168);
    (sessions, rng(&mut p) % 168)
}

/// A session's start, ms after the window start; 5 is coprime to 168, so the slots differ.
fn session_start(person: u64, first_slot: u64, session: u64) -> u64 {
    let slot = (first_slot + session * 5) % 168;
    let mut q = seeded(person * 168 + session + 1);
    slot * 7_200_000 + rng(&mut q) % 1_500 * 1_000
}

/// Draws an event as sortbench does: its person and its ts after the window start.
fn event(i: u64) -> (u64, u64) {
    let mut s = seeded(i);
    let u = unit(&mut s);
    let person = if u < 0.075 { (u / 0.015) as u64 } else { WHALES + (unit(&mut s).powi(4) * (PEOPLE - WHALES) as f64) as u64 };
    let (sessions, first_slot) = shape(person);
    let session = rng(&mut s) % sessions;
    let seconds = ((expected(person) / sessions as f64 - 1.0).max(0.0) * 153.0).min(1_799.0) as u64;
    (person, session_start(person, first_slot, session) + rng(&mut s) % (seconds + 1) * 1_000)
}

struct Pieces {
    events: Vec<Vec<PersonEvent>>,
    clicks: Vec<Vec<ClickRec>>,
}

fn main() {
    let start = 1_790_812_800_000; // 2026-10-01T00:00:00Z
    let names: Vec<Vec<u8>> = (0..20).map(|n| format!("cat{n:02}").into_bytes()).collect();
    let products: Vec<(u64, &[u8])> = names.iter().enumerate().map(|(n, name)| (n as u64 + 1, name.as_slice())).collect();
    let campaign_names: Vec<(Vec<u8>, Vec<u8>)> =
        (0..CAMPAIGNS).map(|n| (format!("c_{n}").into_bytes(), format!("ch{}", n % 5).into_bytes())).collect();
    let campaigns: Vec<(&[u8], &[u8])> = campaign_names.iter().map(|(c, h)| (c.as_slice(), h.as_slice())).collect();
    let ctx = Ctx::new(start, start + 14 * DAY, &products, &campaigns).unwrap();
    let buckets = 1usize << ctx.bucket_bits;
    let bucket_of = |person: u64| (person_hash(person) >> (64 - ctx.bucket_bits)) as usize;
    let lo = i64::from(ctx.event_lo);

    let t = Instant::now();
    // Click targets: (person, session start), whales' apart.
    let (mut targets, mut whale_targets) = (Vec::new(), vec![Vec::new(); WHALES as usize]);
    for person in 0..PEOPLE {
        let mut h = seeded(person + 7);
        if person >= WHALES && rng(&mut h) % 100 >= 65 {
            continue;
        }
        let (sessions, first_slot) = shape(person);
        let before = targets.len();
        for session in 0..sessions {
            if rng(&mut h) % 100 < 45 || (session + 1 == sessions && targets.len() == before) {
                let target = (person, session_start(person, first_slot, session) as i64);
                if person < WHALES { whale_targets[person as usize].push(target) } else { targets.push(target) }
            }
        }
    }
    let mut pieces: Vec<Pieces> = (0..WORKERS)
        .map(|_| Pieces { events: vec![Vec::new(); buckets], clicks: vec![Vec::new(); buckets] })
        .collect();
    par::each(&mut pieces, |worker, mine| {
        for i in (worker * N / WORKERS) as u64..((worker + 1) * N / WORKERS) as u64 {
            let (person, ts) = event(i);
            let mut s = seeded(i + (1 << 40));
            let pay = if rng(&mut s) % 100 < 3 {
                (100 + rng(&mut s) % 49_901) as u32 | ((rng(&mut s) % 21) as u32) << PAY_CATEGORY_SHIFT
            } else {
                0
            };
            mine.events[bucket_of(person + 1)].push(PersonEvent { person: person + 1, ts: (lo + ts as i64) as u32, pay });
        }
        let mut pair = None;
        for j in (worker * CLICKS / WORKERS) as u64..((worker + 1) * CLICKS / WORKERS) as u64 {
            let mut s = seeded(j + (1 << 41));
            let (person, ts) = match pair.take() {
                Some(same) => same,
                None => {
                    let pick = rng(&mut s) % 100;
                    let from = if pick < WHALES { &whale_targets[pick as usize] } else { &targets };
                    let (person, session) = from[(rng(&mut s) % from.len() as u64) as usize];
                    let delta = match rng(&mut s) % 1_000 {
                        0..4 => WEEK,
                        4..8 => WEEK + 1,
                        8..12 => 0,
                        12..16 => -1,
                        16..750 => {
                            let low = 1i64 << (rng(&mut s) % 19);
                            ((low + (rng(&mut s) % low as u64) as i64) * SECOND).min(WEEK)
                        }
                        _ => WEEK + (1 + rng(&mut s) % (3 * DAY / SECOND) as u64) as i64 * SECOND,
                    };
                    let mut ts = lo + session - delta;
                    if ts < 0 {
                        ts = lo + session - (rng(&mut s) % ((lo + session) / SECOND + 1) as u64) as i64 * SECOND;
                    }
                    if rng(&mut s).is_multiple_of(100) {
                        pair = Some((person, ts));
                    }
                    (person, ts)
                }
            };
            let campaign = if rng(&mut s) % 10 < 3 { rng(&mut s) % 4 } else { rng(&mut s) % CAMPAIGNS };
            let click = ClickRec { user: person + 1, click: rng(&mut s) >> 2 | 1, ts: ts as u32, campaign: campaign as u32 };
            mine.clicks[bucket_of(person + 1)].push(click);
        }
    });
    let mut order: Vec<usize> = (0..buckets).collect();
    order.sort_by_cached_key(|&b| std::cmp::Reverse(pieces.iter().map(|p| p.events[b].len()).sum::<usize>()));
    let largest: usize = pieces.iter().map(|p| p.events[order[0]].len()).sum();
    println!("generated in {:.1} s; largest bucket {largest} events", t.elapsed().as_secs_f64());

    for with_clicks in [true, false] {
        let mut best = f64::MAX;
        let mut summary = String::new();
        for _ in 0..5 {
            let mut aggs: Vec<sessions::Agg> = (0..WORKERS).map(|_| sessions::Agg::new(&ctx)).collect();
            let t = Instant::now();
            par::run(&mut aggs, buckets, |_, agg, task| {
                let bucket = order[task];
                let events = pieces.iter().map(|p| &p.events[bucket][..]);
                let clicks = pieces.iter().map(|p| if with_clicks { &p.clicks[bucket][..] } else { &[][..] });
                sessions::bucket(&ctx, events, clicks, agg);
            });
            let campaign_rows = sessions::campaign_daily(&ctx, &aggs);
            let secs = t.elapsed().as_secs_f64();
            best = best.min(secs);
            let user_rows: usize = aggs.iter().map(|agg| agg.user_daily.person.len() + agg.user_narrow.len()).sum();
            let sessions: i64 = campaign_rows.sessions.iter().sum();
            let organic: i64 = campaign_rows
                .campaign
                .iter()
                .zip(&campaign_rows.sessions)
                .filter(|&(&c, _)| c == ctx.organic())
                .map(|(_, &s)| s)
                .sum();
            let assisted: i64 = campaign_rows.assisted_sessions.iter().sum();
            summary = format!(
                "{sessions} sessions, {user_rows} user_daily rows, {:.1}% credited to a campaign, {:.2} assists per session",
                100.0 * (sessions - organic) as f64 / sessions as f64,
                assisted as f64 / sessions as f64
            );
            print!("{secs:.3} ");
        }
        println!("\n{}: best {best:.3} s ({summary})", if with_clicks { "events + clicks" } else { "events only" });
    }
}
