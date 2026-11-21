//! The whole parse of a dataset folder as `src/main.rs` runs it, minus the union-find: map and
//! plan, then every chunk task plus the links on `NCPU` workers. Best of 5.
//!
//! `cargo run --release -p ingest --example parse_bench -- full` (`SS_TOK`, `SS_CHUNK_KB`, `NCPU` apply)

use std::path::Path;
use std::time::Instant;

use ingest::{Ingest, ParseOut};
use types::par;
use types::record::VALID;

fn main() {
    let data = std::env::args().nth(1).expect("usage: parse_bench DATA_DIR");
    let data = Path::new(&data);
    let bytes: u64 = ["events", "clicks", "links"]
        .iter()
        .flat_map(|folder| std::fs::read_dir(data.join(folder)).unwrap())
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    let ctx = ingest::load_ctx(data).unwrap();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let ingest = Ingest::open(data, &ctx).unwrap();
        let opened = started.elapsed().as_secs_f64();
        let mut outs: Vec<ParseOut> = (0..ctx.workers).map(|_| ingest.new_out(&ctx)).collect();
        let allocated = started.elapsed().as_secs_f64();
        let links = std::sync::OnceLock::new();
        par::run(&mut outs, ingest.tasks() + 1, |_, out, task| match task {
            0 => drop(links.set(ingest.links().len())),
            _ => ingest.parse(&ctx, task - 1, out),
        });
        let seconds = started.elapsed().as_secs_f64();
        best = best.min(seconds);
        let events = outs.iter().flat_map(|out| &out.parts).flatten();
        let (rows, valid) = events.fold((0, 0), |(rows, valid), e| (rows + 1, valid + (e.flags & VALID) as usize));
        let clicks: usize = outs.iter().map(|out| out.clicks.len()).sum();
        eprintln!(
            "{seconds:.3} s ({:.1} ms to open, {:.1} ms to allocate): {} tasks, {rows} event rows ({valid} valid), {clicks} valid clicks, {} links",
            opened * 1e3,
            (allocated - opened) * 1e3,
            ingest.tasks(),
            links.get().unwrap()
        );
    }
    let gb = bytes as f64 / 1e9;
    println!(
        "best {best:.3} s for {gb:.2} GB on {} workers: {:.2} GB/s, {:.2} GB/s per worker",
        ctx.workers,
        gb / best,
        gb / best / ctx.workers as f64
    );
}
