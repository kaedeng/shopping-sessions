//! `user_daily` at full size, shaped like `prototypes/pqbench`: 7M rows of about 2M distinct
//! 9-byte IDs, 10% null categories out of the generator's 20 and `unknown`, split over 8 workers
//! and written on 8 threads. Best of 5, and the bytes written.
//! `cargo run --release -p output --example output_bench -- FOLDER`
//! With `NULL=1` every part file is a symlink to /dev/null: the encode alone, without the kernel write.

use std::path::PathBuf;
use std::time::Instant;

use types::rows::{NO_CATEGORY, UserDaily};
use types::time::DAY;
use types::{Ctx, id, par};

const ALPHA: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const CATEGORIES: [&str; 20] = [
    "apparel", "automotive", "baby", "beauty", "books", "electronics", "garden", "grocery", "home", "jewelry",
    "kitchen", "music", "office", "outdoors", "pets", "shoes", "sports", "tools", "toys", "video_games",
];
const ROWS: usize = 7_000_000;
const WORKERS: usize = 8;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("output folder"));
    let products: Vec<_> =
        CATEGORIES.iter().enumerate().map(|(p, name)| (id::pack(format!("p{p}").as_bytes()), name.as_bytes())).collect();
    let ctx = Ctx::new(20_000 * DAY, 20_014 * DAY, &products, &[]).unwrap();
    assert_eq!(ctx.categories.len(), 21);

    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let parts: Vec<UserDaily> = (0..WORKERS)
        .map(|_| {
            let mut p = UserDaily::default();
            while p.day.len() < ROWS / WORKERS {
                let mut x = next();
                let name: Vec<u8> = (0..9).map(|_| (ALPHA[(x % 36) as usize], x /= 36).0).collect();
                let person = id::pack(&name);
                let first = (next() % 15) as i32;
                for j in 0..(1 + next() % 6).min((ROWS / WORKERS - p.day.len()) as u64) as i32 {
                    let r = next();
                    p.person.push(person);
                    p.day.push(20_000 + (first + j) % 15);
                    p.sessions.push(1 + (r % 5) as i64);
                    p.events.push(1 + ((r >> 8) % 60) as i64);
                    p.revenue_cents.push(((r >> 16) % 50_000) as i64);
                    p.top_category.push(if (r >> 40) % 10 == 0 { NO_CATEGORY } else { ((r >> 44) % 21) as u8 });
                }
            }
            p
        })
        .collect();

    let mut times = vec![];
    let mut bytes = 0;
    for _ in 0..5 {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if std::env::var_os("NULL").is_some() {
            for w in 0..WORKERS {
                std::os::unix::fs::symlink("/dev/null", dir.join(format!("part-{w}.parquet"))).unwrap();
            }
        }
        let mut rows: Vec<UserDaily> = parts
            .iter()
            .map(|p| UserDaily {
                person: p.person.clone(),
                day: p.day.clone(),
                sessions: p.sessions.clone(),
                events: p.events.clone(),
                revenue_cents: p.revenue_cents.clone(),
                top_category: p.top_category.clone(),
            })
            .collect();
        let started = Instant::now();
        par::each(&mut rows, |worker, rows| output::user_daily(&dir, worker, &ctx, std::mem::take(rows)).unwrap());
        times.push(started.elapsed().as_secs_f64());
        bytes = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum::<u64>();
    }
    times.sort_by(f64::total_cmp);
    println!("user_daily {ROWS} rows, {WORKERS} files: best {:.4} s, median {:.4} s, {} bytes ({} MB)", times[0], times[2], bytes, bytes >> 20);
}
